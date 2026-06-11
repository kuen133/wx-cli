use super::*;

pub async fn q_favorites(
    db: &DbCache,
    limit: usize,
    fav_type: Option<i64>,
    query: Option<String>,
) -> Result<Value> {
    let path = db
        .get("favorite/favorite.db")
        .await?
        .context("找不到 favorite.db，请确认微信数据目录")?;

    let rows: Vec<Value> = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&path)?;

        let mut clauses: Vec<&'static str> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(t) = fav_type {
            clauses.push("type = ?");
            params.push(Box::new(t));
        }
        let like_str: Option<String> = query.map(|q| {
            let esc = q
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("%{}%", esc)
        });
        if let Some(ref s) = like_str {
            clauses.push("content LIKE ? ESCAPE '\\'");
            params.push(Box::new(s.clone()));
        }

        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        params.push(Box::new(limit as i64));

        let sql = format!(
            "SELECT local_id, type, update_time, content, fromusr, realchatname
             FROM fav_db_item {} ORDER BY update_time DESC LIMIT ?",
            where_clause
        );

        let params_ref: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<Value> = stmt
            .query_map(params_ref.as_slice(), |row| {
                Ok((
                    row.get::<_, i64>(0).unwrap_or(0),
                    row.get::<_, i64>(1).unwrap_or(0),
                    row.get::<_, i64>(2).unwrap_or(0),
                    row.get::<_, String>(3).unwrap_or_default(),
                    row.get::<_, String>(4).unwrap_or_default(),
                    row.get::<_, String>(5).unwrap_or_default(),
                ))
            })?
            .filter_map(|r| r.ok())
            .map(|(local_id, ftype, ts, content, fromusr, chatname)| {
                let type_str = match ftype {
                    1 => "文本",
                    2 => "图片",
                    5 => "文章",
                    19 => "名片",
                    20 => "视频",
                    _ => "其他",
                };
                // 安全截断（按 Unicode 字符而非字节）
                let preview: String = content.chars().take(100).collect();
                let preview = if content.chars().count() > 100 {
                    format!("{}...", preview)
                } else {
                    preview
                };
                // WeChat 部分版本的 update_time 为毫秒，10位以上判定为毫秒后转秒
                let ts_secs = if ts > 9_999_999_999 { ts / 1000 } else { ts };
                json!({
                    "id": local_id,
                    "type": type_str,
                    "type_num": ftype,
                    "time": fmt_time(ts_secs, "%Y-%m-%d %H:%M"),
                    "timestamp": ts_secs,
                    "preview": preview,
                    "from": fromusr,
                    "chat": chatname,
                })
            })
            .collect();

        Ok::<_, anyhow::Error>(rows)
    })
    .await??;

    Ok(json!({
        "count": rows.len(),
        "items": rows,
    }))
}
