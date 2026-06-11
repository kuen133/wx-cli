use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::task::JoinSet;

use crate::config;
use crate::daemon::{self, cache::DbCache, query, voice_asr};

use super::history::{parse_time, parse_time_end};
use super::output::{print_value, resolve};

const DEFAULT_ASR_CONCURRENCY: usize = 4;
const MAX_ASR_CONCURRENCY: usize = 16;
const ASR_PROGRESS_INTERVAL: usize = 25;

pub fn cmd_asr_backfill(
    limit: Option<usize>,
    since: Option<String>,
    until: Option<String>,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    let since_ts = since.as_deref().map(parse_time).transpose()?;
    let until_ts = until.as_deref().map(parse_time_end).transpose()?;

    let rt = tokio::runtime::Runtime::new().context("无法创建 tokio runtime")?;
    let value = rt.block_on(async { run_backfill(limit, since_ts, until_ts, dry_run).await })?;
    print_value(&value, &resolve(json_output))
}

async fn run_backfill(
    limit: Option<usize>,
    since: Option<i64>,
    until: Option<i64>,
    dry_run: bool,
) -> Result<Value> {
    let (db, names) = load_runtime_context().await?;

    eprintln!("[asr-backfill] 扫描历史语音消息...");
    let targets = voice_asr::list_history_voice_targets(&db, &names, since, until, limit).await?;
    eprintln!(
        "[asr-backfill] 找到 {} 条语音消息，正在核对缓存并估算剩余成本...",
        targets.len()
    );

    let estimate = voice_asr::estimate_backfill(&db, targets).await?;
    let mut summary = summary_value(&estimate, dry_run, since, until, limit);
    if dry_run || estimate.pending_messages == 0 {
        return Ok(summary);
    }

    let concurrency = asr_concurrency();
    eprintln!(
        "[asr-backfill] 开始预转写 {} 条未缓存语音，预计音频时长 {:.2} 分钟，并发度 {}...",
        estimate.pending_messages,
        estimate.pending_seconds / 60.0,
        concurrency
    );

    let mut transcribed_now = 0usize;
    let mut failed = 0usize;
    let mut error_examples = Vec::new();

    let cached_messages = estimate.cached_messages;
    let pending_messages = estimate.pending_messages;
    let mut pending_targets = estimate.pending_targets.into_iter();
    let db = Arc::new(db);
    let mut join_set = JoinSet::new();

    for _ in 0..concurrency {
        let Some(target) = pending_targets.next() else {
            break;
        };
        spawn_transcribe_task(&mut join_set, db.clone(), target);
    }

    let mut done = 0usize;
    while let Some(joined) = join_set.join_next().await {
        done += 1;
        match joined {
            Ok((target, Ok(()))) => {
                transcribed_now += 1;
                drop(target);
            }
            Ok((target, Err(err))) => {
                failed += 1;
                push_error_example(&mut error_examples, &target, err.to_string());
            }
            Err(err) => {
                failed += 1;
                if error_examples.len() < 20 {
                    error_examples.push(json!({
                        "error": format!("转写任务 join 失败: {}", err),
                    }));
                }
            }
        }

        if let Some(target) = pending_targets.next() {
            spawn_transcribe_task(&mut join_set, db.clone(), target);
        }

        if done == 1 || done % ASR_PROGRESS_INTERVAL == 0 || done == pending_messages {
            eprintln!(
                "[asr-backfill] 进度 {}/{}，成功 {}，失败 {}",
                done, pending_messages, transcribed_now, failed
            );
        }
    }

    if let Some(obj) = summary.as_object_mut() {
        obj.insert("dry_run".into(), Value::Bool(false));
        obj.insert("transcribed_now".into(), json!(transcribed_now));
        obj.insert("failed".into(), json!(failed));
        obj.insert("error_examples".into(), Value::Array(error_examples));
        obj.insert(
            "cache_after_run".into(),
            json!(cached_messages + transcribed_now),
        );
        obj.insert(
            "pending_after_run".into(),
            json!(pending_messages.saturating_sub(transcribed_now)),
        );
    }

    Ok(summary)
}

fn asr_concurrency() -> usize {
    let raw = std::env::var("WX_ASR_CONCURRENCY").ok();
    parse_asr_concurrency(raw.as_deref())
}

fn parse_asr_concurrency(raw: Option<&str>) -> usize {
    raw.and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(MAX_ASR_CONCURRENCY))
        .unwrap_or(DEFAULT_ASR_CONCURRENCY)
}

fn spawn_transcribe_task(
    join_set: &mut JoinSet<(voice_asr::VoiceBackfillTarget, Result<()>)>,
    db: Arc<DbCache>,
    target: voice_asr::VoiceBackfillTarget,
) {
    join_set.spawn(async move {
        let result = voice_asr::transcribe_voice_message(
            &db,
            &target.chat_username,
            target.local_id,
            target.create_time,
        )
        .await
        .map(|_| ());
        (target, result)
    });
}

fn push_error_example(
    error_examples: &mut Vec<Value>,
    target: &voice_asr::VoiceBackfillTarget,
    error: String,
) {
    if error_examples.len() < 20 {
        error_examples.push(json!({
            "chat_username": target.chat_username,
            "local_id": target.local_id,
            "timestamp": target.create_time,
            "error": error,
        }));
    }
}

async fn load_runtime_context() -> Result<(DbCache, query::Names)> {
    let cfg = config::load_config()?;
    let keys_content = tokio::fs::read_to_string(&cfg.keys_file)
        .await
        .with_context(|| format!("读取密钥文件 {:?} 失败", cfg.keys_file))?;
    let keys_raw: serde_json::Value = serde_json::from_str(&keys_content)?;
    let all_keys = daemon::extract_keys(&keys_raw);

    let msg_db_keys: Vec<String> = all_keys
        .keys()
        .filter(|key| {
            let key = key.replace('\\', "/");
            key.ends_with(".db")
                && !key.contains("_fts")
                && !key.contains("_resource")
                && (key.contains("message/message_") || key.contains("message/biz_message_"))
        })
        .cloned()
        .collect();

    let db = DbCache::new(cfg.db_dir.clone(), all_keys).await?;
    let mut names = query::load_names(&db).await?;
    names.msg_db_keys = msg_db_keys;
    Ok((db, names))
}

fn summary_value(
    estimate: &voice_asr::VoiceBackfillEstimate,
    dry_run: bool,
    since: Option<i64>,
    until: Option<i64>,
    limit: Option<usize>,
) -> Value {
    json!({
        "dry_run": dry_run,
        "scope": {
            "since": since,
            "until": until,
            "limit": limit,
        },
        "total_messages": estimate.total_messages,
        "cached_messages": estimate.cached_messages,
        "pending_messages": estimate.pending_messages,
        "unresolved_messages": estimate.unresolved_messages,
        "pending_audio_seconds": round4(estimate.pending_seconds),
        "pending_audio_minutes": round4(estimate.pending_seconds / 60.0),
        "estimated_cost_usd": round4(estimate.pending_cost_usd),
        "price_per_second_usd": estimate.price_per_second_usd,
    })
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_asr_concurrency_defaults_and_caps_env_values() {
        assert_eq!(parse_asr_concurrency(None), DEFAULT_ASR_CONCURRENCY);
        assert_eq!(parse_asr_concurrency(Some("")), DEFAULT_ASR_CONCURRENCY);
        assert_eq!(parse_asr_concurrency(Some("0")), DEFAULT_ASR_CONCURRENCY);
        assert_eq!(
            parse_asr_concurrency(Some("not-a-number")),
            DEFAULT_ASR_CONCURRENCY
        );
        assert_eq!(parse_asr_concurrency(Some("2")), 2);
        assert_eq!(parse_asr_concurrency(Some("128")), MAX_ASR_CONCURRENCY);
    }

    #[test]
    fn summary_value_keeps_dry_run_from_reporting_real_transcribes() {
        let estimate = voice_asr::VoiceBackfillEstimate {
            total_messages: 3,
            cached_messages: 1,
            pending_messages: 2,
            unresolved_messages: 0,
            pending_seconds: 12.34567,
            pending_cost_usd: 0.01234,
            price_per_second_usd: 0.001,
            pending_targets: Vec::new(),
        };

        let value = summary_value(&estimate, true, Some(10), Some(20), Some(30));

        assert_eq!(value["dry_run"], true);
        assert_eq!(value["pending_messages"], 2);
        assert!(value.get("transcribed_now").is_none());
        assert!(value.get("failed").is_none());
    }
}
