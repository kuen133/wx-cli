use super::*;

pub(crate) fn load_id2u(conn: &Connection) -> HashMap<i64, String> {
    let mut map = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT rowid, user_name FROM Name2Id") {
        let _ = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map(|rows| {
                for r in rows.flatten() {
                    map.insert(r.0, r.1);
                }
            });
    }
    map
}

pub(super) fn sender_username(
    real_sender_id: i64,
    content: &str,
    is_group: bool,
    chat_username: &str,
    id2u: &HashMap<i64, String>,
) -> String {
    let sender_uname = id2u.get(&real_sender_id).cloned().unwrap_or_default();
    if !is_group {
        if !sender_uname.is_empty() && sender_uname != chat_username {
            return sender_uname;
        }
        return String::new();
    }
    if !sender_uname.is_empty() && sender_uname != chat_username {
        return sender_uname;
    }
    if content.contains(":\n") {
        return content.splitn(2, ":\n").next().unwrap_or("").to_string();
    }
    String::new()
}

pub(super) fn add_sender_identity(
    row: &mut Value,
    is_group: bool,
    username: &str,
    names: &HashMap<String, String>,
) {
    if !is_group || username.is_empty() {
        return;
    }
    row["sender_username"] = Value::String(username.to_string());
    row["from_wxid"] = Value::String(username.to_string());
    row["sender_contact_display"] = Value::String(
        names
            .get(username)
            .cloned()
            .unwrap_or_else(|| username.to_string()),
    );
}

pub(super) fn sender_label(
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
