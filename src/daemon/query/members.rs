use super::*;

pub async fn q_members(db: &DbCache, names: &Names, chat: &str) -> Result<Value> {
    let username =
        resolve_username(chat, names).with_context(|| format!("找不到联系人: {}", chat))?;

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

            let has_table: bool = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='chatroom_member'",
                    [],
                    |_| Ok(true),
                )
                .unwrap_or(false);

            if !has_table {
                return Ok::<_, anyhow::Error>(None);
            }

            // 从 chat_room 表获取整数 room_id 和群主
            // WeChat 不同版本列名可能不同：username / chat_room_name / name
            let (room_id, owner): (i64, String) = [
                "SELECT id, owner FROM chat_room WHERE username = ?",
                "SELECT id, owner FROM chat_room WHERE chat_room_name = ?",
                "SELECT id, owner FROM chat_room WHERE name = ?",
            ]
            .iter()
            .find_map(|sql| {
                conn.query_row(sql, [&uname2], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1).unwrap_or_default(),
                    ))
                })
                .ok()
            })
            .unwrap_or((0, String::new()));

            if room_id == 0 {
                return Ok::<_, anyhow::Error>(None);
            }

            let mut stmt = conn.prepare(
                "SELECT c.username, c.nick_name, c.remark
                 FROM chatroom_member cm
                 LEFT JOIN contact c ON c.id = cm.member_id
                 WHERE cm.room_id = ?",
            )?;
            let raw: Vec<(String, String, String)> = stmt
                .query_map([room_id], |row| {
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

            let mut members: Vec<Value> = raw
                .iter()
                .map(|(uid, nick, remark)| {
                    let disp = if !remark.is_empty() {
                        remark.clone()
                    } else if !nick.is_empty() {
                        nick.clone()
                    } else {
                        names_map2.get(uid).cloned().unwrap_or_else(|| uid.clone())
                    };
                    let is_owner = uid == &owner && !owner.is_empty();
                    json!({ "username": uid, "display": disp, "is_owner": is_owner })
                })
                .collect();

            // 群主排首位，其余按 display 字典序
            members.sort_by(|a, b| {
                let ao = a["is_owner"].as_bool().unwrap_or(false);
                let bo = b["is_owner"].as_bool().unwrap_or(false);
                if ao != bo {
                    return bo.cmp(&ao);
                }
                a["display"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["display"].as_str().unwrap_or(""))
            });

            let _ = display2; // 不在此 closure 内使用
            Ok(Some(members))
        })
        .await??;

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
                "SELECT DISTINCT real_sender_id FROM [{}] WHERE real_sender_id > 0",
                tname
            ))?;
            let ids: Vec<i64> = stmt
                .query_map([], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect();
            let senders: Vec<String> = ids
                .iter()
                .filter_map(|id| id2u.get(id))
                .filter(|u| *u != &uname)
                .cloned()
                .collect();
            Ok::<_, anyhow::Error>(senders)
        })
        .await??;

        sender_set.extend(senders);
    }

    let mut members: Vec<Value> = sender_set
        .iter()
        .map(|u| {
            json!({
                "username": u,
                "display": names_map.get(u).cloned().unwrap_or_else(|| u.clone()),
                "is_owner": false,
            })
        })
        .collect();
    members.sort_by(|a, b| {
        a["display"]
            .as_str()
            .unwrap_or("")
            .cmp(b["display"].as_str().unwrap_or(""))
    });

    Ok(json!({
        "chat": display,
        "username": username,
        "count": members.len(),
        "members": members,
    }))
}
