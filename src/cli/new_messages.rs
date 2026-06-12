use super::output::{print_value, resolve};
use super::transport;
use crate::ipc::{NewMessageCursor, Request};
use anyhow::Result;
use std::collections::HashMap;

fn state_file() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".wx-cli")
        .join("last_check.json")
}

/// 加载上次的 per-session 游标快照
/// 格式：{ "sessions": { "username": {"create_time": timestamp, "local_id": id}, ... } }
/// 兼容旧格式：{ "sessions": { "username": timestamp, ... } }
/// 旧格式（只有 timestamp 字段）直接丢弃，重新全量获取
fn load_state() -> Option<HashMap<String, NewMessageCursor>> {
    let data = std::fs::read_to_string(state_file()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;
    // 旧格式（只有 timestamp 字段）没有 sessions key → 返回 None 触发首次运行逻辑
    let map: HashMap<String, NewMessageCursor> =
        serde_json::from_value(v.get("sessions")?.clone()).ok()?;
    // 空 map 也是合法状态（账号无任何会话），返回 Some(empty) 而非 None
    // 这样不会误触发全量历史拉取
    Some(map)
}

fn save_state(new_state: &HashMap<String, NewMessageCursor>) -> Result<()> {
    let path = state_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &path,
        serde_json::to_string(&serde_json::json!({ "sessions": new_state }))?,
    )?;
    Ok(())
}

pub fn cmd_new_messages(limit: usize, json: bool) -> Result<()> {
    let state = load_state();
    let resp = transport::send(Request::NewMessages { state, limit })?;

    // 保存 daemon 返回的 new_state
    if let Some(obj) = resp.data.get("new_state").and_then(|v| v.as_object()) {
        let map: HashMap<String, NewMessageCursor> =
            serde_json::from_value(serde_json::Value::Object(obj.clone())).unwrap_or_default();
        if !map.is_empty() {
            let _ = save_state(&map);
        }
    }

    let messages = resp
        .data
        .get("messages")
        .cloned()
        .unwrap_or(serde_json::Value::Array(vec![]));
    print_value(&messages, &resolve(json))
}
