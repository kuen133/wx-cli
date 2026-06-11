use super::*;

pub async fn q_sessions(db: &DbCache, names: &Names, limit: usize) -> Result<Value> {
    let path = db
        .get("session/session.db")
        .await?
        .context("无法解密 session.db")?;

    let path2 = path.clone();
    let limit_val = limit;
    let rows: Vec<(String, i64, Vec<u8>, i64, i64, String, String)> =
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&path2)?;
            let mut stmt = conn.prepare(
                "SELECT username, unread_count, summary, last_timestamp,
                    last_msg_type, last_msg_sender, last_sender_display_name
             FROM SessionTable
             WHERE last_timestamp > 0
             ORDER BY last_timestamp DESC LIMIT ?",
            )?;
            let rows = stmt
                .query_map([limit_val as i64], |row| {
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
        })
        .await??;

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
                if !sender_name.is_empty() {
                    sender_name.clone()
                } else {
                    sender.clone()
                }
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

pub async fn q_unread(
    db: &DbCache,
    names: &Names,
    limit: usize,
    filter: Option<Vec<String>>,
) -> Result<Value> {
    let path = db
        .get("session/session.db")
        .await?
        .context("无法解密 session.db")?;

    // 归一化 filter：小写 + 去除别名。返回 None 代表"不过滤"。
    let filter_set: Option<std::collections::HashSet<&'static str>> = filter.and_then(|v| {
        let mut set = std::collections::HashSet::new();
        for raw in v {
            match raw.trim().to_lowercase().as_str() {
                "" | "all" => return None,
                "private" => {
                    set.insert("private");
                }
                "group" => {
                    set.insert("group");
                }
                "official" | "official_account" => {
                    set.insert("official_account");
                }
                "folded" | "fold" => {
                    set.insert("folded");
                }
                _ => {} // 未知值忽略，避免拼错导致什么都不返回
            }
        }
        if set.is_empty() {
            None
        } else {
            Some(set)
        }
    });

    // 有 filter 时必须全表扫：SQL LIMIT 会把想要的公众号先筛掉。
    // 无 filter 时保留 LIMIT，避免重度用户的大量未读会话拖慢默认路径。
    let has_filter = filter_set.is_some();
    let limit_val = limit;
    let rows: Vec<(String, i64, Vec<u8>, i64, i64, String, String)> =
        tokio::task::spawn_blocking(move || {
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
            let map_row = |row: &rusqlite::Row<'_>| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1).unwrap_or(0),
                    get_content_bytes(row, 2),
                    row.get::<_, i64>(3).unwrap_or(0),
                    row.get::<_, i64>(4).unwrap_or(0),
                    row.get::<_, String>(5).unwrap_or_default(),
                    row.get::<_, String>(6).unwrap_or_default(),
                ))
            };
            let rows = if has_filter {
                stmt.query_map([], map_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            } else {
                stmt.query_map([limit_val as i64], map_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            Ok::<_, anyhow::Error>(rows)
        })
        .await??;

    let mut results = Vec::new();
    for (username, unread, summary_bytes, ts, msg_type, sender, sender_name) in rows {
        let chat_type = chat_type_of(&username, names);
        if let Some(ref set) = filter_set {
            if !set.contains(chat_type) {
                continue;
            }
        }
        if results.len() >= limit {
            break;
        }

        let display = names.display(&username);
        let is_group = chat_type == "group";
        let summary = decompress_or_str(&summary_bytes);
        let summary = strip_group_prefix(&summary);
        let sender_display = if is_group && !sender.is_empty() {
            names.map.get(&sender).cloned().unwrap_or_else(|| {
                if !sender_name.is_empty() {
                    sender_name.clone()
                } else {
                    sender.clone()
                }
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
