use super::*;

pub(super) fn get_content_bytes(row: &rusqlite::Row<'_>, idx: usize) -> Vec<u8> {
    // 先尝试 BLOB，再 fallback 到 TEXT→bytes
    row.get::<_, Vec<u8>>(idx)
        .or_else(|_| row.get::<_, String>(idx).map(|s| s.into_bytes()))
        .unwrap_or_default()
}

pub(crate) fn decompress_message(data: &[u8], ct: i64) -> String {
    if ct == 4 && !data.is_empty() {
        // zstd 压缩
        if let Ok(dec) = zstd::decode_all(data) {
            return String::from_utf8_lossy(&dec).into_owned();
        }
    }
    String::from_utf8_lossy(data).into_owned()
}

pub(super) fn decompress_or_str(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }
    // 尝试 zstd 解压
    if let Ok(dec) = zstd::decode_all(data) {
        if let Ok(s) = String::from_utf8(dec) {
            return s;
        }
    }
    String::from_utf8_lossy(data).into_owned()
}

pub(super) fn strip_group_prefix(s: &str) -> String {
    if s.contains(":\n") {
        s.splitn(2, ":\n").nth(1).unwrap_or(s).to_string()
    } else {
        s.to_string()
    }
}

pub(crate) fn fmt_type(t: i64) -> String {
    let base = (t as u64 & 0xFFFFFFFF) as i64;
    match base {
        1 => "文本".into(),
        3 => "图片".into(),
        34 => "语音".into(),
        42 => "名片".into(),
        43 => "视频".into(),
        47 => "表情".into(),
        48 => "位置".into(),
        49 => "链接/文件".into(),
        50 => "通话".into(),
        10000 => "系统".into(),
        10002 => "撤回".into(),
        _ => format!("type={}", base),
    }
}

pub(super) fn fmt_content(local_id: i64, local_type: i64, content: &str, is_group: bool) -> String {
    let base = (local_type as u64 & 0xFFFFFFFF) as i64;
    match base {
        3 => return format!("[图片] local_id={}", local_id),
        34 => return "[语音]".into(),
        43 => return "[视频]".into(),
        47 => return "[表情]".into(),
        50 => return "[通话]".into(),
        10000 => return parse_sysmsg(content).unwrap_or_else(|| "[系统消息]".into()),
        10002 => return parse_revoke(content).unwrap_or_else(|| "[撤回了一条消息]".into()),
        _ => {}
    }

    let text = if is_group && content.contains(":\n") {
        content.splitn(2, ":\n").nth(1).unwrap_or(content)
    } else {
        content
    };

    if base == 42 {
        return extract_xml_attr(text, "msg", "nickname")
            .map(|nickname| format!("[名片] {}", nickname))
            .unwrap_or_else(|| "[名片]".into());
    }

    if base == 48 {
        return extract_xml_attr(text, "location", "poiname")
            .or_else(|| extract_xml_attr(text, "location", "label"))
            .map(|name| format!("[位置] {}", name))
            .unwrap_or_else(|| "[位置]".into());
    }

    if base == 49 && text.contains("<appmsg") {
        if let Some(parsed) = parse_appmsg(text) {
            return parsed;
        }
    }
    text.to_string()
}

/// 解析撤回消息 XML，提取被撤回的内容摘要
/// `<sysmsg type="revokemsg"><revokemsg><content>...</content></revokemsg></sysmsg>`
pub(super) fn parse_revoke(xml: &str) -> Option<String> {
    let inner = extract_xml_text(xml, "content")?;
    // 有时 content 是 "xxx recalled a message" 英文，有时是中文
    if inner.is_empty() {
        return Some("[撤回了一条消息]".into());
    }
    // 尝试简化：如果是 XML 格式的撤回内容，直接显示摘要
    Some(format!(
        "[撤回] {}",
        inner.chars().take(30).collect::<String>()
    ))
}

/// 解析系统消息 XML（群通知等）
pub(super) fn parse_sysmsg(xml: &str) -> Option<String> {
    // 常见格式：<sysmsg type="...">...</sysmsg>
    // 尝试提取 content 标签
    if let Some(s) = extract_xml_text(xml, "content") {
        let cleaned = clean_inline_text(&s);
        if !cleaned.is_empty() {
            return Some(format!("[系统] {}", truncate_chars(&cleaned, 50)));
        }
    }
    // 纯文本系统消息（无 XML）
    if !xml.starts_with('<') {
        let cleaned = clean_inline_text(xml);
        if !cleaned.is_empty() {
            return Some(format!("[系统] {}", truncate_chars(&cleaned, 50)));
        }
    }
    Some("[系统消息]".into())
}

pub(super) fn parse_appmsg(text: &str) -> Option<String> {
    // 简单 XML 解析，避免引入重量级 XML 库（或直接用 minidom）
    // 这里用基本字符串搜索实现
    if let Some(transfer) = parse_transfer_appmsg_xml(text) {
        return Some(match transfer.amount_cents {
            Some(amount_cents) => format!("[转账] {}", format_cents_with_symbol(amount_cents)),
            None => "[转账] 微信转账".into(),
        });
    }

    let title = extract_xml_text(text, "title")?;
    let atype = extract_xml_text(text, "type").unwrap_or_default();
    match atype.as_str() {
        "6" => Some(if !title.is_empty() {
            format!("[文件] {}", title)
        } else {
            "[文件]".into()
        }),
        "57" => {
            let ref_content = extract_xml_text(text, "content")
                .map(|s| {
                    // content 可能是 HTML 转义的 XML（被引用的消息是 appmsg 时）
                    let unescaped = unescape_html(&s);
                    // 如果解转义后是 XML，尝试递归解析
                    if unescaped.contains("<appmsg") {
                        if let Some(parsed) = parse_appmsg(&unescaped) {
                            return parsed;
                        }
                    }
                    let s: String = unescaped.split_whitespace().collect::<Vec<_>>().join(" ");
                    if s.chars().count() > 40 {
                        format!("{}...", s.chars().take(40).collect::<String>())
                    } else {
                        s
                    }
                })
                .unwrap_or_default();
            let quote = if !title.is_empty() {
                format!("[引用] {}", title)
            } else {
                "[引用]".into()
            };
            if !ref_content.is_empty() {
                Some(format!("{}\n  \u{21b3} {}", quote, ref_content))
            } else {
                Some(quote)
            }
        }
        "33" | "36" | "44" => Some(if !title.is_empty() {
            format!("[小程序] {}", title)
        } else {
            "[小程序]".into()
        }),
        _ => Some(if !title.is_empty() {
            format!("[链接] {}", title)
        } else {
            "[链接/文件]".into()
        }),
    }
}

pub(super) fn truncate_chars(s: &str, max_chars: usize) -> String {
    let truncated: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        format!("{}...", truncated)
    } else {
        truncated
    }
}

pub(super) fn fmt_time(ts: i64, fmt: &str) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.format(fmt).to_string())
        .unwrap_or_else(|| ts.to_string())
}
