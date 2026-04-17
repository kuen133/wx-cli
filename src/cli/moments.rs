use anyhow::Result;
use crate::ipc::Request;
use super::transport;
use super::output::{resolve, print_value};
use super::history::{parse_time, parse_time_end};

pub fn cmd_moments(
    limit: usize,
    user: Option<String>,
    since: Option<String>,
    until: Option<String>,
    query: Option<String>,
    with_media: bool,
    json: bool,
) -> Result<()> {
    let since_ts = since.as_deref().map(parse_time).transpose()?;
    let until_ts = until.as_deref().map(parse_time_end).transpose()?;

    let req = Request::Moments {
        limit, user, since: since_ts, until: until_ts,
        query, with_media,
    };
    let resp = transport::send(req)?;
    let data = resp.data.get("moments")
        .cloned()
        .unwrap_or(serde_json::Value::Array(vec![]));
    print_value(&data, &resolve(json))
}
