use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::config;
use crate::daemon::{self, cache::DbCache, query, voice_asr};

use super::history::{parse_time, parse_time_end};
use super::output::{print_value, resolve};

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

    eprintln!(
        "[asr-backfill] 开始预转写 {} 条未缓存语音，预计音频时长 {:.2} 分钟...",
        estimate.pending_messages,
        estimate.pending_seconds / 60.0
    );

    let mut transcribed_now = 0usize;
    let mut failed = 0usize;
    let mut error_examples = Vec::new();

    for (idx, target) in estimate.pending_targets.iter().enumerate() {
        match voice_asr::transcribe_voice_message(
            &db,
            &target.chat_username,
            target.local_id,
            target.create_time,
        )
        .await
        {
            Ok(_) => {
                transcribed_now += 1;
            }
            Err(err) => {
                failed += 1;
                if error_examples.len() < 20 {
                    error_examples.push(json!({
                        "chat_username": target.chat_username,
                        "local_id": target.local_id,
                        "timestamp": target.create_time,
                        "error": err.to_string(),
                    }));
                }
            }
        }

        let done = idx + 1;
        if done == 1 || done % 25 == 0 || done == estimate.pending_messages {
            eprintln!(
                "[asr-backfill] 进度 {}/{}，成功 {}，失败 {}",
                done, estimate.pending_messages, transcribed_now, failed
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
            json!(estimate.cached_messages + transcribed_now),
        );
        obj.insert(
            "pending_after_run".into(),
            json!(estimate.pending_messages.saturating_sub(transcribed_now)),
        );
    }

    Ok(summary)
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
