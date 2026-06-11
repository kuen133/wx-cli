use super::*;

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
            Ok(_) => {}
            Err(e) => eprintln!("[search] 索引同步失败（降级 LIKE）: {}", e),
        }
        // 解析 chats → usernames（如果指定）
        let chat_unames: Option<Vec<String>> = chats.as_ref().map(|v| {
            v.iter()
                .filter_map(|n| resolve_username(n, names))
                .collect()
        });
        if let Ok(Some(hits)) = index
            .search(keyword, chat_unames, names, since, until, msg_type, limit)
            .await
        {
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
                    targets.push((
                        p.to_string_lossy().into_owned(),
                        t,
                        names.display(&uname),
                        uname.clone(),
                    ));
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

            let table_targets: Vec<(String, String, String, String)> =
                match tokio::task::spawn_blocking(move || {
                    let conn = Connection::open(&path2)?;
                    let mut stmt = conn.prepare(
                        "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'",
                    )?;
                    let table_names: Vec<String> = stmt
                        .query_map([], |row| row.get(0))?
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
                            names_map
                                .get(&uname)
                                .cloned()
                                .unwrap_or_else(|| uname.clone())
                        };
                        result.push((path2.to_string_lossy().into_owned(), tname, display, uname));
                    }
                    Ok::<_, anyhow::Error>(result)
                })
                .await
                {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => {
                        eprintln!("[search] skip DB {}: {}", rel_key, e);
                        continue;
                    }
                    Err(e) => {
                        eprintln!("[search] task error {}: {}", rel_key, e);
                        continue;
                    }
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
                match search_in_table(
                    &conn,
                    tname,
                    &uname,
                    is_group,
                    &names_map2,
                    &kw2,
                    since2,
                    until2,
                    msg_type,
                    limit2,
                ) {
                    Ok(rows) => {
                        for mut row in rows {
                            if row
                                .get("chat")
                                .map(|v| v.as_str().unwrap_or(""))
                                .unwrap_or("")
                                .is_empty()
                            {
                                if let Some(obj) = row.as_object_mut() {
                                    obj.insert(
                                        "chat".into(),
                                        serde_json::Value::String(if display.is_empty() {
                                            tname.clone()
                                        } else {
                                            display.clone()
                                        }),
                                    );
                                }
                            }
                            all.push(row);
                        }
                    }
                    Err(e) => eprintln!("[search] skip table {}: {}", tname, e),
                }
            }
            Ok::<_, anyhow::Error>(all)
        })
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                eprintln!("[search] skip DB: {}", e);
                continue;
            }
            Err(e) => {
                eprintln!("[search] task error: {}", e);
                continue;
            }
        };

        results.extend(found);
    }

    results.sort_by_key(|r| std::cmp::Reverse(r["timestamp"].as_i64().unwrap_or(0)));
    let paged: Vec<Value> = results.into_iter().take(limit).collect();
    Ok(json!({ "keyword": keyword, "count": paged.len(), "results": paged }))
}

pub(super) fn search_in_table(
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
    ensure_create_time_index(conn, table);
    let id2u = load_id2u(conn);
    // 转义 LIKE 通配符，使用 '\' 作为 ESCAPE 字符
    let escaped_kw = keyword
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let mut clauses = vec!["message_content LIKE ? ESCAPE '\\'".to_string()];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
        vec![Box::new(format!("%{}%", escaped_kw))];
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
            "chat": "",
            "sender": sender,
            "content": text,
            "type": fmt_type(local_type),
        });
        add_sender_identity(&mut item, is_group, &sender_username, names_map);
        result.push(item);
    }
    Ok(result)
}
