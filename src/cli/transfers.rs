use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate, TimeZone};
use serde_json::json;

use crate::ipc::Request;

use super::history::{parse_time, parse_time_end};
use super::output::{print_value, resolve};
use super::transport;

pub fn cmd_transfers(
    chat: String,
    month: Option<String>,
    since: Option<String>,
    until: Option<String>,
    summary_only: bool,
    json: bool,
) -> Result<()> {
    let (since_ts, until_ts) = if let Some(month) = month.as_deref() {
        parse_month_range(month)?
    } else {
        (
            since.as_deref().map(parse_time).transpose()?,
            until.as_deref().map(parse_time_end).transpose()?,
        )
    };

    let resp = transport::send(Request::Transfers {
        chat,
        since: since_ts,
        until: until_ts,
    })?;

    let value = if summary_only {
        json!({
            "chat": resp.data.get("chat").cloned().unwrap_or(serde_json::Value::Null),
            "username": resp.data.get("username").cloned().unwrap_or(serde_json::Value::Null),
            "summary": resp.data.get("summary").cloned().unwrap_or(serde_json::Value::Null),
            "monthly_rows": resp.data.get("monthly_rows").cloned().unwrap_or_else(|| json!([])),
        })
    } else {
        resp.data
    };

    print_value(&value, &resolve(json))
}

fn parse_month_range(s: &str) -> Result<(Option<i64>, Option<i64>)> {
    let (year, month) = if matches!(s, "this" | "current") {
        let now = Local::now();
        (now.year(), now.month())
    } else {
        let with_day = format!("{}-01", s);
        let first_day = NaiveDate::parse_from_str(&with_day, "%Y-%m-%d")
            .with_context(|| format!("无法解析月份 '{}'，支持 YYYY-MM / this", s))?;
        (first_day.year(), first_day.month())
    };

    let start = NaiveDate::from_ymd_opt(year, month, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .context("无法构造月份起始时间")?;
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let next = NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .context("无法构造下个月起始时间")?;

    let since = Local.from_local_datetime(&start).single()
        .map(|d| d.timestamp())
        .ok_or_else(|| anyhow::anyhow!("本地时间歧义: {}-{:02}", year, month))?;
    let until = Local.from_local_datetime(&next).single()
        .map(|d| d.timestamp() - 1)
        .ok_or_else(|| anyhow::anyhow!("本地时间歧义: {}-{:02}", next_year, next_month))?;

    Ok((Some(since), Some(until)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_month_range_handles_explicit_month() {
        let (since, until) = parse_month_range("2026-04").expect("month should parse");
        assert!(since.is_some());
        assert!(until.is_some());
        assert!(until.unwrap() > since.unwrap());
    }

    #[test]
    fn parse_month_range_rejects_invalid_month() {
        assert!(parse_month_range("2026-13").is_err());
    }
}
