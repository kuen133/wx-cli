use super::*;

pub async fn q_sns_notifications(
    db: &DbCache,
    names: &Names,
    limit: usize,
    since: Option<i64>,
    until: Option<i64>,
    include_read: bool,
) -> Result<Value> {
    let path = db.get("sns/sns.db").await?.context("无法解密 sns.db")?;

    let path2 = path.clone();
    type Row = (i64, i64, i64, i64, String, String, String);
    let rows: Vec<Row> = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&path2)?;
        let mut clauses: Vec<&str> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if !include_read {
            clauses.push("is_unread = 1");
        }
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
        let sql = format!(
            "SELECT local_id, create_time, type, feed_id, from_username, from_nickname, content
             FROM SnsMessage_tmp3 {} ORDER BY create_time DESC LIMIT ?",
            where_clause
        );
        params.push(Box::new(limit as i64));
        let params_ref: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_ref.as_slice(), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2).unwrap_or(0),
                    row.get::<_, i64>(3).unwrap_or(0),
                    row.get::<_, String>(4).unwrap_or_default(),
                    row.get::<_, String>(5).unwrap_or_default(),
                    row.get::<_, String>(6).unwrap_or_default(),
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok::<_, anyhow::Error>(rows)
    })
    .await??;

    let feed_ids: Vec<i64> = {
        let mut v: Vec<i64> = rows.iter().map(|r| r.3).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let path3 = path.clone();
    let feed_ids_clone = feed_ids.clone();
    let feeds: HashMap<i64, (String, String)> = tokio::task::spawn_blocking(move || {
        if feed_ids_clone.is_empty() {
            return Ok::<_, anyhow::Error>(HashMap::new());
        }
        let conn = Connection::open(&path3)?;
        let placeholders = std::iter::repeat("?")
            .take(feed_ids_clone.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT tid, user_name, content FROM SnsTimeLine WHERE tid IN ({})",
            placeholders
        );
        let params: Vec<&dyn rusqlite::types::ToSql> = feed_ids_clone
            .iter()
            .map(|id| id as &dyn rusqlite::types::ToSql)
            .collect();
        let mut stmt = conn.prepare(&sql)?;
        let mut map = HashMap::new();
        let mut rows2 = stmt.query(params.as_slice())?;
        while let Some(row) = rows2.next()? {
            let tid: i64 = row.get(0)?;
            let author: String = row.get::<_, String>(1).unwrap_or_default();
            let content: String = row.get::<_, String>(2).unwrap_or_default();
            let preview = extract_xml_text(&content, "contentDesc")
                .map(|s| s.chars().take(60).collect::<String>())
                .unwrap_or_default();
            let author = if author.is_empty() {
                extract_xml_text(&content, "username").unwrap_or_default()
            } else {
                author
            };
            map.insert(tid, (author, preview));
        }
        Ok(map)
    })
    .await??;

    let mut out = Vec::with_capacity(rows.len());
    for (_local_id, ct, _typ, fid, from_u, from_nick, content) in rows {
        let kind = if content.trim().is_empty() {
            "like"
        } else {
            "comment"
        };
        let display = if !from_nick.is_empty() {
            from_nick.clone()
        } else {
            names.display(&from_u)
        };
        let (feed_author_u, feed_preview) = feeds.get(&fid).cloned().unwrap_or_default();
        let feed_author_display = if feed_author_u.is_empty() {
            String::new()
        } else {
            names.display(&feed_author_u)
        };
        out.push(json!({
            "type": kind,
            "time": fmt_time(ct, "%m-%d %H:%M"),
            "timestamp": ct,
            "from_username": from_u,
            "from_nickname": display,
            "content": content,
            "feed_id": fid,
            "feed_author_username": feed_author_u,
            "feed_author": feed_author_display,
            "feed_preview": feed_preview,
        }));
    }
    let total = out.len();
    Ok(json!({ "notifications": out, "total": total }))
}

const SNS_MAX_LIMIT: usize = 10_000;
const SNS_MAX_SCAN: usize = 50_000;

fn escape_like_pattern(s: &str) -> String {
    s.replace('\\', r"\\")
        .replace('%', r"\%")
        .replace('_', r"\_")
}

fn xml_child<'a, 'input>(node: Node<'a, 'input>, tag: &str) -> Option<Node<'a, 'input>> {
    node.children()
        .find(|child| child.is_element() && child.has_tag_name(tag))
}

fn xml_text<'a, 'input>(node: Option<Node<'a, 'input>>) -> Option<String> {
    node.and_then(|n| n.text())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn xml_attr<'a, 'input>(node: Option<Node<'a, 'input>>, attr: &str) -> Option<String> {
    node.and_then(|n| n.attribute(attr))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn insert_media_string(out: &mut serde_json::Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        out.insert(key.to_string(), Value::String(value));
    }
}

fn insert_media_i64(out: &mut serde_json::Map<String, Value>, key: &str, value: Option<i64>) {
    if let Some(value) = value {
        out.insert(key.to_string(), Value::from(value));
    }
}

pub(super) fn parse_media_from_timeline(timeline: Node) -> Vec<Value> {
    let Some(media_list) =
        xml_child(timeline, "ContentObject").and_then(|node| xml_child(node, "mediaList"))
    else {
        return Vec::new();
    };

    media_list
        .children()
        .filter(|node| node.is_element() && node.has_tag_name("media"))
        .map(|media| {
            let url_el = xml_child(media, "url");
            let thumb_el = xml_child(media, "thumb");
            let size_el = xml_child(media, "size");
            let mut out = serde_json::Map::new();

            insert_media_string(&mut out, "type", xml_text(xml_child(media, "type")));
            insert_media_string(&mut out, "sub_type", xml_text(xml_child(media, "sub_type")));
            insert_media_string(&mut out, "url", xml_text(url_el));
            insert_media_string(&mut out, "thumb", xml_text(thumb_el));
            insert_media_string(&mut out, "md5", xml_attr(url_el, "md5"));
            insert_media_string(&mut out, "url_key", xml_attr(url_el, "key"));
            insert_media_string(&mut out, "url_token", xml_attr(url_el, "token"));
            insert_media_string(&mut out, "url_enc_idx", xml_attr(url_el, "enc_idx"));
            insert_media_string(&mut out, "thumb_key", xml_attr(thumb_el, "key"));
            insert_media_string(&mut out, "thumb_token", xml_attr(thumb_el, "token"));
            insert_media_string(&mut out, "thumb_enc_idx", xml_attr(thumb_el, "enc_idx"));
            insert_media_i64(
                &mut out,
                "width",
                xml_attr(size_el, "width").and_then(|v| v.parse::<i64>().ok()),
            );
            insert_media_i64(
                &mut out,
                "height",
                xml_attr(size_el, "height").and_then(|v| v.parse::<i64>().ok()),
            );
            insert_media_i64(
                &mut out,
                "total_size",
                xml_attr(size_el, "totalSize").and_then(|v| v.parse::<i64>().ok()),
            );
            insert_media_string(
                &mut out,
                "video_md5",
                xml_text(xml_child(media, "videomd5")),
            );
            insert_media_i64(
                &mut out,
                "video_duration",
                xml_text(xml_child(media, "videoDuration")).and_then(|v| v.parse::<i64>().ok()),
            );

            Value::Object(out)
        })
        .collect()
}

#[cfg(test)]
pub(super) fn parse_post_media(xml: &str) -> Vec<Value> {
    let Ok(doc) = Document::parse(xml) else {
        return Vec::new();
    };
    let Some(timeline) = doc.descendants().find(|n| n.has_tag_name("TimelineObject")) else {
        return Vec::new();
    };
    parse_media_from_timeline(timeline)
}

pub(super) struct ParsedPost {
    pub(super) tid: i64,
    pub(super) create_time: i64,
    pub(super) author_username: String,
    pub(super) content: String,
    pub(super) media: Vec<Value>,
    pub(super) location: String,
}

fn parse_post_xml_fallback(tid: i64, user_name_column: &str, content: &str) -> ParsedPost {
    let create_time = extract_xml_text(content, "createTime")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let text = extract_xml_text(content, "contentDesc")
        .map(|s| unescape_html(&s))
        .unwrap_or_default();
    let author_username = if user_name_column.is_empty() {
        extract_xml_text(content, "username")
            .map(|s| unescape_html(&s))
            .unwrap_or_default()
    } else {
        user_name_column.to_string()
    };
    let location = extract_xml_attr(content, "location", "poiName")
        .map(|s| unescape_html(&s))
        .unwrap_or_default();

    ParsedPost {
        tid,
        create_time,
        author_username,
        content: text,
        media: Vec::new(),
        location,
    }
}

pub(super) fn parse_post_xml(tid: i64, user_name_column: &str, content: &str) -> ParsedPost {
    let Ok(doc) = Document::parse(content) else {
        return parse_post_xml_fallback(tid, user_name_column, content);
    };
    let Some(timeline) = doc.descendants().find(|n| n.has_tag_name("TimelineObject")) else {
        return parse_post_xml_fallback(tid, user_name_column, content);
    };

    let create_time = xml_text(xml_child(timeline, "createTime"))
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let text = xml_text(xml_child(timeline, "contentDesc")).unwrap_or_default();
    let author_username = if user_name_column.is_empty() {
        xml_text(xml_child(timeline, "username")).unwrap_or_default()
    } else {
        user_name_column.to_string()
    };
    let media = parse_media_from_timeline(timeline);
    let location = xml_child(timeline, "location")
        .and_then(|n| n.attribute("poiName"))
        .map(str::to_string)
        .unwrap_or_default();

    ParsedPost {
        tid,
        create_time,
        author_username,
        content: text,
        media,
        location,
    }
}

fn post_to_value(p: ParsedPost, names: &Names) -> Value {
    let author = if p.author_username.is_empty() {
        String::new()
    } else {
        names.display(&p.author_username)
    };
    json!({
        "tid": p.tid,
        "timestamp": p.create_time,
        "time": fmt_time(p.create_time, "%Y-%m-%d %H:%M"),
        "author_username": p.author_username,
        "author": author,
        "content": p.content,
        "media_count": p.media.len() as i64,
        "media": p.media,
        "location": p.location,
    })
}

pub async fn q_sns_feed(
    db: &DbCache,
    names: &Names,
    limit: usize,
    since: Option<i64>,
    until: Option<i64>,
    user: Option<&str>,
) -> Result<Value> {
    let path = db.get("sns/sns.db").await?.context("无法解密 sns.db")?;

    let limit = limit.min(SNS_MAX_LIMIT);
    let user_uname = match user {
        Some(q) => {
            Some(resolve_username(q, names).with_context(|| format!("找不到联系人: {}", q))?)
        }
        None => None,
    };

    let path2 = path.clone();
    let parsed: Vec<ParsedPost> = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&path2)?;
        let sql = "SELECT tid, user_name, content FROM SnsTimeLine ORDER BY tid DESC";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1).unwrap_or_default(),
            row.get::<_, String>(2).unwrap_or_default(),
        )))?;

        let mut scanned = 0usize;
        let mut out: Vec<ParsedPost> = Vec::new();
        for row in rows {
            scanned += 1;
            if scanned > SNS_MAX_SCAN {
                eprintln!(
                    "[sns_feed] scan 超过硬上限 {}，结果可能不完整。建议加 --user / --since 缩小范围。",
                    SNS_MAX_SCAN
                );
                break;
            }
            let (tid, uname, content) = row?;
            let p = parse_post_xml(tid, &uname, &content);
            if let Some(u) = user_uname.as_ref() { if &p.author_username != u { continue; } }
            if let Some(s) = since { if p.create_time < s { continue; } }
            if let Some(u) = until { if p.create_time > u { continue; } }
            out.push(p);
        }
        out.sort_by_key(|p| std::cmp::Reverse(p.create_time));
        out.truncate(limit);
        Ok::<_, anyhow::Error>(out)
    }).await??;

    let posts: Vec<Value> = parsed
        .into_iter()
        .map(|p| post_to_value(p, names))
        .collect();
    let total = posts.len();
    Ok(json!({ "posts": posts, "total": total }))
}

pub async fn q_sns_search(
    db: &DbCache,
    names: &Names,
    keyword: &str,
    limit: usize,
    since: Option<i64>,
    until: Option<i64>,
    user: Option<&str>,
) -> Result<Value> {
    if keyword.trim().is_empty() {
        anyhow::bail!("搜索关键词不能为空");
    }
    let path = db.get("sns/sns.db").await?.context("无法解密 sns.db")?;

    let limit = limit.min(SNS_MAX_LIMIT);
    let user_uname = match user {
        Some(q) => {
            Some(resolve_username(q, names).with_context(|| format!("找不到联系人: {}", q))?)
        }
        None => None,
    };

    let like_pattern = format!("%{}%", escape_like_pattern(keyword));
    let keyword_owned = keyword.to_string();

    let path2 = path.clone();
    let parsed: Vec<ParsedPost> = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&path2)?;
        let sql = "SELECT tid, user_name, content FROM SnsTimeLine \
                   WHERE content LIKE ? ESCAPE '\\' ORDER BY tid DESC";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([&like_pattern], |row| Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1).unwrap_or_default(),
            row.get::<_, String>(2).unwrap_or_default(),
        )))?;

        let needle = keyword_owned.to_lowercase();
        let mut scanned = 0usize;
        let mut out: Vec<ParsedPost> = Vec::new();
        for row in rows {
            scanned += 1;
            if scanned > SNS_MAX_SCAN {
                eprintln!(
                    "[sns_search] scan 超过硬上限 {}，结果可能不完整。建议缩小 keyword 或加 --user / --since。",
                    SNS_MAX_SCAN
                );
                break;
            }
            let (tid, uname, content) = row?;
            let desc = extract_xml_text(&content, "contentDesc").unwrap_or_default();
            if !desc.to_lowercase().contains(&needle) { continue; }

            let p = parse_post_xml(tid, &uname, &content);
            if let Some(u) = user_uname.as_ref() { if &p.author_username != u { continue; } }
            if let Some(s) = since { if p.create_time < s { continue; } }
            if let Some(u) = until { if p.create_time > u { continue; } }
            out.push(p);
        }
        out.sort_by_key(|p| std::cmp::Reverse(p.create_time));
        out.truncate(limit);
        Ok::<_, anyhow::Error>(out)
    }).await??;

    let posts: Vec<Value> = parsed
        .into_iter()
        .map(|p| post_to_value(p, names))
        .collect();
    let total = posts.len();
    Ok(json!({ "keyword": keyword, "posts": posts, "total": total }))
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
    let path = db
        .get("general/general.db")
        .await?
        .context("无法解密 general.db")?;

    let dir_filter: Option<i64> = match direction.as_deref() {
        Some("incoming") | Some("received") => Some(0),
        Some("outgoing") | Some("sent") => Some(1),
        _ => None,
    };

    let rows: Vec<(String, i64, i64, String, i64, i64, String)> =
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&path)?;
            let mut clauses: Vec<&'static str> = Vec::new();
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            if let Some(s) = since {
                clauses.push("timestamp_ >= ?");
                params.push(Box::new(s));
            }
            if let Some(u) = until {
                clauses.push("timestamp_ <= ?");
                params.push(Box::new(u));
            }
            if let Some(d) = dir_filter {
                clauses.push("is_sender_ = ?");
                params.push(Box::new(d));
            }
            let where_clause = if clauses.is_empty() {
                String::new()
            } else {
                format!("WHERE {}", clauses.join(" AND "))
            };
            params.push(Box::new(limit as i64));

            let sql = format!(
                "SELECT user_name_, type_, timestamp_, content_, is_sender_, scene_, remark_ \
             FROM FMessageTable {} ORDER BY timestamp_ DESC LIMIT ?",
                where_clause
            );
            let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();
            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<_> = stmt
                .query_map(params_ref.as_slice(), |r| {
                    Ok((
                        r.get::<_, String>(0).unwrap_or_default(),
                        r.get::<_, i64>(1).unwrap_or(0),
                        r.get::<_, i64>(2).unwrap_or(0),
                        r.get::<_, String>(3).unwrap_or_default(),
                        r.get::<_, i64>(4).unwrap_or(0),
                        r.get::<_, i64>(5).unwrap_or(0),
                        r.get::<_, String>(6).unwrap_or_default(),
                    ))
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok::<_, anyhow::Error>(rows)
        })
        .await??;

    let out: Vec<Value> = rows
        .into_iter()
        .map(|(uname, type_, ts, content, is_sender, scene, remark)| {
            let direction = if is_sender == 1 {
                "outgoing"
            } else {
                "incoming"
            };
            // 已成为好友的话能解析到显示名
            let display = if names.map.contains_key(&uname) {
                names.display(&uname)
            } else {
                uname.clone()
            };
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
        })
        .collect();

    Ok(json!({ "count": out.len(), "requests": out }))
}

/// 添加场景（FMessageTable.scene_）→ 中文描述
fn scene_str(s: i64) -> &'static str {
    match s {
        1 => "QQ好友",
        3 => "微信号搜索",
        6 => "QQ群",
        7 => "群聊",
        8 => "扫一扫",
        14 => "群聊",
        15 => "名片分享",
        17 => "附近的人/摇一摇",
        18 => "雷达",
        22 => "手机联系人",
        25 => "漂流瓶",
        27 => "搜索手机号",
        29 => "附近的人",
        30 => "手机通讯录",
        _ => "其他",
    }
}

fn fm_type_str(t: i64) -> &'static str {
    match t {
        37 => "好友申请",
        38 => "推荐名片",
        40 => "认证回复",
        65 => "申请通过",
        _ => "其他",
    }
}

// ─── 公众号文章查询 ───────────────────────────────────────────────────────────
