use super::*;

pub(super) fn extract_xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)?;
    let content_start = start + open.len();
    let end = xml[content_start..].find(&close)?;
    let raw = xml[content_start..content_start + end].trim();
    // 剥掉 CDATA 包装（公众号链接的 title/des 常见）
    let stripped = raw
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(raw);
    Some(stripped.trim().to_string())
}

pub(super) fn extract_xml_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let open = format!("<{}", tag);
    let start = xml.find(&open)?;
    let tag_end = start + xml[start..].find('>')?;
    let attr_pat = format!(r#"{}=""#, attr);
    let attr_start = start + xml[start..tag_end].find(&attr_pat)? + attr_pat.len();
    let attr_end = attr_start + xml[attr_start..tag_end].find('"')?;
    let value = xml[attr_start..attr_end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

pub(super) fn unescape_html(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn xml_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"</?[^>]+>").unwrap())
}

pub(super) fn clean_inline_text(s: &str) -> String {
    let unescaped = unescape_html(s);
    let without_tags = xml_tag_re().replace_all(&unescaped, "");
    without_tags
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn extract_cdata(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)?;
    let inner = xml[start..start + end].trim();
    if inner.starts_with("<![CDATA[") {
        let body = &inner[9..];
        let content = if body.as_bytes().ends_with(b"]]>") {
            &body[..body.len() - 3]
        } else if body.as_bytes().ends_with(b"]]") {
            &body[..body.len() - 2]
        } else {
            body
        };
        let content = content.trim();
        if content.is_empty() {
            None
        } else {
            Some(content.to_string())
        }
    } else if inner.is_empty() {
        None
    } else {
        Some(unescape_html(inner))
    }
}
