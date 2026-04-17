use anyhow::Result;
use crate::ipc::Request;
use super::transport;
use super::output::{resolve, print_value};
use super::history::{parse_time, parse_time_end};

pub fn cmd_friend_requests(
    limit: usize,
    since: Option<String>,
    until: Option<String>,
    direction: Option<String>,
    json: bool,
) -> Result<()> {
    let since_ts = since.as_deref().map(parse_time).transpose()?;
    let until_ts = until.as_deref().map(parse_time_end).transpose()?;
    let req = Request::FriendRequests { limit, since: since_ts, until: until_ts, direction };
    let resp = transport::send(req)?;
    let data = resp.data.get("requests")
        .cloned()
        .unwrap_or(serde_json::Value::Array(vec![]));
    print_value(&data, &resolve(json))
}
