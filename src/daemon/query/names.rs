use super::*;

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
        self.map
            .get(username)
            .cloned()
            .unwrap_or_else(|| username.to_string())
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
            let mut stmt =
                conn.prepare("SELECT username, nick_name, remark, verify_flag FROM contact")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1).unwrap_or_default(),
                        row.get::<_, String>(2).unwrap_or_default(),
                        row.get::<_, i64>(3).unwrap_or(0),
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, anyhow::Error>(rows)
        })
        .await??;

        for (uname, nick, remark, vf) in rows {
            let display = if !remark.is_empty() {
                remark
            } else if !nick.is_empty() {
                nick
            } else {
                uname.clone()
            };
            verify_flags.insert(uname.clone(), vf);
            map.insert(uname, display);
        }
    }

    let md5_to_uname: HashMap<String, String> = map
        .keys()
        .map(|u| (format!("{:x}", md5::compute(u.as_bytes())), u.clone()))
        .collect();

    Ok(Names {
        map,
        md5_to_uname,
        msg_db_keys: Vec::new(),
        verify_flags,
    })
}

pub async fn q_contacts(names: &Names, query: Option<&str>, limit: usize) -> Result<Value> {
    let mut contacts: Vec<Value> = names
        .map
        .iter()
        .filter(|(u, _)| !u.starts_with("gh_") && !u.starts_with("biz_"))
        .map(|(u, d)| json!({ "username": u, "display": d }))
        .collect();

    if let Some(q) = query {
        let low = q.to_lowercase();
        contacts.retain(|c| {
            c["display"]
                .as_str()
                .map(|s| s.to_lowercase().contains(&low))
                .unwrap_or(false)
                || c["username"]
                    .as_str()
                    .map(|s| s.to_lowercase().contains(&low))
                    .unwrap_or(false)
        });
    }

    contacts.sort_by(|a, b| {
        a["display"]
            .as_str()
            .unwrap_or("")
            .cmp(b["display"].as_str().unwrap_or(""))
    });

    let total = contacts.len();
    contacts.truncate(limit);
    Ok(json!({ "contacts": contacts, "total": total }))
}

// ─── 内部辅助函数 ────────────────────────────────────────────────────────────

pub(super) fn resolve_username(chat_name: &str, names: &Names) -> Option<String> {
    if names.map.contains_key(chat_name)
        || chat_name.contains("@chatroom")
        || chat_name.starts_with("wxid_")
    {
        return Some(chat_name.to_string());
    }
    let low = chat_name.to_lowercase();
    // 精确匹配显示名：排序后取第一个，保证确定性
    let mut exact: Vec<&String> = names
        .map
        .iter()
        .filter(|(_, display)| display.to_lowercase() == low)
        .map(|(uname, _)| uname)
        .collect();
    exact.sort();
    if let Some(u) = exact.into_iter().next() {
        return Some(u.clone());
    }
    // 模糊匹配：取 display name 最短的（最精确），相同长度取字典序最小
    let mut candidates: Vec<(&String, &String)> = names
        .map
        .iter()
        .filter(|(_, display)| display.to_lowercase().contains(&low))
        .collect();
    candidates.sort_by_key(|(uname, display)| (display.len(), uname.as_str()));
    candidates
        .into_iter()
        .next()
        .map(|(uname, _)| uname.clone())
}
