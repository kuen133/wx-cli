use super::*;
use crate::ipc::NewMessageCursor;

pub(crate) fn msg_table_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^Msg_[0-9a-f]{32}$").unwrap())
}

#[derive(Debug, Clone)]
pub(super) struct MsgTable {
    pub rel_key: String,
    pub path: std::path::PathBuf,
    pub table_name: String,
}

fn sqlite_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(super) fn ensure_create_time_index(conn: &Connection, table: &str) {
    // 这里只处理 query 层打开的解密缓存副本，不碰微信源库 db_storage。
    // full_decrypt 重写缓存文件后索引会丢失，下次查询由 IF NOT EXISTS 一次性重建；
    // WAL 增量是原地更新缓存文件，已建索引会保留。建索引失败按 best-effort 忽略，
    // 避免只读缓存、缺表或异常 schema 影响原本可执行的查询。
    let index = format!("idx_{}_ct", table);
    let sql = format!(
        "CREATE INDEX IF NOT EXISTS {} ON {}(create_time)",
        sqlite_identifier(&index),
        sqlite_identifier(table)
    );
    let _ = conn.execute(&sql, []);
}

/// 判定会话类型。返回值固定为 `group` / `official_account` / `folded` / `private` 之一。
///
/// 判据次序：
/// 1. `@chatroom` / 折叠入口特殊 username
/// 2. `contact.verify_flag` 非 0 —— 覆盖所有被微信官方打了认证标的账号，
///    包括 username 为 `wxid_*` 但实为公众号的情况（如"人物"），
///    以及品牌服务号 `cmb4008205555`、系统号 `qqsafe` / `mphelper` 等
/// 3. username 前缀兜底（`gh_*` / `biz_*` / `@*` 等）—— 在 contact 表未加载或没记录时

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
    let username =
        resolve_username(chat, names).with_context(|| format!("找不到联系人: {}", chat))?;
    let display = names.display(&username);
    let chat_type = chat_type_of(&username, names);
    let is_group = chat_type == "group";

    let tables = find_msg_table_infos(db, names, &username).await?;
    if tables.is_empty() {
        anyhow::bail!("找不到 {} 的消息记录", display);
    }

    let mut all_msgs: Vec<(String, Value)> = Vec::new();
    let mut shards_hit = 0usize;
    let account_root = db.db_dir().parent().map(Path::to_path_buf);
    for table in &tables {
        let path = table.path.clone();
        let tname = table.table_name.clone();
        let rel_key = table.rel_key.clone();
        let uname = username.clone();
        let is_group2 = is_group;
        let names_map = names.map.clone();
        let account_root2 = account_root.clone();
        let since2 = since;
        let until2 = until;
        let limit2 = limit;
        let offset2 = offset;

        let msgs: Vec<Value> = tokio::task::spawn_blocking(move || {
            // per-DB 软上限：offset + limit 已足够全局分页，避免大群全量加载
            let per_db_cap = offset2 + limit2;
            query_messages(
                &path,
                &tname,
                &uname,
                is_group2,
                &names_map,
                since2,
                until2,
                msg_type,
                per_db_cap,
                0,
                account_root2.as_deref(),
            )
        })
        .await??;

        if !msgs.is_empty() {
            shards_hit += 1;
        }
        all_msgs.extend(msgs.into_iter().map(|msg| (rel_key.clone(), msg)));
    }

    all_msgs.sort_by_key(|(_, m)| std::cmp::Reverse(m["timestamp"].as_i64().unwrap_or(0)));
    let mut paged: Vec<(String, Value)> = all_msgs.into_iter().skip(offset).take(limit).collect();
    let chat_latest = latest_from_sourced_messages(&paged);
    paged.sort_by_key(|(_, m)| m["timestamp"].as_i64().unwrap_or(0));
    let mut paged: Vec<Value> = paged.into_iter().map(|(_, msg)| msg).collect();

    voice_asr::enrich_history_messages(db, &username, &mut paged, with_asr).await;

    let session_last = session_last_timestamp(db, &username).await;
    let meta = build_query_meta(
        db,
        names,
        chat_latest,
        session_last,
        names.msg_db_keys.len(),
        shards_hit,
        since.is_some() || until.is_some() || offset > 0,
    );

    Ok(attach_meta(
        json!({
            "chat": display,
            "username": username,
            "is_group": is_group,
            "chat_type": chat_type,
            "count": paged.len(),
            "messages": paged,
        }),
        meta,
    ))
}

pub(super) async fn find_msg_tables(
    db: &DbCache,
    names: &Names,
    username: &str,
) -> Result<Vec<(std::path::PathBuf, String)>> {
    Ok(find_msg_table_infos(db, names, username)
        .await?
        .into_iter()
        .map(|info| (info.path, info.table_name))
        .collect())
}

pub(super) async fn find_msg_table_infos(
    db: &DbCache,
    names: &Names,
    username: &str,
) -> Result<Vec<MsgTable>> {
    let table_name = format!("Msg_{:x}", md5::compute(username.as_bytes()));
    if !msg_table_re().is_match(&table_name) {
        return Ok(Vec::new());
    }

    let mut results: Vec<(i64, MsgTable)> = Vec::new();
    for rel_key in &names.msg_db_keys {
        let path = match db.get(rel_key).await? {
            Some(p) => p,
            None => continue,
        };
        let tname = table_name.clone();
        let path2 = path.clone();
        let max_ts: Option<i64> = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&path2)?;
            let table_exists: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
                    [&tname],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            if table_exists.is_none() {
                return Ok::<_, anyhow::Error>(None);
            }
            ensure_create_time_index(&conn, &tname);
            let ts: Option<i64> = conn
                .query_row(
                    &format!("SELECT MAX(create_time) FROM [{}]", tname),
                    [],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            Ok(ts)
        })
        .await??;

        if let Some(ts) = max_ts {
            results.push((
                ts,
                MsgTable {
                    rel_key: rel_key.clone(),
                    path: path.clone(),
                    table_name: table_name.clone(),
                },
            ));
        }
    }

    // 按最大时间戳降序排列（最新的优先）
    results.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
    Ok(results.into_iter().map(|(_, info)| info).collect())
}

pub(super) fn query_messages(
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
    account_root: Option<&Path>,
) -> Result<Vec<Value>> {
    let conn = Connection::open(db_path)?;
    ensure_create_time_index(&conn, table);
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
    let rows = stmt
        .query_map(params_ref.as_slice(), |row| {
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
        let sender_username =
            sender_username(real_sender_id, &content, is_group, chat_username, &id2u);
        let sender = sender_label(
            real_sender_id,
            &content,
            is_group,
            chat_username,
            &id2u,
            names_map,
        );
        let text = fmt_content(local_id, local_type, &content, is_group);

        let mut item = json!({
            "timestamp": ts,
            "time": fmt_time(ts, "%Y-%m-%d %H:%M"),
            "sender": sender,
            "content": text,
            "type": fmt_type(local_type),
            "local_id": local_id,
        });
        add_sender_identity(&mut item, is_group, &sender_username, names_map);

        let image_paths = account_root
            .map(|root| existing_image_paths(root, table, local_id, ts))
            .unwrap_or_default();
        if !image_paths.is_empty() {
            if let Some(obj) = item.as_object_mut() {
                obj.insert(
                    "image_paths".into(),
                    Value::Array(
                        image_paths
                            .iter()
                            .map(|path| Value::String(path.to_string_lossy().into_owned()))
                            .collect(),
                    ),
                );
                obj.insert(
                    "image_path".into(),
                    Value::String(image_paths[0].to_string_lossy().into_owned()),
                );
            }
        }

        result.push(item);
    }
    Ok(result)
}

struct NewMessageHit {
    rel_key: String,
    username: String,
    create_time: i64,
    local_id: i64,
    msg: Value,
}

pub async fn q_new_messages(
    db: &DbCache,
    names: &Names,
    state: Option<HashMap<String, NewMessageCursor>>,
    limit: usize,
) -> Result<Value> {
    // 首次运行（state=None）或未见过的会话，用 24h 前作为起点，
    // 避免第一次运行时把全量历史消息涌入
    let fallback_ts = chrono::Utc::now().timestamp() - 86400;

    // 1. 从 session.db 读取所有会话的当前 last_timestamp
    let session_path = db
        .get("session/session.db")
        .await?
        .context("无法解密 session.db")?;

    let all_sessions: Vec<(String, i64)> = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&session_path)?;
        let mut stmt = conn.prepare(
            "SELECT username, last_timestamp FROM SessionTable WHERE last_timestamp > 0",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1).unwrap_or(0)))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok::<_, anyhow::Error>(rows)
    })
    .await??;

    // 2. 记录 session.db 的当前快照（用于构建 new_state 基础）
    let session_cursor_map: HashMap<String, NewMessageCursor> = all_sessions
        .iter()
        .map(|(u, ts)| {
            let cursor = state
                .as_ref()
                .and_then(|m| m.get(u))
                .copied()
                .filter(|cursor| cursor.create_time == *ts && cursor.local_id.is_some())
                .unwrap_or(NewMessageCursor { create_time: *ts, local_id: None });
            (u.clone(), cursor)
        })
        .collect();

    // 3. 找出有新消息的会话
    // 不在 state 中的会话（首次运行或新会话）以 fallback_ts 为基准
    let changed: Vec<(String, NewMessageCursor)> = all_sessions
        .into_iter()
        .filter_map(|(uname, ts)| {
            let cursor = state
                .as_ref()
                .and_then(|m| m.get(&uname))
                .copied()
                .unwrap_or(NewMessageCursor { create_time: fallback_ts, local_id: None });
            (ts > cursor.create_time || (cursor.local_id.is_some() && ts == cursor.create_time))
                .then_some((uname, cursor))
        })
        .collect();

    if changed.is_empty() {
        let meta = build_query_meta(db, names, None, None, 0, 0, false);
        return Ok(attach_meta(
            json!({
                "count": 0,
                "messages": [],
                "new_state": session_cursor_map,
            }),
            meta,
        ));
    }

    // 4. 只查询有新消息的会话的消息表
    // per_table_limit 取 limit*5 防止单表截断，最终由全局 truncate 收尾
    let per_table_limit = limit.saturating_mul(5).max(200);
    let mut all_msgs: Vec<NewMessageHit> = Vec::new();
    let mut hit_shards = std::collections::HashSet::new();

    for (uname, since_cursor) in &changed {
        let tables = find_msg_table_infos(db, names, uname).await?;
        if tables.is_empty() {
            continue;
        }

        let display = names.display(uname);
        let chat_type = chat_type_of(uname, names);
        let is_group = chat_type == "group";

        for table in &tables {
            let path = table.path.clone();
            let tname = table.table_name.clone();
            let rel_key = table.rel_key.clone();
            let uname2 = uname.clone();
            let display2 = display.clone();
            let names_map = names.map.clone();
            let tname_for_log = tname.clone();
            let since_cursor2 = *since_cursor;
            let rel_key2 = rel_key.clone();

            let msgs: Vec<NewMessageHit> = match tokio::task::spawn_blocking(move || {
                let conn = Connection::open(&path)?;
                ensure_create_time_index(&conn, &tname);
                let id2u = load_id2u(&conn);

                let sql = format!(
                    "SELECT local_id, local_type, create_time, real_sender_id,
                            message_content, WCDB_CT_message_content
                     FROM [{}]
                     WHERE (create_time > ? OR (? IS NOT NULL AND create_time = ? AND local_id > ?))
                     ORDER BY create_time ASC, local_id ASC LIMIT ?",
                    tname
                );
                let rows: Vec<_> = conn
                    .prepare(&sql)
                    .and_then(|mut stmt| {
                        stmt.query_map(
                            rusqlite::params![
                                since_cursor2.create_time,
                                since_cursor2.local_id,
                                since_cursor2.create_time,
                                since_cursor2.local_id,
                                per_table_limit as i64
                            ],
                            |row| {
                                Ok((
                                    row.get::<_, i64>(0)?,
                                    row.get::<_, i64>(1)?,
                                    row.get::<_, i64>(2)?,
                                    row.get::<_, i64>(3)?,
                                    get_content_bytes(row, 4),
                                    row.get::<_, i64>(5).unwrap_or(0),
                                ))
                            },
                        )
                        .map(|it| it.filter_map(|r| r.ok()).collect())
                    })
                    .unwrap_or_default();

                let mut result = Vec::new();
                for (local_id, local_type, ts, real_sender_id, content_bytes, ct) in rows {
                    let content = decompress_message(&content_bytes, ct);
                    let sender_username =
                        sender_username(real_sender_id, &content, is_group, &uname2, &id2u);
                    let sender = sender_label(
                        real_sender_id,
                        &content,
                        is_group,
                        &uname2,
                        &id2u,
                        &names_map,
                    );
                    let text = fmt_content(local_id, local_type, &content, is_group);
                    let mut msg = json!({
                        "chat": display2,
                        "username": uname2,
                        "is_group": is_group,
                        "chat_type": chat_type,
                        "timestamp": ts,
                        "time": fmt_time(ts, "%Y-%m-%d %H:%M"),
                        "sender": sender,
                        "content": text,
                        "type": fmt_type(local_type),
                    });
                    add_sender_identity(&mut msg, is_group, &sender_username, &names_map);
                    result.push(NewMessageHit {
                        rel_key: rel_key2.clone(),
                        username: uname2.clone(),
                        create_time: ts,
                        local_id,
                        msg,
                    });
                }
                Ok::<_, anyhow::Error>(result)
            })
            .await
            {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => {
                    eprintln!("[new-messages] skip {}: {}", tname_for_log, e);
                    continue;
                }
                Err(e) => {
                    eprintln!("[new-messages] task error: {}", e);
                    continue;
                }
            };

            if !msgs.is_empty() {
                hit_shards.insert(rel_key.clone());
            }
            all_msgs.extend(msgs);
        }
    }

    all_msgs.sort_by_key(|hit| (hit.create_time, hit.local_id));
    all_msgs.truncate(limit);
    let chat_latest = latest_from_sourced_messages(
        &all_msgs
            .iter()
            .map(|hit| (hit.rel_key.clone(), hit.msg.clone()))
            .collect::<Vec<_>>(),
    );

    // 5. 重建 new_state，防止全局 limit 截断导致消息永久丢失：
    //    - 未变化的会话：沿用 session.db 的 last_timestamp
    //    - 变化但全被截断（无消息在最终结果中）：保留旧 cursor，下次重试
    //    - 变化且有消息返回：推进到该会话在结果中的最大 (create_time, local_id)
    let mut new_state = session_cursor_map;
    // 先把 changed 会话重置回旧 cursor
    for (uname, cursor) in &changed {
        new_state.insert(uname.clone(), *cursor);
    }
    // 再根据实际返回的消息向前推进
    for hit in &all_msgs {
        let e = new_state
            .entry(hit.username.clone())
            .or_insert(NewMessageCursor { create_time: 0, local_id: None });
        if hit.create_time > e.create_time {
            *e = NewMessageCursor { create_time: hit.create_time, local_id: Some(hit.local_id) };
        } else if hit.create_time == e.create_time && Some(hit.local_id) > e.local_id {
            e.local_id = Some(hit.local_id);
        }
    }

    let messages: Vec<Value> = all_msgs.into_iter().map(|hit| hit.msg).collect();
    let meta = build_query_meta(
        db,
        names,
        chat_latest,
        None,
        names.msg_db_keys.len(),
        hit_shards.len(),
        false,
    );

    Ok(attach_meta(
        json!({
            "count": messages.len(),
            "messages": messages,
            "new_state": new_state,
        }),
        meta,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::cache::DbCache;

    const FAKE_KEY_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn unique_tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("wx-cli-new-messages-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn mtime_nanos_for_test(path: &Path) -> u64 {
        std::fs::metadata(path).and_then(|m| m.modified()).map(|t| {
            t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos() as u64
        }).unwrap_or(0)
    }

    fn insert_message(path: &Path, chat_uname: &str, local_id: i64, ts: i64, content: &str) {
        let table_name = format!("Msg_{:x}", md5::compute(chat_uname.as_bytes()));
        let conn = Connection::open(path).expect("open message db");
        conn.execute(
            &format!("INSERT INTO [{}] VALUES (?1, ?2, ?3, ?4, ?5, ?6)", table_name),
            rusqlite::params![local_id, 1_i64, ts, 7_i64, content, 0_i64],
        ).expect("insert message");
    }

    async fn seeded_cache(root: &Path, entries: &[(&str, &Path)]) -> DbCache {
        let db_dir = root.join("db_storage");
        let cache_dir = root.join("cache");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();

        let mut all_keys = HashMap::new();
        let mut mtimes = serde_json::Map::new();
        for (rel_key, decrypted_path) in entries {
            let raw_db_path = db_dir.join(rel_key);
            std::fs::create_dir_all(raw_db_path.parent().unwrap()).unwrap();
            std::fs::write(&raw_db_path, b"fake encrypted db").unwrap();
            all_keys.insert((*rel_key).to_string(), FAKE_KEY_HEX.to_string());
            mtimes.insert(
                (*rel_key).to_string(),
                json!({
                    "db_mt": mtime_nanos_for_test(&raw_db_path),
                    "wal_mt": 0u64,
                    "path": decrypted_path.display().to_string(),
                }),
            );
        }

        let mtime_file = cache_dir.join("_mtimes.json");
        std::fs::write(&mtime_file, serde_json::Value::Object(mtimes).to_string()).unwrap();
        DbCache::with_dirs(db_dir, cache_dir, mtime_file, all_keys).await.unwrap()
    }

    #[tokio::test]
    async fn new_messages_returns_same_second_arrivals_without_duplicates() {
        let root = unique_tmpdir("same-second");
        let session_db = root.join("session.db");
        let message_db = root.join("message_0.db");
        let rel_key = "message/message_0.db";
        let chat_uname = "room@chatroom";
        let same_second = chrono::Utc::now().timestamp();

        Connection::open(&session_db).unwrap().execute_batch(&format!(
            "CREATE TABLE SessionTable (username TEXT, last_timestamp INTEGER);
             INSERT INTO SessionTable VALUES ('{chat_uname}', {same_second});"
        )).unwrap();
        let table_name = format!("Msg_{:x}", md5::compute(chat_uname.as_bytes()));
        Connection::open(&message_db).unwrap().execute_batch(&format!(
            "CREATE TABLE Name2Id (user_name TEXT);
             INSERT INTO Name2Id(rowid, user_name) VALUES (7, 'wxid_sender');
             CREATE TABLE [{table_name}] (local_id INTEGER, local_type INTEGER,
                create_time INTEGER, real_sender_id INTEGER, message_content TEXT,
                WCDB_CT_message_content INTEGER);"
        )).unwrap();
        insert_message(&message_db, chat_uname, 1, same_second, "first");

        let cache = seeded_cache(&root, &[("session/session.db", &session_db), (rel_key, &message_db)]).await;
        let names = Names {
            map: HashMap::from([
                (chat_uname.to_string(), "Test Room".to_string()),
                ("wxid_sender".to_string(), "Sender".to_string()),
            ]),
            md5_to_uname: HashMap::new(),
            msg_db_keys: vec![rel_key.to_string()],
            verify_flags: HashMap::new(),
        };

        let first = q_new_messages(&cache, &names, None, 10).await.expect("first new-messages");
        assert_eq!(first["count"].as_u64(), Some(1));
        assert_eq!(first["messages"][0]["content"].as_str(), Some("first"));
        assert!(first["messages"][0].get("local_id").is_none());
        let first_state: HashMap<String, NewMessageCursor> =
            serde_json::from_value(first["new_state"].clone()).unwrap();
        assert_eq!(first_state.get(chat_uname).copied(), Some(NewMessageCursor { create_time: same_second, local_id: Some(1) }));

        insert_message(&message_db, chat_uname, 2, same_second, "second");

        let second = q_new_messages(&cache, &names, Some(first_state), 10).await.expect("second new-messages");
        assert_eq!(second["count"].as_u64(), Some(1));
        assert_eq!(second["messages"][0]["content"].as_str(), Some("second"));
        let second_state: HashMap<String, NewMessageCursor> =
            serde_json::from_value(second["new_state"].clone()).unwrap();
        assert_eq!(second_state.get(chat_uname).copied(), Some(NewMessageCursor { create_time: same_second, local_id: Some(2) }));

        let third = q_new_messages(&cache, &names, Some(second_state), 10).await.expect("third new-messages");
        assert_eq!(third["count"].as_u64(), Some(0));
    }
}
