use super::*;

pub async fn q_stats(
    db: &DbCache,
    names: &Names,
    chat: &str,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Value> {
    let username =
        resolve_username(chat, names).with_context(|| format!("找不到联系人: {}", chat))?;
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

        // 用 SQL GROUP BY 在数据库侧聚合，避免把全量消息内容加载进内存
        let result: (i64, HashMap<String, i64>, HashMap<String, i64>, [i64; 24]) =
            tokio::task::spawn_blocking(move || {
                let conn = Connection::open(&path)?;
                ensure_create_time_index(&conn, &tname);
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
                                        *sender_c.entry(u.clone()).or_insert(0) += cnt;
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
        for (k, v) in type_c {
            *type_counts.entry(k).or_insert(0) += v;
        }
        for (k, v) in sender_c {
            *sender_counts.entry(k).or_insert(0) += v;
        }
        for i in 0..24 {
            hour_counts[i] += hour_c[i];
        }
    }

    // 类型分布，按数量降序
    let mut by_type: Vec<Value> = type_counts
        .iter()
        .map(|(t, c)| json!({ "type": t, "count": c }))
        .collect();
    by_type.sort_by_key(|v| std::cmp::Reverse(v["count"].as_i64().unwrap_or(0)));

    // 发言排行，Top 10
    let mut top_senders: Vec<Value> = sender_counts
        .iter()
        .map(|(username, count)| {
            let mut row = json!({
                "sender": names.map.get(username).cloned().unwrap_or_else(|| username.clone()),
                "count": count,
            });
            add_sender_identity(&mut row, true, username, &names.map);
            row
        })
        .collect();
    top_senders.sort_by_key(|v| std::cmp::Reverse(v["count"].as_i64().unwrap_or(0)));
    top_senders.truncate(10);

    // 24小时分布
    let by_hour: Vec<Value> = hour_counts
        .iter()
        .enumerate()
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
