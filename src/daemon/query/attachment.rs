use super::*;

pub async fn q_attachments(
    db: &DbCache,
    names: &Names,
    chat: &str,
    kinds: Option<Vec<String>>,
    limit: usize,
    offset: usize,
    since: Option<i64>,
    until: Option<i64>,
    _with_meta: bool,
    _debug_source: bool,
) -> Result<Value> {
    use crate::attachment::{AttachmentId, AttachmentKind};

    let username =
        resolve_username(chat, names).with_context(|| format!("找不到联系人: {}", chat))?;
    let display = names.display(&username);
    let chat_type = chat_type_of(&username, names);
    let is_group = chat_type == "group";

    let kind_filters = parse_attachment_kinds(kinds.as_deref())?;
    if kind_filters.is_empty() {
        anyhow::bail!("kinds 为空 — 当前至少传一种 image");
    }
    let lo32_types: Vec<i64> = kind_filters.iter().map(|(_, t)| *t).collect();
    let type_to_kind: HashMap<i64, AttachmentKind> =
        kind_filters.iter().map(|(k, t)| (*t, *k)).collect();

    let tables = find_msg_tables(db, names, &username).await?;
    if tables.is_empty() {
        anyhow::bail!("找不到 {} 的消息记录", display);
    }

    let mut all_rows: Vec<(i64, i64, i64, String, String)> = Vec::new();
    for (db_path, table_name) in tables {
        let path = db_path.clone();
        let tname = table_name.clone();
        let uname = username.clone();
        let names_map = names.map.clone();
        let lo32_types2 = lo32_types.clone();

        let rows: Vec<(i64, i64, i64, String, String)> = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&path)?;
            ensure_create_time_index(&conn, &tname);
            let id2u = load_id2u(&conn);
            let placeholders = lo32_types2
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let mut clauses: Vec<String> =
                vec![format!("(local_type & 4294967295) IN ({})", placeholders)];
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = lo32_types2
                .iter()
                .map(|t| Box::new(*t) as Box<dyn rusqlite::types::ToSql>)
                .collect();
            if let Some(s) = since {
                clauses.push("create_time >= ?".into());
                params.push(Box::new(s));
            }
            if let Some(u) = until {
                clauses.push("create_time <= ?".into());
                params.push(Box::new(u));
            }
            let where_clause = format!("WHERE {}", clauses.join(" AND "));
            let sql = format!(
                "SELECT local_id, local_type, create_time, real_sender_id,
                            message_content, WCDB_CT_message_content
                     FROM [{}] {} ORDER BY create_time DESC LIMIT ?",
                tname, where_clause
            );
            params.push(Box::new(((offset + limit).max(limit) * 2) as i64));

            let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();
            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<(i64, i64, i64, String, String)> = stmt
                .query_map(params_ref.as_slice(), |row| {
                    let local_id: i64 = row.get(0)?;
                    let raw_type: i64 = row.get(1)?;
                    let lo32 = (raw_type as u64 & 0xFFFFFFFF) as i64;
                    let ts: i64 = row.get(2)?;
                    let real_sender_id: i64 = row.get(3)?;
                    let content_bytes = get_content_bytes(row, 4);
                    let ct: i64 = row.get::<_, i64>(5).unwrap_or(0);
                    let content = decompress_message(&content_bytes, ct);
                    let sender = if is_group {
                        sender_label(real_sender_id, &content, true, &uname, &id2u, &names_map)
                    } else {
                        String::new()
                    };
                    let sender_uname = if is_group {
                        sender_username(real_sender_id, &content, true, &uname, &id2u)
                    } else {
                        String::new()
                    };
                    Ok((local_id, lo32, ts, sender, sender_uname))
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok::<_, anyhow::Error>(rows)
        })
        .await??;

        all_rows.extend(rows);
    }

    all_rows.sort_by_key(|r| std::cmp::Reverse(r.2));
    let paged: Vec<_> = all_rows.into_iter().skip(offset).take(limit).collect();

    let mut results: Vec<Value> = Vec::with_capacity(paged.len());
    for (local_id, lo32, ts, sender, sender_uname) in paged {
        let kind = type_to_kind
            .get(&lo32)
            .copied()
            .unwrap_or(crate::attachment::AttachmentKind::Image);
        let id = AttachmentId {
            v: 1,
            chat: username.clone(),
            local_id,
            create_time: ts,
            kind,
            db: None,
        };
        let mut row = json!({
            "attachment_id": id.encode()?,
            "kind": kind.as_str(),
            "type": fmt_type(lo32),
            "local_id": local_id,
            "timestamp": ts,
            "time": fmt_time(ts, "%Y-%m-%d %H:%M"),
        });
        if is_group && !sender.is_empty() {
            row["sender"] = Value::String(sender);
        }
        add_sender_identity(&mut row, is_group, &sender_uname, &names.map);
        results.push(row);
    }

    Ok(json!({
        "chat": display,
        "username": username,
        "is_group": is_group,
        "chat_type": chat_type,
        "count": results.len(),
        "attachments": results,
    }))
}

pub async fn q_extract(
    db: &DbCache,
    _names: &Names,
    attachment_id: &str,
    output: &str,
    overwrite: bool,
) -> Result<Value> {
    use crate::attachment::{
        attachment_id::AttachmentId,
        decoder::{self, V2KeyMaterial},
        image_key, resolver,
    };

    let id = AttachmentId::decode(attachment_id)
        .context("解析 attachment_id 失败（不是合法 base64url(json)？）")?;

    let output_path = std::path::PathBuf::from(output);
    if output_path.exists() && !overwrite {
        anyhow::bail!(
            "目标已存在：{}（加 --overwrite 覆盖）",
            output_path.display()
        );
    }
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("创建输出目录失败：{}", parent.display()))?;
        }
    }

    let resource_path = db
        .get("message/message_resource.db")
        .await?
        .context("无法解密 message_resource.db（请确认 all_keys.json 包含该 DB 的密钥）")?;
    let wxchat_base = db
        .db_dir()
        .parent()
        .ok_or_else(|| anyhow::anyhow!("db_dir 没有 parent，无法推断 xwechat_files 根目录"))?
        .to_path_buf();
    let attach_root = resolver::attach_root_for(&wxchat_base);

    let id_for_task = id.clone();
    let resource_path2 = resource_path.clone();
    let attach_root2 = attach_root.clone();
    let wxchat_base2 = wxchat_base.clone();
    let output_path2 = output_path.clone();

    let report: Value = tokio::task::spawn_blocking(move || -> Result<Value> {
        let resolved = resolver::resolve_blocking(&id_for_task, &resource_path2, &attach_root2)?;
        let dat_bytes = std::fs::read(&resolved.dat_path)
            .with_context(|| format!("读取 .dat 失败：{}", resolved.dat_path.display()))?;

        let provider = image_key::default_provider();
        let key_material = if let Some(p) = provider.as_ref() {
            let wxid = wxchat_base2
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            if wxid.is_empty() {
                None
            } else {
                match p.get_key(&wxid) {
                    Ok(km) => Some(km),
                    Err(e) => {
                        eprintln!(
                            "[extract] image key 提取失败 (wxid={}): {} — V2 文件将无法解码",
                            wxid, e
                        );
                        None
                    }
                }
            }
        } else {
            None
        };
        let v2_key = match key_material.as_ref() {
            Some(km) => V2KeyMaterial {
                aes_key: Some(&km.aes_key),
                xor_key: km.xor_key,
            },
            None => V2KeyMaterial::default(),
        };

        let decoded = decoder::dispatch(&dat_bytes, v2_key)?;
        std::fs::write(&output_path2, &decoded.data)
            .with_context(|| format!("写出文件失败：{}", output_path2.display()))?;

        Ok(json!({
            "kind": id_for_task.kind.as_str(),
            "md5": resolved.md5,
            "dat_path": resolved.dat_path.display().to_string(),
            "dat_size": resolved.size,
            "output": output_path2.display().to_string(),
            "output_size": decoded.data.len(),
            "format": decoded.format,
            "decoder": decoded.decoder,
        }))
    })
    .await??;

    Ok(report)
}

fn parse_attachment_kinds(
    kinds: Option<&[String]>,
) -> Result<Vec<(crate::attachment::AttachmentKind, i64)>> {
    use crate::attachment::AttachmentKind;
    let raw = kinds.unwrap_or(&[]);
    if raw.is_empty() {
        return Ok(vec![(AttachmentKind::Image, 3)]);
    }
    let mut out: Vec<(AttachmentKind, i64)> = Vec::with_capacity(raw.len());
    let mut seen = std::collections::HashSet::<&'static str>::new();
    for k in raw {
        let (kind, t): (AttachmentKind, i64) = match k.to_ascii_lowercase().as_str() {
            "image" | "img" => (AttachmentKind::Image, 3),
            "voice" | "audio" | "video" | "file" => {
                anyhow::bail!(
                    "当前只支持 image 提取；video/file/voice 的资源路径与 decoder 还没接通"
                )
            }
            other => anyhow::bail!("未知附件类型：{}（当前仅支持 image）", other),
        };
        if seen.insert(kind.as_str()) {
            out.push((kind, t));
        }
    }
    Ok(out)
}
