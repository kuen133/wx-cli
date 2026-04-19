use anyhow::{Context, Result};
use chrono::{Local, TimeZone, Timelike};
use regex::Regex;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use super::cache::DbCache;
use super::voice_asr;

/// 静态编译的 Msg 表名正则，避免在热路径中重复编译
fn msg_table_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^Msg_[0-9a-f]{32}$").unwrap())
}

/// 判定会话类型。返回值固定为 `group` / `official_account` / `folded` / `private` 之一。
///
/// 判据次序：
/// 1. `@chatroom` / 折叠入口特殊 username
/// 2. `contact.verify_flag` 非 0 —— 覆盖所有被微信官方打了认证标的账号，
///    包括 username 为 `wxid_*` 但实为公众号的情况（如"人物"），
///    以及品牌服务号 `cmb4008205555`、系统号 `qqsafe` / `mphelper` 等
/// 3. username 前缀兜底（`gh_*` / `biz_*` / `@*` 等）—— 在 contact 表未加载或没记录时
///    仍能给出正确结果
pub fn chat_type_of(username: &str, names: &Names) -> &'static str {
    if username.contains("@chatroom") {
        return "group";
    }
    if username == "brandsessionholder" || username == "@placeholder_foldgroup" {
        return "folded";
    }
    if names.is_verified(username) {
        return "official_account";
    }
    if username.starts_with("gh_") || username.starts_with("biz_") {
        return "official_account";
    }
    // `@` 开头的剩余 username（如 `@opencustomerservicemsg`）是微信内部系统账号，
    // 通常不落在 contact 表里，verify_flag 兜不住，按前缀兜底。
    if username.starts_with('@') {
        return "official_account";
    }
    "private"
}

/// 联系人名称缓存
#[derive(Clone)]
pub struct Names {
    /// username -> display_name
    pub map: HashMap<String, String>,
    /// md5(username) -> username（用于从 Msg_<md5> 表名反推联系人）
    pub md5_to_uname: HashMap<String, String>,
    /// 消息 DB 的相对路径列表（message/message_N.db）
    pub msg_db_keys: Vec<String>,
    /// username -> contact.verify_flag（0=真人，非 0 通常为公众号/服务号/认证账号）
    pub verify_flags: HashMap<String, i64>,
}

impl Names {
    pub fn display(&self, username: &str) -> String {
        self.map.get(username).cloned().unwrap_or_else(|| username.to_string())
    }

    /// 是否被微信官方标了认证/服务号 flag。未在 contact 表中的 username 返回 false。
    pub fn is_verified(&self, username: &str) -> bool {
        self.verify_flags.get(username).copied().unwrap_or(0) != 0
    }
}

/// 加载联系人缓存（从 contact/contact.db）
pub async fn load_names(db: &DbCache) -> Result<Names> {
    let path = db.get("contact/contact.db").await?;
    let mut map = HashMap::new();
    let mut verify_flags: HashMap<String, i64> = HashMap::new();
    if let Some(p) = path {
        let p2 = p.clone();
        let rows: Vec<(String, String, String, i64)> = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&p2).context("打开 contact.db 失败")?;
            let mut stmt = conn.prepare(
                "SELECT username, nick_name, remark, verify_flag FROM contact"
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1).unwrap_or_default(),
                    row.get::<_, String>(2).unwrap_or_default(),
                    row.get::<_, i64>(3).unwrap_or(0),
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, anyhow::Error>(rows)
        }).await??;

        for (uname, nick, remark, vf) in rows {
            let display = if !remark.is_empty() { remark }
                else if !nick.is_empty() { nick }
                else { uname.clone() };
            verify_flags.insert(uname.clone(), vf);
            map.insert(uname, display);
        }
    }

    let md5_to_uname: HashMap<String, String> = map.keys()
        .map(|u| (format!("{:x}", md5::compute(u.as_bytes())), u.clone()))
        .collect();

    Ok(Names { map, md5_to_uname, msg_db_keys: Vec::new(), verify_flags })
}

/// 查询最近会话列表
pub async fn q_sessions(db: &DbCache, names: &Names, limit: usize) -> Result<Value> {
    let path = db.get("session/session.db").await?
        .context("无法解密 session.db")?;

    let path2 = path.clone();
    let limit_val = limit;
    let rows: Vec<(String, i64, Vec<u8>, i64, i64, String, String)> = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&path2)?;
        let mut stmt = conn.prepare(
            "SELECT username, unread_count, summary, last_timestamp,
                    last_msg_type, last_msg_sender, last_sender_display_name
             FROM SessionTable
             WHERE last_timestamp > 0
             ORDER BY last_timestamp DESC LIMIT ?"
        )?;
        let rows = stmt.query_map([limit_val as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1).unwrap_or(0),
                get_content_bytes(row, 2),
                row.get::<_, i64>(3).unwrap_or(0),
                row.get::<_, i64>(4).unwrap_or(0),
                row.get::<_, String>(5).unwrap_or_default(),
                row.get::<_, String>(6).unwrap_or_default(),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok::<_, anyhow::Error>(rows)
    }).await??;

    let mut results = Vec::new();
    for (username, unread, summary_bytes, ts, msg_type, sender, sender_name) in rows {
        let display = names.display(&username);
        let chat_type = chat_type_of(&username, names);
        let is_group = chat_type == "group";

        // 尝试 zstd 解压 summary
        let summary = decompress_or_str(&summary_bytes);
        let summary = strip_group_prefix(&summary);

        let sender_display = if is_group && !sender.is_empty() {
            names.map.get(&sender).cloned().unwrap_or_else(|| {
                if !sender_name.is_empty() { sender_name.clone() } else { sender.clone() }
            })
        } else {
            String::new()
        };

        results.push(json!({
            "chat": display,
            "username": username,
            "is_group": is_group,
            "chat_type": chat_type,
            "unread": unread,
            "last_msg_type": fmt_type(msg_type),
            "last_sender": sender_display,
            "summary": summary,
            "timestamp": ts,
            "time": fmt_time(ts, "%m-%d %H:%M"),
        }));
    }
    Ok(json!({ "sessions": results }))
}

/// 查询聊天记录
pub async fn q_history(
    db: &DbCache,
    names: &Names,
    chat: &str,
    limit: usize,
    offset: usize,
    since: Option<i64>,
    until: Option<i64>,
    msg_type: Option<i64>,
    with_asr: bool,
) -> Result<Value> {
    let username = resolve_username(chat, names)
        .with_context(|| format!("找不到联系人: {}", chat))?;
    let display = names.display(&username);
    let chat_type = chat_type_of(&username, names);
    let is_group = chat_type == "group";

    let tables = find_msg_tables(db, names, &username).await?;
    if tables.is_empty() {
        anyhow::bail!("找不到 {} 的消息记录", display);
    }

    let mut all_msgs: Vec<Value> = Vec::new();
    for (db_path, table_name) in &tables {
        let path = db_path.clone();
        let tname = table_name.clone();
        let uname = username.clone();
        let is_group2 = is_group;
        let names_map = names.map.clone();
        let since2 = since;
        let until2 = until;
        let limit2 = limit;
        let offset2 = offset;

        let msgs: Vec<Value> = tokio::task::spawn_blocking(move || {
            // per-DB 软上限：offset + limit 已足够全局分页，避免大群全量加载
            let per_db_cap = offset2 + limit2;
            query_messages(&path, &tname, &uname, is_group2, &names_map, since2, until2, msg_type, per_db_cap, 0)
        }).await??;

        all_msgs.extend(msgs);
    }

    all_msgs.sort_by_key(|m| std::cmp::Reverse(m["timestamp"].as_i64().unwrap_or(0)));
    let paged: Vec<Value> = all_msgs.into_iter().skip(offset).take(limit).collect();
    let mut paged = paged;
    paged.sort_by_key(|m| m["timestamp"].as_i64().unwrap_or(0));

    voice_asr::enrich_history_messages(db, &username, &mut paged, with_asr).await;

    Ok(json!({
        "chat": display,
        "username": username,
        "is_group": is_group,
        "chat_type": chat_type,
        "count": paged.len(),
        "messages": paged,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransferAppMsg {
    transfer_id: String,
    title: String,
    description: String,
    paysubtype: String,
    receiver_username: String,
    amount_cents: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferDirection {
    Sent,
    Received,
}

impl TransferDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Received => "received",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferOutcome {
    Completed,
    Refunded,
    Pending,
    Unknown,
}

impl TransferOutcome {
    fn reason(self, direction: TransferDirection) -> &'static str {
        match (self, direction) {
            (Self::Completed, _) => "completed",
            (Self::Refunded, TransferDirection::Sent) => "returned_by_receiver",
            (Self::Refunded, TransferDirection::Received) => "not_collected_or_returned",
            (Self::Pending, _) => "pending_confirmation",
            (Self::Unknown, _) => "unknown_final_state",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransferMessage {
    local_id: i64,
    timestamp: i64,
    sender_username: String,
    app: TransferAppMsg,
}

#[derive(Debug, Default)]
struct TransferBucket {
    sent_count: usize,
    sent_total_cents: i64,
    received_count: usize,
    received_total_cents: i64,
}

impl TransferBucket {
    fn record(&mut self, direction: TransferDirection, amount_cents: i64) {
        match direction {
            TransferDirection::Sent => {
                self.sent_count += 1;
                self.sent_total_cents += amount_cents;
            }
            TransferDirection::Received => {
                self.received_count += 1;
                self.received_total_cents += amount_cents;
            }
        }
    }

    fn to_value(&self, period: &str) -> Value {
        json!({
            "period": period,
            "sent_count": self.sent_count,
            "sent_total": format_cents(self.sent_total_cents),
            "sent_total_cents": self.sent_total_cents,
            "received_count": self.received_count,
            "received_total": format_cents(self.received_total_cents),
            "received_total_cents": self.received_total_cents,
        })
    }
}

#[derive(Debug, Default)]
struct TransferSummary {
    transfers: Vec<Value>,
    excluded_transfers: Vec<Value>,
    summary: TransferBucket,
    monthly: BTreeMap<String, TransferBucket>,
    skipped: usize,
}

/// 查询某个联系人与你之间的转账台账
pub async fn q_transfers(
    db: &DbCache,
    names: &Names,
    chat: &str,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Value> {
    let username = resolve_username(chat, names)
        .with_context(|| format!("找不到联系人: {}", chat))?;
    let display = names.display(&username);
    let chat_type = chat_type_of(&username, names);

    if chat_type != "private" {
        anyhow::bail!("目前只支持私聊联系人的转账统计");
    }

    let tables = find_msg_tables(db, names, &username).await?;
    if tables.is_empty() {
        return Ok(json!({
            "chat": display,
            "username": username,
            "is_group": false,
            "chat_type": chat_type,
            "count": 0,
            "sent_total": "0.00",
            "sent_total_cents": 0,
            "received_total": "0.00",
            "received_total_cents": 0,
            "sent_count": 0,
            "received_count": 0,
            "skipped": 0,
            "summary": TransferBucket::default().to_value("all"),
            "monthly_rows": [],
            "excluded_transfers": [],
            "transfers": [],
        }));
    }

    let mut all_msgs: Vec<TransferMessage> = Vec::new();
    for (db_path, table_name) in &tables {
        let path = db_path.clone();
        let tname = table_name.clone();
        let uname = username.clone();
        let msgs = tokio::task::spawn_blocking(move || {
            query_transfer_messages(&path, &tname, &uname, since, until)
        }).await??;
        all_msgs.extend(msgs);
    }

    let summary = summarize_transfer_messages(&username, all_msgs);
    let monthly_rows: Vec<Value> = summary.monthly.iter()
        .map(|(period, bucket)| bucket.to_value(period))
        .collect();

    Ok(json!({
        "chat": display,
        "username": username,
        "is_group": false,
        "chat_type": chat_type,
        "count": summary.transfers.len(),
        "sent_total": format_cents(summary.summary.sent_total_cents),
        "sent_total_cents": summary.summary.sent_total_cents,
        "received_total": format_cents(summary.summary.received_total_cents),
        "received_total_cents": summary.summary.received_total_cents,
        "sent_count": summary.summary.sent_count,
        "received_count": summary.summary.received_count,
        // 兼容上一版字段名，避免外部脚本马上断掉
        "paid_total": format_cents(summary.summary.sent_total_cents),
        "paid_total_cents": summary.summary.sent_total_cents,
        "skipped": summary.skipped,
        "summary": summary.summary.to_value("all"),
        "monthly_rows": monthly_rows,
        "excluded_transfers": summary.excluded_transfers,
        "transfers": summary.transfers,
    }))
}

/// 搜索消息
pub async fn q_search(
    db: &DbCache,
    names: &Names,
    index: &super::search_index::SearchIndex,
    keyword: &str,
    chats: Option<Vec<String>>,
    limit: usize,
    since: Option<i64>,
    until: Option<i64>,
    msg_type: Option<i64>,
) -> Result<Value> {
    // 查询 ≥ 3 字符时走 FTS 索引（trigram tokenizer 限制）
    if keyword.chars().count() >= 3 {
        // 确保索引是最新的（首次会慢，之后增量都是毫秒）
        match index.sync(db, names).await {
            Ok(n) if n > 0 => eprintln!("[search] 索引新增 {} 条消息", n),
            Ok(_) => {},
            Err(e) => eprintln!("[search] 索引同步失败（降级 LIKE）: {}", e),
        }
        // 解析 chats → usernames（如果指定）
        let chat_unames: Option<Vec<String>> = chats.as_ref().map(|v| {
            v.iter().filter_map(|n| resolve_username(n, names)).collect()
        });
        if let Ok(Some(hits)) = index.search(keyword, chat_unames, names, since, until, msg_type, limit).await {
            return Ok(json!({
                "keyword": keyword,
                "count": hits.len(),
                "results": hits,
                "backend": "fts",
            }));
        }
    }

    // 降级：原有 LIKE 实现
    let mut targets: Vec<(String, String, String, String)> = Vec::new(); // (path, table, display, uname)

    if let Some(chat_names) = chats {
        for chat_name in &chat_names {
            if let Some(uname) = resolve_username(chat_name, names) {
                let tables = find_msg_tables(db, names, &uname).await?;
                for (p, t) in tables {
                    targets.push((p.to_string_lossy().into_owned(), t, names.display(&uname), uname.clone()));
                }
            }
        }
    } else {
        // 全局搜索：遍历所有消息 DB
        for rel_key in &names.msg_db_keys {
            let path = match db.get(rel_key).await? {
                Some(p) => p,
                None => continue,
            };
            let path2 = path.clone();
            let md5_lookup = names.md5_to_uname.clone();
            let names_map = names.map.clone();

            let table_targets: Vec<(String, String, String, String)> = match tokio::task::spawn_blocking(move || {
                let conn = Connection::open(&path2)?;
                let mut stmt = conn.prepare(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'"
                )?;
                let table_names: Vec<String> = stmt.query_map([], |row| row.get(0))?
                    .filter_map(|r| r.ok())
                    .collect();

                let re = msg_table_re();
                let mut result = Vec::new();
                for tname in table_names {
                    if !re.is_match(&tname) {
                        continue;
                    }
                    let hash = &tname[4..];
                    let uname = md5_lookup.get(hash).cloned().unwrap_or_default();
                    let display = if uname.is_empty() {
                        String::new()
                    } else {
                        names_map.get(&uname).cloned().unwrap_or_else(|| uname.clone())
                    };
                    result.push((
                        path2.to_string_lossy().into_owned(),
                        tname,
                        display,
                        uname,
                    ));
                }
                Ok::<_, anyhow::Error>(result)
            }).await {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => { eprintln!("[search] skip DB {}: {}", rel_key, e); continue; }
                Err(e) => { eprintln!("[search] task error {}: {}", rel_key, e); continue; }
            };

            targets.extend(table_targets);
        }
    }

    // 按 db_path 分组
    let mut by_path: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
    for (p, t, d, u) in targets {
        by_path.entry(p).or_default().push((t, d, u));
    }

    let mut results: Vec<Value> = Vec::new();
    let kw = keyword.to_string();
    for (db_path, table_list) in by_path {
        let kw2 = kw.clone();
        let since2 = since;
        let until2 = until;
        let limit2 = limit * 3;

        let names_map2 = names.map.clone();
        let found: Vec<Value> = match tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut all = Vec::new();
            for (tname, display, uname) in &table_list {
                let is_group = uname.contains("@chatroom");
                match search_in_table(&conn, tname, &uname, is_group,
                    &names_map2, &kw2, since2, until2, msg_type, limit2)
                {
                    Ok(rows) => {
                        for mut row in rows {
                            if row.get("chat").map(|v| v.as_str().unwrap_or("")).unwrap_or("").is_empty() {
                                if let Some(obj) = row.as_object_mut() {
                                    obj.insert("chat".into(), serde_json::Value::String(
                                        if display.is_empty() { tname.clone() } else { display.clone() }
                                    ));
                                }
                            }
                            all.push(row);
                        }
                    }
                    Err(e) => eprintln!("[search] skip table {}: {}", tname, e),
                }
            }
            Ok::<_, anyhow::Error>(all)
        }).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => { eprintln!("[search] skip DB: {}", e); continue; }
            Err(e) => { eprintln!("[search] task error: {}", e); continue; }
        };

        results.extend(found);
    }

    results.sort_by_key(|r| std::cmp::Reverse(r["timestamp"].as_i64().unwrap_or(0)));
    let paged: Vec<Value> = results.into_iter().take(limit).collect();
    Ok(json!({ "keyword": keyword, "count": paged.len(), "results": paged }))
}

/// 查询联系人
pub async fn q_contacts(names: &Names, query: Option<&str>, limit: usize) -> Result<Value> {
    let mut contacts: Vec<Value> = names.map.iter()
        .filter(|(u, _)| !u.starts_with("gh_") && !u.starts_with("biz_"))
        .map(|(u, d)| json!({ "username": u, "display": d }))
        .collect();

    if let Some(q) = query {
        let low = q.to_lowercase();
        contacts.retain(|c| {
            c["display"].as_str().map(|s| s.to_lowercase().contains(&low)).unwrap_or(false)
            || c["username"].as_str().map(|s| s.to_lowercase().contains(&low)).unwrap_or(false)
        });
    }

    contacts.sort_by(|a, b| {
        a["display"].as_str().unwrap_or("").cmp(b["display"].as_str().unwrap_or(""))
    });

    let total = contacts.len();
    contacts.truncate(limit);
    Ok(json!({ "contacts": contacts, "total": total }))
}

// ─── 内部辅助函数 ────────────────────────────────────────────────────────────

fn resolve_username(chat_name: &str, names: &Names) -> Option<String> {
    if names.map.contains_key(chat_name)
        || chat_name.contains("@chatroom")
        || chat_name.starts_with("wxid_")
    {
        return Some(chat_name.to_string());
    }
    let low = chat_name.to_lowercase();
    // 精确匹配显示名：排序后取第一个，保证确定性
    let mut exact: Vec<&String> = names.map.iter()
        .filter(|(_, display)| display.to_lowercase() == low)
        .map(|(uname, _)| uname)
        .collect();
    exact.sort();
    if let Some(u) = exact.into_iter().next() {
        return Some(u.clone());
    }
    // 模糊匹配：取 display name 最短的（最精确），相同长度取字典序最小
    let mut candidates: Vec<(&String, &String)> = names.map.iter()
        .filter(|(_, display)| display.to_lowercase().contains(&low))
        .collect();
    candidates.sort_by_key(|(uname, display)| (display.len(), uname.as_str()));
    candidates.into_iter().next().map(|(uname, _)| uname.clone())
}

async fn find_msg_tables(
    db: &DbCache,
    names: &Names,
    username: &str,
) -> Result<Vec<(std::path::PathBuf, String)>> {
    let table_name = format!("Msg_{:x}", md5::compute(username.as_bytes()));
    if !msg_table_re().is_match(&table_name) {
        return Ok(Vec::new());
    }

    let mut results: Vec<(i64, std::path::PathBuf, String)> = Vec::new();
    for rel_key in &names.msg_db_keys {
        let path = match db.get(rel_key).await? {
            Some(p) => p,
            None => continue,
        };
        let tname = table_name.clone();
        let path2 = path.clone();
        let max_ts: Option<i64> = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&path2)?;
            let table_exists: Option<i64> = conn.query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
                [&tname],
                |row| row.get(0),
            ).ok().flatten();
            if table_exists.is_none() {
                return Ok::<_, anyhow::Error>(None);
            }
            let ts: Option<i64> = conn.query_row(
                &format!("SELECT MAX(create_time) FROM [{}]", tname),
                [],
                |row| row.get(0),
            ).ok().flatten();
            Ok(ts)
        }).await??;

        if let Some(ts) = max_ts {
            results.push((ts, path.clone(), table_name.clone()));
        }
    }

    // 按最大时间戳降序排列（最新的优先）
    results.sort_by_key(|(ts, _, _)| std::cmp::Reverse(*ts));
    Ok(results.into_iter().map(|(_, p, t)| (p, t)).collect())
}

fn query_messages(
    db_path: &std::path::Path,
    table: &str,
    chat_username: &str,
    is_group: bool,
    names_map: &HashMap<String, String>,
    since: Option<i64>,
    until: Option<i64>,
    msg_type: Option<i64>,
    limit: usize,
    offset: usize,
) -> Result<Vec<Value>> {
    let conn = Connection::open(db_path)?;
    let id2u = load_id2u(&conn);

    let mut clauses = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(s) = since {
        clauses.push("create_time >= ?");
        params.push(Box::new(s));
    }
    if let Some(u) = until {
        clauses.push("create_time <= ?");
        params.push(Box::new(u));
    }
    if let Some(t) = msg_type {
        clauses.push("local_type = ?");
        params.push(Box::new(t));
    }
    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };

    let sql = format!(
        "SELECT local_id, local_type, create_time, real_sender_id,
                message_content, WCDB_CT_message_content
         FROM [{}] {} ORDER BY create_time DESC LIMIT ? OFFSET ?",
        table, where_clause
    );

    params.push(Box::new(limit as i64));
    params.push(Box::new(offset as i64));

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_ref.as_slice(), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            get_content_bytes(row, 4),
            row.get::<_, i64>(5).unwrap_or(0),
        ))
    })?
    .filter_map(|r| r.ok())
    .collect::<Vec<_>>();

    let mut result = Vec::new();
    for (local_id, local_type, ts, real_sender_id, content_bytes, ct) in rows {
        let content = decompress_message(&content_bytes, ct);
        let sender = sender_label(real_sender_id, &content, is_group, chat_username, &id2u, names_map);
        let text = fmt_content(local_id, local_type, &content, is_group);

        result.push(json!({
            "timestamp": ts,
            "time": fmt_time(ts, "%Y-%m-%d %H:%M"),
            "sender": sender,
            "content": text,
            "type": fmt_type(local_type),
            "local_id": local_id,
        }));
    }
    Ok(result)
}

fn query_transfer_messages(
    db_path: &std::path::Path,
    table: &str,
    chat_username: &str,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Vec<TransferMessage>> {
    let conn = Connection::open(db_path)?;
    let id2u = load_id2u(&conn);

    let mut clauses = vec!["(local_type & 4294967295) = 49".to_string()];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(s) = since {
        clauses.push("create_time >= ?".into());
        params.push(Box::new(s));
    }
    if let Some(u) = until {
        clauses.push("create_time <= ?".into());
        params.push(Box::new(u));
    }
    let where_clause = format!("WHERE {}", clauses.join(" AND "));
    let sql = format!(
        "SELECT local_id, create_time, real_sender_id,
                message_content, WCDB_CT_message_content
         FROM [{}] {} ORDER BY create_time ASC, local_id ASC",
        table, where_clause
    );

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_ref.as_slice(), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            get_content_bytes(row, 3),
            row.get::<_, i64>(4).unwrap_or(0),
        ))
    })?
    .filter_map(|r| r.ok())
    .collect::<Vec<_>>();

    let mut result = Vec::new();
    for (local_id, ts, real_sender_id, content_bytes, ct) in rows {
        let content = decompress_message(&content_bytes, ct);
        let Some(app) = parse_transfer_appmsg_xml(&content) else {
            continue;
        };

        let mut sender_username = id2u.get(&real_sender_id).cloned().unwrap_or_default();
        if sender_username.is_empty() {
            sender_username = infer_sender_from_receiver(chat_username, &app.receiver_username);
        }

        result.push(TransferMessage {
            local_id,
            timestamp: ts,
            sender_username,
            app,
        });
    }

    Ok(result)
}

fn summarize_transfer_messages(chat_username: &str, messages: Vec<TransferMessage>) -> TransferSummary {
    let mut grouped: HashMap<String, Vec<TransferMessage>> = HashMap::new();
    for msg in messages {
        grouped.entry(msg.app.transfer_id.clone()).or_default().push(msg);
    }

    let mut summary = TransferSummary::default();
    for (_, mut group) in grouped {
        group.sort_by_key(|msg| (msg.timestamp, msg.local_id));

        let initiator = group.iter()
            .find(|msg| matches!(msg.app.paysubtype.as_str(), "1" | "8"));
        let Some(representative) = initiator else {
            let fallback = group.first();
            let amount_cents = fallback.and_then(|msg| msg.app.amount_cents)
                .or_else(|| group.iter().find_map(|msg| msg.app.amount_cents));
            let direction = fallback
                .and_then(|msg| determine_transfer_direction(chat_username, msg))
                .or_else(|| group.iter().find_map(|msg| determine_transfer_direction(chat_username, msg)));

            let mut source_local_ids: Vec<i64> = group.iter().map(|msg| msg.local_id).collect();
            source_local_ids.sort_unstable();
            source_local_ids.dedup();

            summary.excluded_transfers.push(json!({
                "time": fallback.map(|msg| fmt_time(msg.timestamp, "%Y-%m-%d %H:%M")).unwrap_or_default(),
                "timestamp": fallback.map(|msg| msg.timestamp).unwrap_or(0),
                "direction": direction.map(|d| d.as_str()).unwrap_or("unknown"),
                "amount": amount_cents.map(format_cents).unwrap_or_else(|| "0.00".into()),
                "amount_display": amount_cents.map(format_cents_with_symbol).unwrap_or_else(|| "￥0.00".into()),
                "amount_cents": amount_cents.unwrap_or(0),
                "transfer_id": fallback.map(|msg| msg.app.transfer_id.clone()).unwrap_or_default(),
                "reason": "missing_initiator_card",
                "source_local_ids": source_local_ids,
            }));
            summary.skipped += 1;
            continue;
        };

        let amount_cents = representative.app.amount_cents
            .or_else(|| group.iter().find_map(|msg| msg.app.amount_cents));
        let direction = determine_transfer_direction(chat_username, representative)
            .or_else(|| group.iter().find_map(|msg| determine_transfer_direction(chat_username, msg)));

        let (amount_cents, direction) = match (amount_cents, direction) {
            (Some(amount_cents), Some(direction)) => (amount_cents, direction),
            _ => {
                summary.skipped += 1;
                continue;
            }
        };

        let mut source_local_ids: Vec<i64> = group.iter().map(|msg| msg.local_id).collect();
        source_local_ids.sort_unstable();
        source_local_ids.dedup();
        let period = fmt_time(representative.timestamp, "%Y-%m");
        let final_subtype = group.iter()
            .rev()
            .find(|msg| !matches!(msg.app.paysubtype.as_str(), "1" | "8"))
            .map(|msg| msg.app.paysubtype.as_str())
            .unwrap_or_default();
        let outcome = classify_transfer_outcome(final_subtype);

        if outcome == TransferOutcome::Completed {
            summary.summary.record(direction, amount_cents);
            summary.monthly.entry(period.clone()).or_default().record(direction, amount_cents);
        } else {
            summary.excluded_transfers.push(json!({
                "time": fmt_time(representative.timestamp, "%Y-%m-%d %H:%M"),
                "timestamp": representative.timestamp,
                "direction": direction.as_str(),
                "amount": format_cents(amount_cents),
                "amount_display": format_cents_with_symbol(amount_cents),
                "amount_cents": amount_cents,
                "transfer_id": representative.app.transfer_id.clone(),
                "reason": outcome.reason(direction),
                "final_subtype": final_subtype,
                "source_local_ids": source_local_ids,
            }));
            summary.skipped += 1;
            continue;
        }

        summary.transfers.push(json!({
            "time": fmt_time(representative.timestamp, "%Y-%m-%d %H:%M"),
            "timestamp": representative.timestamp,
            "month": period,
            "direction": direction.as_str(),
            "final_subtype": final_subtype,
            "amount": format_cents(amount_cents),
            "amount_display": format_cents_with_symbol(amount_cents),
            "amount_cents": amount_cents,
            "transfer_id": representative.app.transfer_id.clone(),
            "title": representative.app.title.clone(),
            "description": representative.app.description.clone(),
            "source_local_ids": source_local_ids,
        }));
    }

    summary.transfers.sort_by_key(|item| item["timestamp"].as_i64().unwrap_or(0));
    summary.excluded_transfers.sort_by_key(|item| item["timestamp"].as_i64().unwrap_or(0));
    summary
}

fn classify_transfer_outcome(final_subtype: &str) -> TransferOutcome {
    match final_subtype {
        "3" => TransferOutcome::Completed,
        "4" => TransferOutcome::Refunded,
        "" => TransferOutcome::Pending,
        _ => TransferOutcome::Unknown,
    }
}

fn determine_transfer_direction(chat_username: &str, msg: &TransferMessage) -> Option<TransferDirection> {
    if !msg.sender_username.is_empty() {
        return Some(if msg.sender_username == chat_username {
            TransferDirection::Received
        } else {
            TransferDirection::Sent
        });
    }
    if !msg.app.receiver_username.is_empty() {
        return Some(if msg.app.receiver_username == chat_username {
            TransferDirection::Sent
        } else {
            TransferDirection::Received
        });
    }
    None
}

fn infer_sender_from_receiver(chat_username: &str, receiver_username: &str) -> String {
    if receiver_username.is_empty() {
        return String::new();
    }
    if receiver_username == chat_username {
        String::new()
    } else {
        chat_username.to_string()
    }
}

fn parse_transfer_appmsg_xml(text: &str) -> Option<TransferAppMsg> {
    let atype = extract_xml_text(text, "type")?;
    if atype != "2000" {
        return None;
    }

    let transfer_id = extract_xml_text(text, "transferid").unwrap_or_default();
    if transfer_id.is_empty() {
        return None;
    }

    let description = extract_xml_text(text, "des").unwrap_or_default();
    let feedesc = extract_xml_text(text, "feedesc").unwrap_or_default();
    let amount_cents = parse_amount_cents(&feedesc)
        .or_else(|| parse_amount_cents(&description));

    Some(TransferAppMsg {
        transfer_id,
        title: extract_xml_text(text, "title").unwrap_or_default(),
        description,
        paysubtype: extract_xml_text(text, "paysubtype").unwrap_or_default(),
        receiver_username: extract_xml_text(text, "receiver_username").unwrap_or_default(),
        amount_cents,
    })
}

fn parse_amount_cents(text: &str) -> Option<i64> {
    for capture in [
        transfer_amount_currency_re().captures(text),
        transfer_amount_yuan_re().captures(text),
        transfer_amount_generic_re().captures(text),
    ] {
        let Some(caps) = capture else {
            continue;
        };
        let amount = caps.get(1)?.as_str();
        if let Some(cents) = decimal_amount_to_cents(amount) {
            return Some(cents);
        }
    }
    None
}

fn transfer_amount_currency_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[¥￥]\s*([0-9][0-9,]*(?:\.[0-9]{1,2})?)").unwrap())
}

fn transfer_amount_yuan_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"([0-9][0-9,]*(?:\.[0-9]{1,2})?)\s*元").unwrap())
}

fn transfer_amount_generic_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"([0-9][0-9,]*(?:\.[0-9]{1,2})?)").unwrap())
}

fn decimal_amount_to_cents(raw: &str) -> Option<i64> {
    let normalized = raw.trim().replace(',', "");
    if normalized.is_empty() {
        return None;
    }

    let (whole, frac) = normalized.split_once('.').unwrap_or((&normalized, ""));
    let whole = whole.parse::<i64>().ok()?;
    let frac = match frac.len() {
        0 => "00".to_string(),
        1 => format!("{}0", frac),
        _ => frac.chars().take(2).collect::<String>(),
    };
    let frac = frac.parse::<i64>().ok()?;
    whole.checked_mul(100)?.checked_add(frac)
}

fn format_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{}{whole}.{frac:02}", sign, whole = abs / 100, frac = abs % 100)
}

fn format_cents_with_symbol(cents: i64) -> String {
    format!("￥{}", format_cents(cents))
}

fn search_in_table(
    conn: &Connection,
    table: &str,
    chat_username: &str,
    is_group: bool,
    names_map: &HashMap<String, String>,
    keyword: &str,
    since: Option<i64>,
    until: Option<i64>,
    msg_type: Option<i64>,
    limit: usize,
) -> Result<Vec<Value>> {
    let id2u = load_id2u(conn);
    // 转义 LIKE 通配符，使用 '\' 作为 ESCAPE 字符
    let escaped_kw = keyword.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    let mut clauses = vec!["message_content LIKE ? ESCAPE '\\'".to_string()];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(format!("%{}%", escaped_kw))];
    if let Some(s) = since {
        clauses.push("create_time >= ?".into());
        params.push(Box::new(s));
    }
    if let Some(u) = until {
        clauses.push("create_time <= ?".into());
        params.push(Box::new(u));
    }
    if let Some(t) = msg_type {
        clauses.push("local_type = ?".into());
        params.push(Box::new(t));
    }
    let where_clause = format!("WHERE {}", clauses.join(" AND "));
    let sql = format!(
        "SELECT local_id, local_type, create_time, real_sender_id,
                message_content, WCDB_CT_message_content
         FROM [{}] {} ORDER BY create_time DESC LIMIT ?",
        table, where_clause
    );
    params.push(Box::new(limit as i64));

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_ref.as_slice(), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            get_content_bytes(row, 4),
            row.get::<_, i64>(5).unwrap_or(0),
        ))
    })?
    .filter_map(|r| r.ok())
    .collect::<Vec<_>>();

    let mut result = Vec::new();
    for (local_id, local_type, ts, real_sender_id, content_bytes, ct) in rows {
        let content = decompress_message(&content_bytes, ct);
        let sender = sender_label(real_sender_id, &content, is_group, chat_username, &id2u, names_map);
        let text = fmt_content(local_id, local_type, &content, is_group);

        result.push(json!({
            "timestamp": ts,
            "time": fmt_time(ts, "%Y-%m-%d %H:%M"),
            "chat": "",
            "sender": sender,
            "content": text,
            "type": fmt_type(local_type),
        }));
    }
    Ok(result)
}

fn load_id2u(conn: &Connection) -> HashMap<i64, String> {
    let mut map = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT rowid, user_name FROM Name2Id") {
        let _ = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        }).map(|rows| {
            for r in rows.flatten() {
                map.insert(r.0, r.1);
            }
        });
    }
    map
}

fn sender_label(
    real_sender_id: i64,
    content: &str,
    is_group: bool,
    chat_username: &str,
    id2u: &HashMap<i64, String>,
    names: &HashMap<String, String>,
) -> String {
    let sender_uname = id2u.get(&real_sender_id).cloned().unwrap_or_default();
    if is_group {
        if !sender_uname.is_empty() && sender_uname != chat_username {
            return names.get(&sender_uname).cloned().unwrap_or(sender_uname);
        }
        if content.contains(":\n") {
            let raw = content.splitn(2, ":\n").next().unwrap_or("");
            return names.get(raw).cloned().unwrap_or_else(|| raw.to_string());
        }
        return String::new();
    }
    if !sender_uname.is_empty() && sender_uname != chat_username {
        return names.get(&sender_uname).cloned().unwrap_or(sender_uname);
    }
    String::new()
}

/// 读取消息内容列（兼容 TEXT 和 BLOB 两种存储类型）
///
/// SQLite 中 message_content 在未压缩时为 TEXT，zstd 压缩后为 BLOB。
/// rusqlite 的 Vec<u8> FromSql 只接受 BLOB，读 TEXT 会静默返回空。
fn get_content_bytes(row: &rusqlite::Row<'_>, idx: usize) -> Vec<u8> {
    // 先尝试 BLOB，再 fallback 到 TEXT→bytes
    row.get::<_, Vec<u8>>(idx)
        .or_else(|_| row.get::<_, String>(idx).map(|s| s.into_bytes()))
        .unwrap_or_default()
}

fn decompress_message(data: &[u8], ct: i64) -> String {
    if ct == 4 && !data.is_empty() {
        // zstd 压缩
        if let Ok(dec) = zstd::decode_all(data) {
            return String::from_utf8_lossy(&dec).into_owned();
        }
    }
    String::from_utf8_lossy(data).into_owned()
}

fn decompress_or_str(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }
    // 尝试 zstd 解压
    if let Ok(dec) = zstd::decode_all(data) {
        if let Ok(s) = String::from_utf8(dec) {
            return s;
        }
    }
    String::from_utf8_lossy(data).into_owned()
}

fn strip_group_prefix(s: &str) -> String {
    if s.contains(":\n") {
        s.splitn(2, ":\n").nth(1).unwrap_or(s).to_string()
    } else {
        s.to_string()
    }
}

pub fn fmt_type(t: i64) -> String {
    let base = (t as u64 & 0xFFFFFFFF) as i64;
    match base {
        1 => "文本".into(),
        3 => "图片".into(),
        34 => "语音".into(),
        42 => "名片".into(),
        43 => "视频".into(),
        47 => "表情".into(),
        48 => "位置".into(),
        49 => "链接/文件".into(),
        50 => "通话".into(),
        10000 => "系统".into(),
        10002 => "撤回".into(),
        _ => format!("type={}", base),
    }
}

fn fmt_content(local_id: i64, local_type: i64, content: &str, is_group: bool) -> String {
    let base = (local_type as u64 & 0xFFFFFFFF) as i64;
    match base {
        3 => return format!("[图片] local_id={}", local_id),
        34 => return "[语音]".into(),
        43 => return "[视频]".into(),
        47 => return "[表情]".into(),
        50 => return "[通话]".into(),
        10000 => return parse_sysmsg(content).unwrap_or_else(|| "[系统消息]".into()),
        10002 => return parse_revoke(content).unwrap_or_else(|| "[撤回了一条消息]".into()),
        _ => {}
    }

    let text = if is_group && content.contains(":\n") {
        content.splitn(2, ":\n").nth(1).unwrap_or(content)
    } else {
        content
    };

    if base == 49 && text.contains("<appmsg") {
        if let Some(parsed) = parse_appmsg(text) {
            return parsed;
        }
    }
    text.to_string()
}

/// 解析撤回消息 XML，提取被撤回的内容摘要
/// `<sysmsg type="revokemsg"><revokemsg><content>...</content></revokemsg></sysmsg>`
fn parse_revoke(xml: &str) -> Option<String> {
    let inner = extract_xml_text(xml, "content")?;
    // 有时 content 是 "xxx recalled a message" 英文，有时是中文
    if inner.is_empty() {
        return Some("[撤回了一条消息]".into());
    }
    // 尝试简化：如果是 XML 格式的撤回内容，直接显示摘要
    Some(format!("[撤回] {}", inner
        .chars()
        .take(30)
        .collect::<String>()))
}

/// 解析系统消息 XML（群通知等）
fn parse_sysmsg(xml: &str) -> Option<String> {
    // 常见格式：<sysmsg type="...">...</sysmsg>
    // 尝试提取 content 标签
    if let Some(s) = extract_xml_text(xml, "content") {
        let cleaned = clean_inline_text(&s);
        if !cleaned.is_empty() {
            return Some(format!("[系统] {}", truncate_chars(&cleaned, 50)));
        }
    }
    // 纯文本系统消息（无 XML）
    if !xml.starts_with('<') {
        let cleaned = clean_inline_text(xml);
        if !cleaned.is_empty() {
            return Some(format!("[系统] {}", truncate_chars(&cleaned, 50)));
        }
    }
    Some("[系统消息]".into())
}

fn parse_appmsg(text: &str) -> Option<String> {
    // 简单 XML 解析，避免引入重量级 XML 库（或直接用 minidom）
    // 这里用基本字符串搜索实现
    if let Some(transfer) = parse_transfer_appmsg_xml(text) {
        return Some(match transfer.amount_cents {
            Some(amount_cents) => format!("[转账] {}", format_cents_with_symbol(amount_cents)),
            None => "[转账] 微信转账".into(),
        });
    }

    let title = extract_xml_text(text, "title")?;
    let atype = extract_xml_text(text, "type").unwrap_or_default();
    match atype.as_str() {
        "6" => Some(if !title.is_empty() { format!("[文件] {}", title) } else { "[文件]".into() }),
        "57" => {
            let ref_content = extract_xml_text(text, "content")
                .map(|s| {
                    // content 可能是 HTML 转义的 XML（被引用的消息是 appmsg 时）
                    let unescaped = unescape_html(&s);
                    // 如果解转义后是 XML，尝试递归解析
                    if unescaped.contains("<appmsg") {
                        if let Some(parsed) = parse_appmsg(&unescaped) {
                            return parsed;
                        }
                    }
                    let s: String = unescaped.split_whitespace().collect::<Vec<_>>().join(" ");
                    if s.chars().count() > 40 {
                        format!("{}...", s.chars().take(40).collect::<String>())
                    } else { s }
                })
                .unwrap_or_default();
            let quote = if !title.is_empty() { format!("[引用] {}", title) } else { "[引用]".into() };
            if !ref_content.is_empty() {
                Some(format!("{}\n  \u{21b3} {}", quote, ref_content))
            } else {
                Some(quote)
            }
        }
        "33" | "36" | "44" => Some(if !title.is_empty() { format!("[小程序] {}", title) } else { "[小程序]".into() }),
        _ => Some(if !title.is_empty() { format!("[链接] {}", title) } else { "[链接/文件]".into() }),
    }
}

fn extract_xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)?;
    let content_start = start + open.len();
    let end = xml[content_start..].find(&close)?;
    let raw = xml[content_start..content_start + end].trim();
    // 剥掉 CDATA 包装（公众号链接的 title/des 常见）
    let stripped = raw.strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(raw);
    Some(stripped.trim().to_string())
}

fn unescape_html(s: &str) -> String {
    s.replace("&lt;", "<")
     .replace("&gt;", ">")
     .replace("&amp;", "&")
     .replace("&quot;", "\"")
     .replace("&apos;", "'")
}

fn xml_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"</?[^>]+>").unwrap())
}

fn clean_inline_text(s: &str) -> String {
    let unescaped = unescape_html(s);
    let without_tags = xml_tag_re().replace_all(&unescaped, "");
    without_tags.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let truncated: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        format!("{}...", truncated)
    } else {
        truncated
    }
}

fn fmt_time(ts: i64, fmt: &str) -> String {
    Local.timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.format(fmt).to_string())
        .unwrap_or_else(|| ts.to_string())
}

// ─── 新增命令查询函数 ──────────────────────────────────────────────────────────

/// 查询有未读消息的会话
///
/// `filter`：按 chat_type 过滤，None 或空 Vec 等价于 "all"。
/// 可选值：`private` / `group` / `official` / `folded` / `all`。
/// 多选支持在 CLI 层用逗号分隔后传入多个元素。
pub async fn q_unread(
    db: &DbCache,
    names: &Names,
    limit: usize,
    filter: Option<Vec<String>>,
) -> Result<Value> {
    let path = db.get("session/session.db").await?
        .context("无法解密 session.db")?;

    // 归一化 filter：小写 + 去除别名。返回 None 代表"不过滤"。
    let filter_set: Option<std::collections::HashSet<&'static str>> = filter.and_then(|v| {
        let mut set = std::collections::HashSet::new();
        for raw in v {
            match raw.trim().to_lowercase().as_str() {
                "" | "all" => return None,
                "private" => { set.insert("private"); }
                "group" => { set.insert("group"); }
                "official" | "official_account" => { set.insert("official_account"); }
                "folded" | "fold" => { set.insert("folded"); }
                _ => {} // 未知值忽略，避免拼错导致什么都不返回
            }
        }
        if set.is_empty() { None } else { Some(set) }
    });

    // 有 filter 时必须全表扫：SQL LIMIT 会把想要的公众号先筛掉。
    // 无 filter 时保留 LIMIT，避免重度用户的大量未读会话拖慢默认路径。
    let has_filter = filter_set.is_some();
    let limit_val = limit;
    let rows: Vec<(String, i64, Vec<u8>, i64, i64, String, String)> = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&path)?;
        let sql = if has_filter {
            "SELECT username, unread_count, summary, last_timestamp,
                    last_msg_type, last_msg_sender, last_sender_display_name
             FROM SessionTable WHERE unread_count > 0
             ORDER BY last_timestamp DESC"
        } else {
            "SELECT username, unread_count, summary, last_timestamp,
                    last_msg_type, last_msg_sender, last_sender_display_name
             FROM SessionTable WHERE unread_count > 0
             ORDER BY last_timestamp DESC LIMIT ?"
        };
        let mut stmt = conn.prepare(sql)?;
        let map_row = |row: &rusqlite::Row<'_>| Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1).unwrap_or(0),
            get_content_bytes(row, 2),
            row.get::<_, i64>(3).unwrap_or(0),
            row.get::<_, i64>(4).unwrap_or(0),
            row.get::<_, String>(5).unwrap_or_default(),
            row.get::<_, String>(6).unwrap_or_default(),
        ));
        let rows = if has_filter {
            stmt.query_map([], map_row)?.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map([limit_val as i64], map_row)?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok::<_, anyhow::Error>(rows)
    }).await??;

    let mut results = Vec::new();
    for (username, unread, summary_bytes, ts, msg_type, sender, sender_name) in rows {
        let chat_type = chat_type_of(&username, names);
        if let Some(ref set) = filter_set {
            if !set.contains(chat_type) { continue; }
        }
        if results.len() >= limit { break; }

        let display = names.display(&username);
        let is_group = chat_type == "group";
        let summary = decompress_or_str(&summary_bytes);
        let summary = strip_group_prefix(&summary);
        let sender_display = if is_group && !sender.is_empty() {
            names.map.get(&sender).cloned().unwrap_or_else(|| {
                if !sender_name.is_empty() { sender_name.clone() } else { sender.clone() }
            })
        } else {
            String::new()
        };
        results.push(json!({
            "chat": display,
            "username": username,
            "is_group": is_group,
            "chat_type": chat_type,
            "unread": unread,
            "last_msg_type": fmt_type(msg_type),
            "last_sender": sender_display,
            "summary": summary,
            "timestamp": ts,
            "time": fmt_time(ts, "%m-%d %H:%M"),
        }));
    }
    let total = results.len();
    Ok(json!({ "sessions": results, "total": total }))
}

/// 查询群成员：优先从 contact.db 的 chatroom_member/chat_room 表获取完整列表，
/// 若表不存在则退化为从消息记录聚合有发言记录的成员
pub async fn q_members(db: &DbCache, names: &Names, chat: &str) -> Result<Value> {
    let username = resolve_username(chat, names)
        .with_context(|| format!("找不到联系人: {}", chat))?;

    if !username.contains("@chatroom") {
        anyhow::bail!("'{}' 不是群聊，无法查看群成员", names.display(&username));
    }

    let display = names.display(&username);
    let names_map = names.map.clone();

    // 优先路径：contact.db → chatroom_member + chat_room（完整成员列表）
    if let Some(contact_p) = db.get("contact/contact.db").await? {
        let uname2 = username.clone();
        let display2 = display.clone();
        let names_map2 = names_map.clone();

        let members_opt: Option<Vec<Value>> = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&contact_p)?;

            let has_table: bool = conn.query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='chatroom_member'",
                [],
                |_| Ok(true),
            ).unwrap_or(false);

            if !has_table {
                return Ok::<_, anyhow::Error>(None);
            }

            // 从 chat_room 表获取整数 room_id 和群主
            // WeChat 不同版本列名可能不同：username / chat_room_name / name
            let (room_id, owner): (i64, String) = [
                "SELECT id, owner FROM chat_room WHERE username = ?",
                "SELECT id, owner FROM chat_room WHERE chat_room_name = ?",
                "SELECT id, owner FROM chat_room WHERE name = ?",
            ].iter().find_map(|sql| {
                conn.query_row(sql, [&uname2], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1).unwrap_or_default()))
                }).ok()
            }).unwrap_or((0, String::new()));

            if room_id == 0 {
                return Ok::<_, anyhow::Error>(None);
            }

            let mut stmt = conn.prepare(
                "SELECT c.username, c.nick_name, c.remark
                 FROM chatroom_member cm
                 LEFT JOIN contact c ON c.id = cm.member_id
                 WHERE cm.room_id = ?"
            )?;
            let raw: Vec<(String, String, String)> = stmt.query_map([room_id], |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_default(),
                    row.get::<_, String>(1).unwrap_or_default(),
                    row.get::<_, String>(2).unwrap_or_default(),
                ))
            })?
            .filter_map(|r| r.ok())
            .filter(|(uid, _, _)| !uid.is_empty())
            .collect();

            if raw.is_empty() {
                return Ok(None);
            }

            let mut members: Vec<Value> = raw.iter().map(|(uid, nick, remark)| {
                let disp = if !remark.is_empty() { remark.clone() }
                    else if !nick.is_empty() { nick.clone() }
                    else { names_map2.get(uid).cloned().unwrap_or_else(|| uid.clone()) };
                let is_owner = uid == &owner && !owner.is_empty();
                json!({ "username": uid, "display": disp, "is_owner": is_owner })
            }).collect();

            // 群主排首位，其余按 display 字典序
            members.sort_by(|a, b| {
                let ao = a["is_owner"].as_bool().unwrap_or(false);
                let bo = b["is_owner"].as_bool().unwrap_or(false);
                if ao != bo { return bo.cmp(&ao); }
                a["display"].as_str().unwrap_or("").cmp(b["display"].as_str().unwrap_or(""))
            });

            let _ = display2; // 不在此 closure 内使用
            Ok(Some(members))
        }).await??;

        if let Some(members) = members_opt {
            return Ok(json!({
                "chat": display,
                "username": username,
                "count": members.len(),
                "members": members,
            }));
        }
    }

    // 降级路径：从消息记录中聚合发言过的成员
    let tables = find_msg_tables(db, names, &username).await?;
    if tables.is_empty() {
        return Ok(json!({
            "chat": display,
            "username": username,
            "count": 0,
            "members": [],
        }));
    }

    let mut sender_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (db_path, table_name) in &tables {
        let path = db_path.clone();
        let tname = table_name.clone();
        let uname = username.clone();

        let senders: Vec<String> = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&path)?;
            let id2u = load_id2u(&conn);
            let mut stmt = conn.prepare(&format!(
                "SELECT DISTINCT real_sender_id FROM [{}] WHERE real_sender_id > 0", tname
            ))?;
            let ids: Vec<i64> = stmt.query_map([], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect();
            let senders: Vec<String> = ids.iter()
                .filter_map(|id| id2u.get(id))
                .filter(|u| *u != &uname)
                .cloned()
                .collect();
            Ok::<_, anyhow::Error>(senders)
        }).await??;

        sender_set.extend(senders);
    }

    let mut members: Vec<Value> = sender_set.iter().map(|u| {
        json!({
            "username": u,
            "display": names_map.get(u).cloned().unwrap_or_else(|| u.clone()),
            "is_owner": false,
        })
    }).collect();
    members.sort_by(|a, b| {
        a["display"].as_str().unwrap_or("").cmp(b["display"].as_str().unwrap_or(""))
    });

    Ok(json!({
        "chat": display,
        "username": username,
        "count": members.len(),
        "members": members,
    }))
}

/// 查询新消息：以 session.db 的 last_timestamp 作为 inbox 索引，
/// 只查询 last_timestamp > state[username] 的会话，精确且高效
pub async fn q_new_messages(
    db: &DbCache,
    names: &Names,
    state: Option<HashMap<String, i64>>,
    limit: usize,
) -> Result<Value> {
    // 首次运行（state=None）或未见过的会话，用 24h 前作为起点，
    // 避免第一次运行时把全量历史消息涌入
    let fallback_ts = chrono::Utc::now().timestamp() - 86400;

    // 1. 从 session.db 读取所有会话的当前 last_timestamp
    let session_path = db.get("session/session.db").await?
        .context("无法解密 session.db")?;

    let all_sessions: Vec<(String, i64)> = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&session_path)?;
        let mut stmt = conn.prepare(
            "SELECT username, last_timestamp FROM SessionTable WHERE last_timestamp > 0"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1).unwrap_or(0)))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok::<_, anyhow::Error>(rows)
    }).await??;

    // 2. 记录 session.db 的当前快照（用于构建 new_state 基础）
    let session_ts_map: HashMap<String, i64> = all_sessions.iter()
        .map(|(u, ts)| (u.clone(), *ts))
        .collect();

    // 3. 找出有新消息的会话
    // 不在 state 中的会话（首次运行或新会话）以 fallback_ts 为基准
    let changed: Vec<(String, i64)> = all_sessions.into_iter()
        .filter(|(uname, ts)| {
            let last_known = state.as_ref()
                .and_then(|m| m.get(uname))
                .copied()
                .unwrap_or(fallback_ts);
            *ts > last_known
        })
        .collect();

    if changed.is_empty() {
        return Ok(json!({
            "count": 0,
            "messages": [],
            "new_state": session_ts_map,
        }));
    }

    // 4. 只查询有新消息的会话的消息表
    // per_table_limit 取 limit*5 防止单表截断，最终由全局 truncate 收尾
    let per_table_limit = limit.saturating_mul(5).max(200);
    let mut all_msgs: Vec<Value> = Vec::new();

    for (uname, _) in &changed {
        let since_ts = state.as_ref()
            .and_then(|m| m.get(uname))
            .copied()
            .unwrap_or(fallback_ts);
        let tables = find_msg_tables(db, names, uname).await?;
        if tables.is_empty() { continue; }

        let display = names.display(uname);
        let chat_type = chat_type_of(uname, names);
        let is_group = chat_type == "group";

        for (db_path, table_name) in &tables {
            let path = db_path.clone();
            let tname = table_name.clone();
            let uname2 = uname.clone();
            let display2 = display.clone();
            let names_map = names.map.clone();
            let tname_for_log = tname.clone();

            let msgs: Vec<Value> = match tokio::task::spawn_blocking(move || {
                let conn = Connection::open(&path)?;
                let id2u = load_id2u(&conn);

                let sql = format!(
                    "SELECT local_id, local_type, create_time, real_sender_id,
                            message_content, WCDB_CT_message_content
                     FROM [{}] WHERE create_time > ? ORDER BY create_time ASC LIMIT ?",
                    tname
                );
                let rows: Vec<_> = conn.prepare(&sql)
                    .and_then(|mut stmt| {
                        stmt.query_map(
                            rusqlite::params![since_ts, per_table_limit as i64],
                            |row| Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, i64>(3)?,
                                get_content_bytes(row, 4),
                                row.get::<_, i64>(5).unwrap_or(0),
                            )),
                        ).map(|it| it.filter_map(|r| r.ok()).collect())
                    })
                    .unwrap_or_default();

                let mut result = Vec::new();
                for (local_id, local_type, ts, real_sender_id, content_bytes, ct) in rows {
                    let content = decompress_message(&content_bytes, ct);
                    let sender = sender_label(real_sender_id, &content, is_group, &uname2, &id2u, &names_map);
                    let text = fmt_content(local_id, local_type, &content, is_group);
                    result.push(json!({
                        "chat": display2,
                        "username": uname2,
                        "is_group": is_group,
                        "chat_type": chat_type,
                        "timestamp": ts,
                        "time": fmt_time(ts, "%Y-%m-%d %H:%M"),
                        "sender": sender,
                        "content": text,
                        "type": fmt_type(local_type),
                    }));
                }
                Ok::<_, anyhow::Error>(result)
            }).await {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => { eprintln!("[new-messages] skip {}: {}", tname_for_log, e); continue; }
                Err(e) => { eprintln!("[new-messages] task error: {}", e); continue; }
            };

            all_msgs.extend(msgs);
        }
    }

    all_msgs.sort_by_key(|m| m["timestamp"].as_i64().unwrap_or(0));
    all_msgs.truncate(limit);

    // 5. 重建 new_state，防止全局 limit 截断导致消息永久丢失：
    //    - 未变化的会话：沿用 session.db 的 last_timestamp
    //    - 变化但全被截断（无消息在最终结果中）：保留旧 since_ts，下次重试
    //    - 变化且有消息返回：推进到该会话在结果中的最大 timestamp
    let mut new_state = session_ts_map;
    // 先把 changed 会话重置回旧 since_ts
    for (uname, _) in &changed {
        let old_ts = state.as_ref()
            .and_then(|m| m.get(uname))
            .copied()
            .unwrap_or(fallback_ts);
        new_state.insert(uname.clone(), old_ts);
    }
    // 再根据实际返回的消息向前推进
    for m in &all_msgs {
        if let (Some(uname), Some(ts)) = (m["username"].as_str(), m["timestamp"].as_i64()) {
            let e = new_state.entry(uname.to_string()).or_insert(0);
            if ts > *e { *e = ts; }
        }
    }

    Ok(json!({
        "count": all_msgs.len(),
        "messages": all_msgs,
        "new_state": new_state,
    }))
}

/// 查询收藏内容（favorite/favorite.db 的 fav_db_item 表）
pub async fn q_favorites(
    db: &DbCache,
    limit: usize,
    fav_type: Option<i64>,
    query: Option<String>,
) -> Result<Value> {
    let path = db.get("favorite/favorite.db").await?
        .context("找不到 favorite.db，请确认微信数据目录")?;

    let rows: Vec<Value> = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&path)?;

        let mut clauses: Vec<&'static str> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(t) = fav_type {
            clauses.push("type = ?");
            params.push(Box::new(t));
        }
        let like_str: Option<String> = query.map(|q| {
            let esc = q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            format!("%{}%", esc)
        });
        if let Some(ref s) = like_str {
            clauses.push("content LIKE ? ESCAPE '\\'");
            params.push(Box::new(s.clone()));
        }

        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        params.push(Box::new(limit as i64));

        let sql = format!(
            "SELECT local_id, type, update_time, content, fromusr, realchatname
             FROM fav_db_item {} ORDER BY update_time DESC LIMIT ?",
            where_clause
        );

        let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<Value> = stmt.query_map(params_ref.as_slice(), |row| {
            Ok((
                row.get::<_, i64>(0).unwrap_or(0),
                row.get::<_, i64>(1).unwrap_or(0),
                row.get::<_, i64>(2).unwrap_or(0),
                row.get::<_, String>(3).unwrap_or_default(),
                row.get::<_, String>(4).unwrap_or_default(),
                row.get::<_, String>(5).unwrap_or_default(),
            ))
        })?
        .filter_map(|r| r.ok())
        .map(|(local_id, ftype, ts, content, fromusr, chatname)| {
            let type_str = match ftype {
                1 => "文本",
                2 => "图片",
                5 => "文章",
                19 => "名片",
                20 => "视频",
                _ => "其他",
            };
            // 安全截断（按 Unicode 字符而非字节）
            let preview: String = content.chars().take(100).collect();
            let preview = if content.chars().count() > 100 {
                format!("{}...", preview)
            } else {
                preview
            };
            // WeChat 部分版本的 update_time 为毫秒，10位以上判定为毫秒后转秒
            let ts_secs = if ts > 9_999_999_999 { ts / 1000 } else { ts };
            json!({
                "id": local_id,
                "type": type_str,
                "type_num": ftype,
                "time": fmt_time(ts_secs, "%Y-%m-%d %H:%M"),
                "timestamp": ts_secs,
                "preview": preview,
                "from": fromusr,
                "chat": chatname,
            })
        })
        .collect();

        Ok::<_, anyhow::Error>(rows)
    }).await??;

    Ok(json!({
        "count": rows.len(),
        "items": rows,
    }))
}

/// 聊天统计：消息总数、类型分布、发言排行、24小时分布
pub async fn q_stats(
    db: &DbCache,
    names: &Names,
    chat: &str,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Value> {
    let username = resolve_username(chat, names)
        .with_context(|| format!("找不到联系人: {}", chat))?;
    let display = names.display(&username);
    let chat_type = chat_type_of(&username, names);
    let is_group = chat_type == "group";

    let tables = find_msg_tables(db, names, &username).await?;
    if tables.is_empty() {
        anyhow::bail!("找不到 {} 的消息记录", display);
    }

    // 跨所有分片 DB 累计统计
    let mut total: i64 = 0;
    let mut type_counts: HashMap<String, i64> = HashMap::new();
    let mut sender_counts: HashMap<String, i64> = HashMap::new();
    let mut hour_counts = [0i64; 24];

    for (db_path, table_name) in &tables {
        let path = db_path.clone();
        let tname = table_name.clone();
        let uname = username.clone();
        let is_group2 = is_group;
        let names_map = names.map.clone();

        // 用 SQL GROUP BY 在数据库侧聚合，避免把全量消息内容加载进内存
        let result: (i64, HashMap<String, i64>, HashMap<String, i64>, [i64; 24]) =
            tokio::task::spawn_blocking(move || {
                let conn = Connection::open(&path)?;
                let id2u = load_id2u(&conn);

                let mut clauses = Vec::new();
                let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                if let Some(s) = since {
                    clauses.push("create_time >= ?");
                    params.push(Box::new(s));
                }
                if let Some(u) = until {
                    clauses.push("create_time <= ?");
                    params.push(Box::new(u));
                }
                let where_clause = if clauses.is_empty() {
                    String::new()
                } else {
                    format!("WHERE {}", clauses.join(" AND "))
                };
                let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                    params.iter().map(|p| p.as_ref()).collect();

                // 1. 总数
                let count: i64 = conn.query_row(
                    &format!("SELECT COUNT(*) FROM [{}] {}", tname, where_clause),
                    params_ref.as_slice(),
                    |row| row.get(0),
                ).unwrap_or(0);

                // 2. 类型分布：SQL GROUP BY，不加载消息内容
                let type_sql = format!(
                    "SELECT (local_type & 0xFFFFFFFF), COUNT(*) FROM [{}] {} GROUP BY (local_type & 0xFFFFFFFF)",
                    tname, where_clause
                );
                let mut type_c: HashMap<String, i64> = HashMap::new();
                if let Ok(mut stmt) = conn.prepare(&type_sql) {
                    let _ = stmt.query_map(params_ref.as_slice(), |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                    }).map(|rows| {
                        for r in rows.flatten() {
                            *type_c.entry(fmt_type(r.0)).or_insert(0) += r.1;
                        }
                    });
                }

                // 3. 小时分布：只取时间戳，不加载消息内容
                let hour_sql = format!(
                    "SELECT create_time FROM [{}] {}",
                    tname, where_clause
                );
                let mut hour_c = [0i64; 24];
                if let Ok(mut stmt) = conn.prepare(&hour_sql) {
                    let _ = stmt.query_map(params_ref.as_slice(), |row| row.get::<_, i64>(0))
                        .map(|rows| {
                            for ts in rows.flatten() {
                                if let Some(dt) = Local.timestamp_opt(ts, 0).single() {
                                    let h = dt.hour() as usize;
                                    if h < 24 { hour_c[h] += 1; }
                                }
                            }
                        });
                }

                // 4. 发言排行：只取 real_sender_id，不加载消息内容
                // where_clause 可能已含 WHERE，用 AND 追加而非重复写 WHERE
                let sender_filter = if where_clause.is_empty() {
                    "WHERE real_sender_id > 0".to_string()
                } else {
                    format!("{} AND real_sender_id > 0", where_clause)
                };
                let sender_sql = format!(
                    "SELECT real_sender_id, COUNT(*) FROM [{}] {} GROUP BY real_sender_id",
                    tname, sender_filter
                );
                let mut sender_c: HashMap<String, i64> = HashMap::new();
                if is_group2 {
                    if let Ok(mut stmt) = conn.prepare(&sender_sql) {
                        let _ = stmt.query_map(params_ref.as_slice(), |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                        }).map(|rows| {
                            for (id, cnt) in rows.flatten() {
                                if let Some(u) = id2u.get(&id) {
                                    if u != &uname {
                                        let name = names_map.get(u).cloned().unwrap_or_else(|| u.clone());
                                        *sender_c.entry(name).or_insert(0) += cnt;
                                    }
                                }
                            }
                        });
                    }
                }

                Ok::<_, anyhow::Error>((count, type_c, sender_c, hour_c))
            }).await??;

        let (count, type_c, sender_c, hour_c) = result;
        total += count;
        for (k, v) in type_c { *type_counts.entry(k).or_insert(0) += v; }
        for (k, v) in sender_c { *sender_counts.entry(k).or_insert(0) += v; }
        for i in 0..24 { hour_counts[i] += hour_c[i]; }
    }

    // 类型分布，按数量降序
    let mut by_type: Vec<Value> = type_counts.iter()
        .map(|(t, c)| json!({ "type": t, "count": c }))
        .collect();
    by_type.sort_by_key(|v| std::cmp::Reverse(v["count"].as_i64().unwrap_or(0)));

    // 发言排行，Top 10
    let mut top_senders: Vec<Value> = sender_counts.iter()
        .map(|(s, c)| json!({ "sender": s, "count": c }))
        .collect();
    top_senders.sort_by_key(|v| std::cmp::Reverse(v["count"].as_i64().unwrap_or(0)));
    top_senders.truncate(10);

    // 24小时分布
    let by_hour: Vec<Value> = hour_counts.iter().enumerate()
        .map(|(h, c)| json!({ "hour": h, "count": c }))
        .collect();

    Ok(json!({
        "chat": display,
        "username": username,
        "is_group": is_group,
        "chat_type": chat_type,
        "total": total,
        "by_type": by_type,
        "top_senders": top_senders,
        "by_hour": by_hour,
    }))
}


// ─── 朋友圈（Moments / SNS） ───────────────────────────────────────────────────

/// 查询朋友圈时间线。数据来自 `sns/sns.db` 的 `SnsTimeLine` 表，
/// `content` 字段是 `<SnsDataItem><TimelineObject>...</TimelineObject></SnsDataItem>` XML。
pub async fn q_moments(
    db: &DbCache,
    names: &Names,
    limit: usize,
    user: Option<String>,
    since: Option<i64>,
    until: Option<i64>,
    query: Option<String>,
    with_media: bool,
) -> Result<Value> {
    let path = db.get("sns/sns.db").await?
        .context("无法解密 sns.db（朋友圈数据库）")?;

    // 先按 user 参数解析出 target username（如指定）
    let target_uname: Option<String> = user.as_deref()
        .and_then(|u| resolve_username(u, names));

    let user_arg = user.clone();
    let target_clone = target_uname.clone();
    let query_clone = query.clone();

    let rows: Vec<(i64, String, String)> = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&path)?;
        let mut clauses: Vec<&'static str> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(u) = target_clone.as_ref() {
            clauses.push("user_name = ?");
            params.push(Box::new(u.clone()));
        }

        let where_clause = if clauses.is_empty() { String::new() }
            else { format!("WHERE {}", clauses.join(" AND ")) };
        // 比 limit 多取几倍，因为还要按 since/until/query 二次过滤
        // SnsTimeLine 的 tid 是 DESC 主键，近似按时间倒序（但 createTime 在 XML 里才精确）
        let fetch_cap = std::cmp::max(limit * 5, 200);
        params.push(Box::new(fetch_cap as i64));

        let sql = format!(
            "SELECT tid, user_name, content FROM SnsTimeLine {} ORDER BY tid DESC LIMIT ?",
            where_clause
        );
        let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<(i64, String, String)> = stmt.query_map(params_ref.as_slice(), |row| {
            Ok((
                row.get::<_, i64>(0).unwrap_or(0),
                row.get::<_, String>(1).unwrap_or_default(),
                row.get::<_, String>(2).unwrap_or_default(),
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
        Ok::<_, anyhow::Error>(rows)
    }).await??;

    // 如果用户指定了 user 但没解析到 target，给个友好错误
    if user_arg.is_some() && target_uname.is_none() {
        anyhow::bail!("找不到联系人: {}", user_arg.unwrap());
    }

    let like = query_clone.as_ref().map(|q| q.to_lowercase());
    let mut out: Vec<Value> = Vec::new();
    for (_tid, uname, content) in rows {
        let parsed = match parse_moment(&content) {
            Some(m) => m,
            None => continue,
        };

        // 时间过滤
        if let Some(s) = since {
            if parsed.create_time < s { continue; }
        }
        if let Some(u) = until {
            if parsed.create_time > u { continue; }
        }
        // 关键词过滤
        if let Some(ref kw) = like {
            if !parsed.text.to_lowercase().contains(kw) { continue; }
        }

        let author_uname = if !parsed.username.is_empty() { &parsed.username } else { &uname };
        let mut obj = serde_json::Map::new();
        obj.insert("id".into(), json!(parsed.id));
        obj.insert("author".into(), json!(names.display(author_uname)));
        obj.insert("username".into(), json!(author_uname.clone()));
        obj.insert("time".into(), json!(fmt_time(parsed.create_time, "%Y-%m-%d %H:%M")));
        obj.insert("timestamp".into(), json!(parsed.create_time));
        obj.insert("type".into(), json!(parsed.kind));
        obj.insert("text".into(), json!(parsed.text));
        if !parsed.link.is_empty() {
            obj.insert("link".into(), json!(parsed.link));
        }
        if with_media && !parsed.media.is_empty() {
            obj.insert("media".into(), json!(parsed.media));
        } else if !parsed.media.is_empty() {
            obj.insert("media_count".into(), json!(parsed.media.len()));
        }
        out.push(Value::Object(obj));

        if out.len() >= limit { break; }
    }

    // 精确按 createTime 倒序（tid 不完全等于 time）
    out.sort_by_key(|m| std::cmp::Reverse(m["timestamp"].as_i64().unwrap_or(0)));
    Ok(json!({ "count": out.len(), "moments": out }))
}

struct ParsedMoment {
    id: String,
    username: String,
    create_time: i64,
    text: String,
    kind: String,        // text / photos / video / link / share / unknown
    link: String,        // ContentObject.contentUrl（分享/链接类才有）
    media: Vec<String>,  // 缩略图或视频 URL
}

fn parse_moment(xml: &str) -> Option<ParsedMoment> {
    let id = extract_xml_text(xml, "id").unwrap_or_default();
    let username = extract_xml_text(xml, "username").unwrap_or_default();
    let create_time: i64 = extract_xml_text(xml, "createTime")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let text = extract_xml_text(xml, "contentDesc").unwrap_or_default();

    // ContentObject 里才有真正的 type
    let content_obj = extract_tag_block(xml, "ContentObject").unwrap_or_default();
    let type_num: i64 = extract_xml_text(&content_obj, "type")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // WeChat 给所有 moments 的 ContentObject 都塞了一个升级占位 URL（page/common_page__upgrade），
    // 那不是真实分享链接，过滤掉
    let link_raw = extract_xml_text(&content_obj, "contentUrl").unwrap_or_default();
    let link = if link_raw.contains("common_page__upgrade") { String::new() } else { link_raw };

    // 抓图片/视频缩略图 URL。media 标签里 thumb 可能是纯文本 URL 也可能带 key/enc_idx 属性
    let mut media: Vec<String> = Vec::new();
    let media_block = extract_tag_block(&content_obj, "mediaList").unwrap_or_default();
    let mut rest = media_block.as_str();
    while let Some(start) = rest.find("<thumb") {
        let after = &rest[start..];
        // 找到 thumb 标签结束位置
        let tag_close = after.find('>')?;
        let after_open = &after[tag_close + 1..];
        // 再找 </thumb>
        if let Some(end) = after_open.find("</thumb>") {
            let url = after_open[..end].trim();
            if !url.is_empty() {
                media.push(unescape_html(url));
            }
            rest = &after_open[end + "</thumb>".len()..];
        } else {
            break;
        }
    }

    if id.is_empty() && create_time == 0 && text.is_empty() && media.is_empty() {
        return None;
    }
    // 类型判断：先按 type_num，再根据实际内容兜底，避免被空 ContentObject 误判
    let kind = moment_type_str(type_num, !media.is_empty(), !link.is_empty()).to_string();
    Some(ParsedMoment { id, username, create_time, text, kind, link, media })
}

/// 提取 `<tag>...</tag>` 的内部块（保留子标签，不做 CDATA 剥离）
fn extract_tag_block(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)?;
    let content_start = start + open.len();
    let end = xml[content_start..].find(&close)?;
    Some(xml[content_start..content_start + end].to_string())
}

/// ContentObject.type 映射（观察到的常见值，未知返回 "unknown"）
///   1=分享链接 / 2=图片 / 3=文字 / 4=音乐 / 5=视频分享 / 15=小视频 / 其他...
/// 同时用 has_media / has_link 兜底：type 不可靠时按实际内容判断
fn moment_type_str(t: i64, has_media: bool, has_link: bool) -> &'static str {
    match t {
        15 => "video",
        4 | 5 => "music",
        _ if has_link => "link",
        _ if has_media => "photos",
        _ => "text",
    }
}

/// 查询朋友圈收件箱（别人对我的评论/点赞通知）。
/// 数据来自 `sns/sns.db` 的 `SnsMessage_tmp3` 表。
/// type 映射：1=点赞 / 2=评论 / 其他。
pub async fn q_moments_inbox(
    db: &DbCache,
    names: &Names,
    limit: usize,
    since: Option<i64>,
    until: Option<i64>,
    unread_only: bool,
) -> Result<Value> {
    let path = db.get("sns/sns.db").await?
        .context("无法解密 sns.db")?;

    let rows: Vec<(i64, i64, String, String, String, String, String, i64, i64)>
        = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&path)?;
        let mut clauses: Vec<&'static str> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(s) = since { clauses.push("create_time >= ?"); params.push(Box::new(s)); }
        if let Some(u) = until { clauses.push("create_time <= ?"); params.push(Box::new(u)); }
        if unread_only { clauses.push("is_unread = 1"); }
        let where_clause = if clauses.is_empty() { String::new() }
            else { format!("WHERE {}", clauses.join(" AND ")) };
        params.push(Box::new(limit as i64));

        let sql = format!(
            "SELECT create_time, type, from_username, from_nickname, \
                    to_username, to_nickname, content, feed_id, is_unread \
             FROM SnsMessage_tmp3 {} ORDER BY create_time DESC LIMIT ?",
            where_clause
        );
        let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<_> = stmt.query_map(params_ref.as_slice(), |r| Ok((
            r.get::<_, i64>(0).unwrap_or(0),
            r.get::<_, i64>(1).unwrap_or(0),
            r.get::<_, String>(2).unwrap_or_default(),
            r.get::<_, String>(3).unwrap_or_default(),
            r.get::<_, String>(4).unwrap_or_default(),
            r.get::<_, String>(5).unwrap_or_default(),
            r.get::<_, String>(6).unwrap_or_default(),
            r.get::<_, i64>(7).unwrap_or(0),
            r.get::<_, i64>(8).unwrap_or(0),
        )))?.filter_map(|r| r.ok()).collect();
        Ok::<_, anyhow::Error>(rows)
    }).await??;

    let out: Vec<Value> = rows.into_iter().map(|(ts, typ, from_u, from_n, to_u, to_n, content, feed_id, unread)| {
        let action = match typ {
            1 => "like",
            2 => "comment",
            _ => "other",
        };
        // 优先用 contacts 解析到的显示名（带备注），fallback 到 SNS 表里的 nickname
        let from_disp = if !from_u.is_empty() && names.map.contains_key(&from_u) {
            names.display(&from_u)
        } else { from_n.clone() };
        let to_disp = if !to_u.is_empty() && names.map.contains_key(&to_u) {
            names.display(&to_u)
        } else { to_n.clone() };

        let mut obj = serde_json::Map::new();
        obj.insert("time".into(), json!(fmt_time(ts, "%Y-%m-%d %H:%M")));
        obj.insert("timestamp".into(), json!(ts));
        obj.insert("action".into(), json!(action));
        obj.insert("from".into(), json!(from_disp));
        obj.insert("from_username".into(), json!(from_u));
        obj.insert("to".into(), json!(to_disp));
        obj.insert("feed_id".into(), json!(feed_id.to_string()));
        if !content.is_empty() {
            obj.insert("content".into(), json!(content));
        }
        if unread != 0 {
            obj.insert("unread".into(), json!(true));
        }
        Value::Object(obj)
    }).collect();

    Ok(json!({ "count": out.len(), "inbox": out }))
}

/// 查询好友申请历史（来自 general.db 的 FMessageTable）。
/// type=37 常规好友申请；scene_ 表示添加途径。
pub async fn q_friend_requests(
    db: &DbCache,
    names: &Names,
    limit: usize,
    since: Option<i64>,
    until: Option<i64>,
    direction: Option<String>,
) -> Result<Value> {
    let path = db.get("general/general.db").await?
        .context("无法解密 general.db")?;

    let dir_filter: Option<i64> = match direction.as_deref() {
        Some("incoming") | Some("received") => Some(0),
        Some("outgoing") | Some("sent")     => Some(1),
        _ => None,
    };

    let rows: Vec<(String, i64, i64, String, i64, i64, String)>
        = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&path)?;
        let mut clauses: Vec<&'static str> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(s) = since { clauses.push("timestamp_ >= ?"); params.push(Box::new(s)); }
        if let Some(u) = until { clauses.push("timestamp_ <= ?"); params.push(Box::new(u)); }
        if let Some(d) = dir_filter { clauses.push("is_sender_ = ?"); params.push(Box::new(d)); }
        let where_clause = if clauses.is_empty() { String::new() }
            else { format!("WHERE {}", clauses.join(" AND ")) };
        params.push(Box::new(limit as i64));

        let sql = format!(
            "SELECT user_name_, type_, timestamp_, content_, is_sender_, scene_, remark_ \
             FROM FMessageTable {} ORDER BY timestamp_ DESC LIMIT ?",
            where_clause
        );
        let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<_> = stmt.query_map(params_ref.as_slice(), |r| Ok((
            r.get::<_, String>(0).unwrap_or_default(),
            r.get::<_, i64>(1).unwrap_or(0),
            r.get::<_, i64>(2).unwrap_or(0),
            r.get::<_, String>(3).unwrap_or_default(),
            r.get::<_, i64>(4).unwrap_or(0),
            r.get::<_, i64>(5).unwrap_or(0),
            r.get::<_, String>(6).unwrap_or_default(),
        )))?.filter_map(|r| r.ok()).collect();
        Ok::<_, anyhow::Error>(rows)
    }).await??;

    let out: Vec<Value> = rows.into_iter().map(|(uname, type_, ts, content, is_sender, scene, remark)| {
        let direction = if is_sender == 1 { "outgoing" } else { "incoming" };
        // 已成为好友的话能解析到显示名
        let display = if names.map.contains_key(&uname) { names.display(&uname) }
            else { uname.clone() };
        let mut obj = serde_json::Map::new();
        obj.insert("time".into(), json!(fmt_time(ts, "%Y-%m-%d %H:%M")));
        obj.insert("timestamp".into(), json!(ts));
        obj.insert("direction".into(), json!(direction));
        obj.insert("contact".into(), json!(display));
        obj.insert("username".into(), json!(uname.clone()));
        obj.insert("content".into(), json!(content));
        obj.insert("scene".into(), json!(scene_str(scene)));
        obj.insert("type".into(), json!(fm_type_str(type_)));
        if !remark.is_empty() {
            obj.insert("remark".into(), json!(remark));
        }
        obj.insert("now_friend".into(), json!(names.map.contains_key(&uname)));
        Value::Object(obj)
    }).collect();

    Ok(json!({ "count": out.len(), "requests": out }))
}

/// 添加场景（FMessageTable.scene_）→ 中文描述
fn scene_str(s: i64) -> &'static str {
    match s {
        1  => "QQ好友",
        3  => "微信号搜索",
        6  => "QQ群",
        7  => "群聊",
        8  => "扫一扫",
        14 => "群聊",
        15 => "名片分享",
        17 => "附近的人/摇一摇",
        18 => "雷达",
        22 => "手机联系人",
        25 => "漂流瓶",
        27 => "搜索手机号",
        29 => "附近的人",
        30 => "手机通讯录",
        _  => "其他",
    }
}

fn fm_type_str(t: i64) -> &'static str {
    match t {
        37 => "好友申请",
        38 => "推荐名片",
        40 => "认证回复",
        65 => "申请通过",
        _  => "其他",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transfer_xml(
        transfer_id: &str,
        paysubtype: &str,
        amount: &str,
        receiver_username: &str,
        description: &str,
    ) -> String {
        format!(
            r#"<msg><appmsg><title><![CDATA[微信转账]]></title><des><![CDATA[{description}]]></des><type>2000</type><wcpayinfo><feedesc><![CDATA[¥{amount}]]></feedesc><paysubtype>{paysubtype}</paysubtype><transferid><![CDATA[{transfer_id}]]></transferid><receiver_username><![CDATA[{receiver_username}]]></receiver_username></wcpayinfo></appmsg></msg>"#
        )
    }

    fn transfer_message(
        local_id: i64,
        timestamp: i64,
        sender_username: &str,
        transfer_id: &str,
        paysubtype: &str,
        amount_cents: i64,
        receiver_username: &str,
        description: &str,
    ) -> TransferMessage {
        TransferMessage {
            local_id,
            timestamp,
            sender_username: sender_username.to_string(),
            app: TransferAppMsg {
                transfer_id: transfer_id.to_string(),
                title: "微信转账".to_string(),
                description: description.to_string(),
                paysubtype: paysubtype.to_string(),
                receiver_username: receiver_username.to_string(),
                amount_cents: Some(amount_cents),
            },
        }
    }

    #[test]
    fn parse_transfer_appmsg_extracts_fields() {
        let xml = transfer_xml(
            "1000050001202512270126284994376",
            "8",
            "4075.00",
            "kuen133",
            "收到转账4075.00元",
        );
        let parsed = parse_transfer_appmsg_xml(&xml).expect("should parse transfer appmsg");

        assert_eq!(parsed.transfer_id, "1000050001202512270126284994376");
        assert_eq!(parsed.paysubtype, "8");
        assert_eq!(parsed.receiver_username, "kuen133");
        assert_eq!(parsed.amount_cents, Some(407_500));
    }

    #[test]
    fn parse_appmsg_formats_transfer_preview() {
        let xml = transfer_xml("t1", "8", "4075.00", "kuen133", "收到转账4075.00元");
        assert_eq!(parse_appmsg(&xml).as_deref(), Some("[转账] ￥4075.00"));
    }

    #[test]
    fn summarize_transfer_messages_dedupes_and_computes_totals() {
        let summary = summarize_transfer_messages(
            "wangchenfei123",
            vec![
                transfer_message(
                    12,
                    2,
                    "kuen133",
                    "t-in",
                    "3",
                    407_500,
                    "wangchenfei123",
                    "转账给对方4075.00元",
                ),
                transfer_message(
                    3,
                    1,
                    "wangchenfei123",
                    "t-in",
                    "8",
                    407_500,
                    "kuen133",
                    "收到转账4075.00元",
                ),
                transfer_message(
                    20,
                    3,
                    "kuen133",
                    "t-out",
                    "1",
                    50_000,
                    "wangchenfei123",
                    "转账给对方500.00元",
                ),
                transfer_message(
                    21,
                    4,
                    "wangchenfei123",
                    "t-out",
                    "3",
                    50_000,
                    "kuen133",
                    "已收款500.00元",
                ),
            ],
        );

        assert_eq!(summary.transfers.len(), 2);
        assert_eq!(summary.summary.received_total_cents, 407_500);
        assert_eq!(summary.summary.sent_total_cents, 50_000);
        assert_eq!(summary.summary.received_count, 1);
        assert_eq!(summary.summary.sent_count, 1);
        assert_eq!(summary.monthly.get("1970-01").map(|b| b.received_total_cents), Some(407_500));
        assert_eq!(summary.monthly.get("1970-01").map(|b| b.sent_total_cents), Some(50_000));
        assert_eq!(summary.skipped, 0);
        assert!(summary.excluded_transfers.is_empty());
        assert_eq!(summary.transfers[0]["transfer_id"].as_str(), Some("t-in"));
        assert_eq!(summary.transfers[0]["direction"].as_str(), Some("received"));
        assert_eq!(summary.transfers[1]["transfer_id"].as_str(), Some("t-out"));
        assert_eq!(summary.transfers[1]["direction"].as_str(), Some("sent"));
    }

    #[test]
    fn summarize_transfer_messages_excludes_status_only_rows() {
        let summary = summarize_transfer_messages(
            "wangchenfei123",
            vec![
                transfer_message(
                    30,
                    5,
                    "wangchenfei123",
                    "t-orphan",
                    "4",
                    150_000,
                    "kuen133",
                    "收到转账1500.00元",
                ),
            ],
        );

        assert!(summary.transfers.is_empty());
        assert_eq!(summary.summary.received_total_cents, 0);
        assert_eq!(summary.summary.sent_total_cents, 0);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.excluded_transfers.len(), 1);
        assert_eq!(summary.excluded_transfers[0]["reason"].as_str(), Some("missing_initiator_card"));
        assert_eq!(summary.excluded_transfers[0]["transfer_id"].as_str(), Some("t-orphan"));
    }

    #[test]
    fn summarize_transfer_messages_excludes_refunded_transfers() {
        let summary = summarize_transfer_messages(
            "wangchenfei123",
            vec![
                transfer_message(
                    40,
                    6,
                    "kuen133",
                    "t-refund",
                    "1",
                    200_000,
                    "wangchenfei123",
                    "收到转账2000.00元",
                ),
                transfer_message(
                    41,
                    7,
                    "wangchenfei123",
                    "t-refund",
                    "4",
                    200_000,
                    "kuen133",
                    "收到转账2000.00元",
                ),
            ],
        );

        assert!(summary.transfers.is_empty());
        assert_eq!(summary.summary.received_total_cents, 0);
        assert_eq!(summary.summary.sent_total_cents, 0);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.excluded_transfers.len(), 1);
        assert_eq!(summary.excluded_transfers[0]["reason"].as_str(), Some("returned_by_receiver"));
        assert_eq!(summary.excluded_transfers[0]["transfer_id"].as_str(), Some("t-refund"));
    }

    #[test]
    fn parse_sysmsg_strips_custom_link_markup() {
        let xml = r#"<sysmsg type="paymsg"><paymsg><content><![CDATA[你有一笔待接收的<_wc_custom_link_ href="weixin://">转账</_wc_custom_link_>]]></content></paymsg></sysmsg>"#;
        assert_eq!(parse_sysmsg(xml).as_deref(), Some("[系统] 你有一笔待接收的转账"));
    }
}
