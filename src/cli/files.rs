use anyhow::Result;

use super::history::{parse_time, parse_time_end};
use super::output::{print_value, resolve};
use super::transport;
use crate::ipc::Request;

pub fn cmd_files(
    file_type: Option<String>,
    limit: Option<usize>,
    since: Option<String>,
    until: Option<String>,
    json: bool,
) -> Result<()> {
    let since_ts = since.as_deref().map(parse_file_time).transpose()?;
    let until_ts = until.as_deref().map(parse_file_time_end).transpose()?;

    let resp = transport::send(Request::Files {
        file_type,
        limit,
        since: since_ts,
        until: until_ts,
    })?;
    print_value(&resp.data, &resolve(json))
}

fn parse_file_time(s: &str) -> Result<i64> {
    parse_unix_time(s).unwrap_or_else(|| parse_time(s))
}

fn parse_file_time_end(s: &str) -> Result<i64> {
    parse_unix_time(s).unwrap_or_else(|| parse_time_end(s))
}

fn parse_unix_time(s: &str) -> Option<Result<i64>> {
    if s.chars().all(|ch| ch.is_ascii_digit()) {
        Some(s.parse::<i64>().map_err(Into::into))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unix_timestamp_for_files_time_filters() {
        assert_eq!(parse_file_time("1700000000").unwrap(), 1_700_000_000);
        assert_eq!(parse_file_time_end("1700000001").unwrap(), 1_700_000_001);
    }
}
