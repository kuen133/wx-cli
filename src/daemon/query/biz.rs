use super::*;

struct BizArticle {
    recv_time: i64,
    account_username: String,
    title: String,
    url: String,
    digest: String,
    cover: String,
    pub_time: i64,
}

fn parse_biz_xml_items(recv_time: i64, account_username: &str, xml: &str) -> Vec<BizArticle> {
    let mut items = Vec::new();
    let mut search_from = 0;
    loop {
        let Some(item_start) = xml[search_from..].find("<item>") else {
            break;
        };
        let abs_start = search_from + item_start;
        let Some(item_end) = xml[abs_start..].find("</item>") else {
            break;
        };
        let abs_end = abs_start + item_end + 7;
        let item_xml = &xml[abs_start..abs_end];

        let title = extract_cdata(item_xml, "title").unwrap_or_default();
        let url = extract_cdata(item_xml, "url").unwrap_or_default();
        if url.is_empty() || title.is_empty() {
            search_from = abs_end;
            continue;
        }
        let digest = extract_cdata(item_xml, "digest").unwrap_or_default();
        let cover = extract_cdata(item_xml, "cover").unwrap_or_default();
        let pub_time = extract_xml_text(item_xml, "pub_time")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(recv_time);

        items.push(BizArticle {
            recv_time,
            account_username: account_username.to_string(),
            title,
            url,
            digest,
            cover,
            pub_time,
        });
        search_from = abs_end;
    }
    items
}

pub async fn q_biz_articles(
    db: &DbCache,
    names: &Names,
    limit: usize,
    account: Option<String>,
    since: Option<i64>,
    until: Option<i64>,
    unread: bool,
) -> Result<Value> {
    let biz_path = db
        .get("message/biz_message_0.db")
        .await?
        .context("无法解密 biz_message_0.db，请确认 all_keys.json 包含对应密钥")?;

    let unread_usernames: Option<std::collections::HashSet<String>> = if unread {
        let session_path = db
            .get("session/session.db")
            .await?
            .context("无法解密 session.db")?;
        let unread_rows: Vec<String> = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&session_path)?;
            let mut stmt =
                conn.prepare("SELECT username FROM SessionTable WHERE unread_count > 0")?;
            let rows: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            Ok::<_, anyhow::Error>(rows)
        })
        .await??;

        let set: std::collections::HashSet<String> = unread_rows
            .into_iter()
            .filter(|u| chat_type_of(u, names) == "official_account")
            .collect();
        if set.is_empty() {
            return Ok(json!({ "count": 0, "articles": [] }));
        }
        Some(set)
    } else {
        None
    };

    let biz_path2 = biz_path.clone();
    let id2username: HashMap<i64, String> = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&biz_path2)?;
        let mut stmt =
            conn.prepare("SELECT rowid, user_name FROM Name2Id WHERE user_name LIKE 'gh_%'")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok::<_, anyhow::Error>(rows.into_iter().collect())
    })
    .await??;

    let md5_to_uname: HashMap<String, String> = id2username
        .values()
        .map(|u| (format!("{:x}", md5::compute(u.as_bytes())), u.clone()))
        .collect();

    let account_low = account.as_deref().map(|s| s.to_lowercase());
    let mut target_usernames: Option<Vec<String>> = account_low.as_ref().map(|low| {
        id2username
            .values()
            .filter(|u| {
                let display = names.display(u);
                display.to_lowercase().contains(low.as_str())
                    || u.to_lowercase().contains(low.as_str())
            })
            .cloned()
            .collect()
    });

    if let Some(ref unread_set) = unread_usernames {
        target_usernames = Some(match target_usernames.take() {
            Some(acc_list) => acc_list
                .into_iter()
                .filter(|u| unread_set.contains(u))
                .collect(),
            None => unread_set.iter().cloned().collect(),
        });
        if target_usernames
            .as_ref()
            .map(|v| v.is_empty())
            .unwrap_or(false)
        {
            return Ok(json!({ "count": 0, "articles": [] }));
        }
    }

    let biz_path3 = biz_path.clone();
    let target_hashes: Option<Vec<String>> = target_usernames.as_ref().map(|unames| {
        unames
            .iter()
            .map(|u| format!("{:x}", md5::compute(u.as_bytes())))
            .collect()
    });

    let rows: Vec<(String, i64, i64, Vec<u8>)> = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&biz_path3)?;
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'")?;
        let table_names: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        let re = regex::Regex::new(r"^Msg_[0-9a-f]{32}$").unwrap();
        let mut all_rows: Vec<(String, i64, i64, Vec<u8>)> = Vec::new();

        for tname in &table_names {
            if !re.is_match(tname) {
                continue;
            }
            let hash = &tname[4..];
            if let Some(ref hashes) = target_hashes {
                if !hashes.iter().any(|h| h == hash) {
                    continue;
                }
            }
            let username = md5_to_uname.get(hash).cloned().unwrap_or_default();
            ensure_create_time_index(&conn, tname);

            let mut clauses: Vec<String> = vec!["(local_type & 4294967295) = 49".to_string()];
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            if let Some(s) = since {
                clauses.push("create_time >= ?".to_string());
                params.push(Box::new(s));
            }
            if let Some(u) = until {
                clauses.push("create_time <= ?".to_string());
                params.push(Box::new(u));
            }
            let where_clause = format!("WHERE {}", clauses.join(" AND "));
            let sql = format!(
                "SELECT create_time, WCDB_CT_message_content, message_content \
                 FROM [{}] {} ORDER BY create_time DESC",
                tname, where_clause
            );

            let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();
            if let Ok(mut inner_stmt) = conn.prepare(&sql) {
                let msg_rows: Vec<_> = inner_stmt
                    .query_map(params_ref.as_slice(), |row| {
                        Ok((
                            username.clone(),
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1).unwrap_or(0),
                            get_content_bytes(row, 2),
                        ))
                    })
                    .map(|it| it.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default();
                all_rows.extend(msg_rows);
            }
        }
        Ok::<_, anyhow::Error>(all_rows)
    })
    .await??;

    let mut articles: Vec<BizArticle> = Vec::new();
    for (username, recv_time, ct, content_bytes) in rows {
        let content = decompress_message(&content_bytes, ct);
        if !content.is_empty() {
            articles.extend(parse_biz_xml_items(recv_time, &username, &content));
        }
    }

    articles.sort_by_key(|a| std::cmp::Reverse(a.pub_time));
    if unread {
        let mut seen = std::collections::HashSet::<String>::new();
        articles.retain(|a| seen.insert(a.account_username.clone()));
    }
    articles.truncate(limit);

    let results: Vec<Value> = articles
        .into_iter()
        .map(|a| {
            json!({
                "time": fmt_time(a.pub_time, "%Y-%m-%d %H:%M"),
                "timestamp": a.pub_time,
                "recv_time": a.recv_time,
                "recv_time_str": fmt_time(a.recv_time, "%Y-%m-%d %H:%M"),
                "account": names.display(&a.account_username),
                "account_username": a.account_username,
                "title": a.title,
                "url": a.url,
                "digest": a.digest,
                "cover_url": a.cover,
            })
        })
        .collect();

    Ok(json!({ "count": results.len(), "articles": results }))
}

// ─── 附件（当前先支持图片）查询与提取 ─────────────────────────────────
