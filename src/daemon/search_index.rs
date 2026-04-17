//! 独立的 FTS5 全文搜索索引（trigram tokenizer），在 `~/.wx-cli/cache/_search_index.db`。
//!
//! 微信自带的 `message_fts.db` 用了私有 tokenizer `MMFtsTokenizer`，标准 SQLite 打不开，
//! 所以我们自己建一个。用 trigram tokenizer 对中英文都有效：
//! - `≥ 3 字符` 的查询走 MATCH（毫秒级）
//! - `< 3 字符` 的查询返回 None，调用方降级到 LIKE
//!
//! 索引增量维护：每个会话记 max(create_time)，下次 sync 只插入新消息。

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::cache::DbCache;
use super::query::Names;

pub struct SearchIndex {
    path: PathBuf,
}

impl SearchIndex {
    pub fn new(cache_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(cache_dir).ok();
        let path = cache_dir.join("_search_index.db");
        let conn = Connection::open(&path)
            .with_context(|| format!("open {:?}", path))?;
        conn.execute_batch(r#"
            PRAGMA journal_mode=WAL;
            CREATE VIRTUAL TABLE IF NOT EXISTS msg_fts USING fts5(
                content,
                chat_uname UNINDEXED,
                sender_uname UNINDEXED,
                local_id UNINDEXED,
                create_time UNINDEXED,
                local_type UNINDEXED,
                tokenize='trigram'
            );
            CREATE TABLE IF NOT EXISTS progress (
                chat_uname TEXT PRIMARY KEY,
                max_time INTEGER NOT NULL
            );
        "#)?;
        Ok(Self { path })
    }

    /// 遍历所有 Msg_<md5> 表，把 create_time > last indexed 的消息写入 FTS。
    /// 首次运行会扫整库，耗时随消息总数变化；之后都是增量。
    pub async fn sync(&self, db: &DbCache, names: &Names) -> Result<usize> {
        let index_path = self.path.clone();
        let msg_db_keys = names.msg_db_keys.clone();
        let md5_to_uname = names.md5_to_uname.clone();

        // 预解析所有 Msg 表所在 DB 的解密路径
        let mut db_paths: Vec<(String, PathBuf)> = Vec::new();
        for rel_key in &msg_db_keys {
            if let Some(p) = db.get(rel_key).await? {
                db_paths.push((rel_key.clone(), p));
            }
        }

        tokio::task::spawn_blocking(move || -> Result<usize> {
            let mut idx = Connection::open(&index_path)?;
            // 载入 progress 到 HashMap
            let progress: HashMap<String, i64> = idx.prepare("SELECT chat_uname, max_time FROM progress")?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .filter_map(|r| r.ok())
                .collect();

            let re = msg_table_re();
            let mut total_new = 0;

            for (_rel_key, path) in &db_paths {
                let src = Connection::open(path)?;
                // 载入该 DB 的 sender_id → username 映射
                let id2u = load_id2u(&src);

                let table_names: Vec<String> = src.prepare(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'"
                )?
                .query_map([], |r| r.get(0))?
                .filter_map(|r| r.ok())
                .collect();

                for tname in &table_names {
                    if !re.is_match(tname) { continue; }
                    let hash = &tname[4..];
                    let uname = match md5_to_uname.get(hash) {
                        Some(u) => u.clone(),
                        None => continue,
                    };
                    let last_time = progress.get(&uname).copied().unwrap_or(0);

                    // 只索引文本类消息（local_type=1）和链接摘要（local_type=49），其它类型搜不出文字
                    let sql = format!(
                        "SELECT local_id, local_type, create_time, real_sender_id, \
                                message_content, WCDB_CT_message_content \
                         FROM [{}] WHERE create_time > ? AND local_type IN (1, 49) \
                         ORDER BY create_time ASC",
                        tname
                    );
                    let mut stmt = src.prepare(&sql)?;
                    let rows: Vec<(i64, i64, i64, i64, Vec<u8>, i64)> = stmt.query_map([last_time], |r| Ok((
                        r.get::<_, i64>(0).unwrap_or(0),
                        r.get::<_, i64>(1).unwrap_or(0),
                        r.get::<_, i64>(2).unwrap_or(0),
                        r.get::<_, i64>(3).unwrap_or(0),
                        r.get::<_, Vec<u8>>(4)
                            .or_else(|_| r.get::<_, String>(4).map(|s| s.into_bytes()))
                            .unwrap_or_default(),
                        r.get::<_, i64>(5).unwrap_or(0),
                    )))?.filter_map(|r| r.ok()).collect();

                    if rows.is_empty() { continue; }

                    let tx = idx.transaction()?;
                    let mut max_time = last_time;
                    {
                        let mut ins = tx.prepare(
                            "INSERT INTO msg_fts(content, chat_uname, sender_uname, local_id, create_time, local_type) \
                             VALUES (?, ?, ?, ?, ?, ?)"
                        )?;
                        for (local_id, local_type, ts, sender_id, content_bytes, ct) in &rows {
                            let content = decompress_message(content_bytes, *ct);
                            if content.is_empty() { continue; }
                            let sender_uname = id2u.get(sender_id).cloned().unwrap_or_default();
                            ins.execute(rusqlite::params![&content, &uname, &sender_uname, local_id, ts, local_type])?;
                            if *ts > max_time { max_time = *ts; }
                            total_new += 1;
                        }
                    }
                    tx.execute(
                        "INSERT INTO progress(chat_uname, max_time) VALUES (?, ?) \
                         ON CONFLICT(chat_uname) DO UPDATE SET max_time = excluded.max_time",
                        rusqlite::params![uname, max_time]
                    )?;
                    tx.commit()?;
                }
            }
            Ok(total_new)
        }).await?
    }

    /// 搜索。keyword 字符数 < 3 时返回 None（trigram tokenizer 要求 ≥ 3 字符）。
    pub async fn search(
        &self,
        keyword: &str,
        chat_unames: Option<Vec<String>>,
        names: &Names,
        since: Option<i64>,
        until: Option<i64>,
        msg_type: Option<i64>,
        limit: usize,
    ) -> Result<Option<Vec<Value>>> {
        // trigram 最小 3 字符（按 Unicode char 计）
        if keyword.chars().count() < 3 {
            return Ok(None);
        }
        let path = self.path.clone();
        let kw = keyword.to_string();
        let names_map = names.map.clone();

        let res: Vec<Value> = tokio::task::spawn_blocking(move || -> Result<Vec<Value>> {
            let conn = Connection::open(&path)?;

            let mut clauses: Vec<String> = vec!["msg_fts MATCH ?".into()];
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            // 把 trigram 敏感符号转义：搜索串用引号包住，fts5 就当字面量处理
            let quoted = format!("\"{}\"", kw.replace('"', "\"\""));
            params.push(Box::new(quoted));

            if let Some(chats) = chat_unames {
                if !chats.is_empty() {
                    let placeholders: Vec<&str> = chats.iter().map(|_| "?").collect();
                    clauses.push(format!("chat_uname IN ({})", placeholders.join(",")));
                    for c in chats { params.push(Box::new(c)); }
                }
            }
            if let Some(s) = since { clauses.push("create_time >= ?".into()); params.push(Box::new(s)); }
            if let Some(u) = until { clauses.push("create_time <= ?".into()); params.push(Box::new(u)); }
            if let Some(t) = msg_type { clauses.push("local_type = ?".into()); params.push(Box::new(t)); }

            let sql = format!(
                "SELECT content, chat_uname, sender_uname, local_id, create_time, local_type \
                 FROM msg_fts WHERE {} ORDER BY create_time DESC LIMIT ?",
                clauses.join(" AND ")
            );
            params.push(Box::new(limit as i64));

            let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<Value> = stmt.query_map(params_ref.as_slice(), |r| Ok((
                r.get::<_, String>(0).unwrap_or_default(),
                r.get::<_, String>(1).unwrap_or_default(),
                r.get::<_, String>(2).unwrap_or_default(),
                r.get::<_, i64>(3).unwrap_or(0),
                r.get::<_, i64>(4).unwrap_or(0),
                r.get::<_, i64>(5).unwrap_or(0),
            )))?
            .filter_map(|r| r.ok())
            .map(|(content, chat_uname, sender_uname, local_id, ts, local_type)| {
                let chat_display = names_map.get(&chat_uname).cloned().unwrap_or_else(|| chat_uname.clone());
                let sender_display = if sender_uname.is_empty() {
                    String::new()
                } else {
                    names_map.get(&sender_uname).cloned().unwrap_or_else(|| sender_uname.clone())
                };
                json!({
                    "chat": chat_display,
                    "chat_uname": chat_uname,
                    "sender": sender_display,
                    "local_id": local_id,
                    "timestamp": ts,
                    "time": fmt_ts(ts),
                    "type": fmt_local_type(local_type),
                    "content": content,
                })
            })
            .collect();
            Ok(rows)
        }).await??;

        Ok(Some(res))
    }
}

fn msg_table_re() -> regex::Regex {
    regex::Regex::new(r"^Msg_[0-9a-f]{32}$").unwrap()
}

/// 与 query.rs 的同名函数对应：Name2Id 表 rowid → user_name
fn load_id2u(conn: &Connection) -> HashMap<i64, String> {
    let mut map = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT rowid, user_name FROM Name2Id") {
        if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))) {
            for r in rows.flatten() {
                map.insert(r.0, r.1);
            }
        }
    }
    map
}

/// 与 query.rs 的同名函数对应：ct=4 时 zstd 解压
fn decompress_message(data: &[u8], ct: i64) -> String {
    if ct == 4 && !data.is_empty() {
        if let Ok(dec) = zstd::decode_all(data) {
            return String::from_utf8_lossy(&dec).into_owned();
        }
    }
    String::from_utf8_lossy(data).into_owned()
}

fn fmt_ts(ts: i64) -> String {
    use chrono::{Local, TimeZone};
    Local.timestamp_opt(ts, 0).single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn fmt_local_type(t: i64) -> String {
    static MAP: OnceLock<HashMap<i64, &'static str>> = OnceLock::new();
    let m = MAP.get_or_init(|| {
        [(1,"文本"),(3,"图片"),(34,"语音"),(43,"视频"),(47,"表情"),
         (48,"位置"),(49,"链接/文件"),(50,"通话"),(10000,"系统")]
            .into_iter().collect()
    });
    m.get(&t).copied().unwrap_or("其他").to_string()
}
