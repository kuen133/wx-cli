use anyhow::{Context, Result};
use chrono::Local;
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::config;

use super::query::Names;
use super::cache::DbCache;

const DEFAULT_ASR_LANGUAGE: &str = "zh";
const DEFAULT_ASR_MODEL: &str = "qwen3-asr-flash";
const DEFAULT_SILK_SAMPLE_RATE: &str = "24000";
const DEFAULT_MAINLAND_PRICE_PER_SECOND_USD: f64 = 0.000032;

pub struct VoiceTranscript {
    pub text: String,
    pub model: String,
    pub cached: bool,
}

#[derive(Debug, Clone)]
pub struct VoiceBackfillTarget {
    pub chat_username: String,
    pub local_id: i64,
    pub create_time: i64,
    pub duration_secs: Option<f64>,
}

pub struct VoiceBackfillEstimate {
    pub total_messages: usize,
    pub cached_messages: usize,
    pub pending_messages: usize,
    pub unresolved_messages: usize,
    pub pending_seconds: f64,
    pub pending_cost_usd: f64,
    pub price_per_second_usd: f64,
    pub pending_targets: Vec<VoiceBackfillTarget>,
}

pub async fn enrich_history_messages(
    db: &DbCache,
    chat_username: &str,
    messages: &mut [Value],
    transcribe_missing: bool,
) {
    for message in messages.iter_mut() {
        let is_voice = message.get("type").and_then(Value::as_str) == Some("语音");
        if !is_voice {
            continue;
        }

        let local_id = match message.get("local_id").and_then(Value::as_i64) {
            Some(v) => v,
            None => continue,
        };
        let timestamp = message
            .get("timestamp")
            .and_then(Value::as_i64)
            .unwrap_or(0);

        if transcribe_missing {
            match transcribe_voice_message(db, chat_username, local_id, timestamp).await {
                Ok(transcript) => annotate_message_with_transcript(message, transcript),
                Err(err) => annotate_message_with_error(message, err),
            }
            continue;
        }

        match cached_transcript(chat_username, local_id, timestamp).await {
            Ok(Some(transcript)) => annotate_message_with_transcript(message, transcript),
            Ok(None) => {}
            Err(err) => annotate_message_with_error(message, err),
        }
    }
}

pub async fn list_history_voice_targets(
    db: &DbCache,
    names: &Names,
    since: Option<i64>,
    until: Option<i64>,
    limit: Option<usize>,
) -> Result<Vec<VoiceBackfillTarget>> {
    let msg_db_keys = names.msg_db_keys.clone();
    let md5_to_uname = names.md5_to_uname.clone();

    let mut db_paths: Vec<PathBuf> = Vec::new();
    for rel_key in &msg_db_keys {
        if let Some(path) = db.get(rel_key).await? {
            db_paths.push(path);
        }
    }

    tokio::task::spawn_blocking(move || -> Result<Vec<VoiceBackfillTarget>> {
        let mut result = Vec::new();
        let mut dedup = HashSet::new();

        for path in &db_paths {
            let conn = Connection::open(path)?;
            let table_names: Vec<String> = conn
                .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'")?
                .query_map([], |row| row.get(0))?
                .filter_map(|row| row.ok())
                .collect();

            for table in &table_names {
                if !msg_table_re().is_match(table) {
                    continue;
                }

                let hash = &table[4..];
                let Some(chat_username) = md5_to_uname.get(hash).cloned() else {
                    continue;
                };

                let mut clauses = vec!["(local_type & 4294967295) = 34".to_string()];
                let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                if let Some(since) = since {
                    clauses.push("create_time >= ?".into());
                    params.push(Box::new(since));
                }
                if let Some(until) = until {
                    clauses.push("create_time <= ?".into());
                    params.push(Box::new(until));
                }

                let sql = format!(
                    "SELECT local_id, create_time
                     FROM [{}]
                     WHERE {}
                     ORDER BY create_time ASC, local_id ASC",
                    table,
                    clauses.join(" AND ")
                );
                let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                    params.iter().map(|p| p.as_ref()).collect();

                let mut stmt = conn.prepare(&sql)?;
                let rows: Vec<(i64, i64)> = stmt
                    .query_map(params_ref.as_slice(), |row| Ok((row.get(0)?, row.get(1)?)))?
                    .filter_map(|row| row.ok())
                    .collect();

                for (local_id, create_time) in rows {
                    let dedup_key = format!("{}:{}:{}", chat_username, local_id, create_time);
                    if !dedup.insert(dedup_key) {
                        continue;
                    }
                    result.push(VoiceBackfillTarget {
                        chat_username: chat_username.clone(),
                        local_id,
                        create_time,
                        duration_secs: None,
                    });
                }
            }
        }

        result.sort_by_key(|item| (item.create_time, item.local_id));
        if let Some(limit) = limit {
            result.truncate(limit);
        }
        Ok(result)
    })
    .await?
}

pub async fn estimate_backfill(
    db: &DbCache,
    targets: Vec<VoiceBackfillTarget>,
) -> Result<VoiceBackfillEstimate> {
    let total_messages = targets.len();
    let mut cached_messages = 0usize;
    let mut unresolved_messages = 0usize;
    let mut pending_seconds = 0.0f64;
    let mut pending_targets = Vec::new();

    for target in targets {
        if cached_transcript(&target.chat_username, target.local_id, target.create_time)
            .await?
            .is_some()
        {
            cached_messages += 1;
            continue;
        }

        let Some(voice_blob) = load_voice_blob(
            db,
            &target.chat_username,
            target.local_id,
            target.create_time,
        )
        .await?
        else {
            unresolved_messages += 1;
            continue;
        };

        let duration_secs =
            tokio::task::spawn_blocking(move || decode_voice_duration_secs(&voice_blob)).await??;
        pending_seconds += duration_secs;

        let mut target = target;
        target.duration_secs = Some(duration_secs);
        pending_targets.push(target);
    }

    let price_per_second_usd = current_asr_price_per_second_usd();
    let pending_messages = pending_targets.len();

    Ok(VoiceBackfillEstimate {
        total_messages,
        cached_messages,
        pending_messages,
        unresolved_messages,
        pending_seconds,
        pending_cost_usd: pending_seconds * price_per_second_usd,
        price_per_second_usd,
        pending_targets,
    })
}

pub async fn cached_transcript(
    chat_username: &str,
    local_id: i64,
    create_time: i64,
) -> Result<Option<VoiceTranscript>> {
    let chat_username = chat_username.to_string();
    let cached = tokio::task::spawn_blocking(move || {
        read_cache_entry(&chat_username, local_id, create_time)
    })
    .await??;

    Ok(cached.map(|(text, model)| VoiceTranscript {
        text,
        model,
        cached: true,
    }))
}

pub async fn transcribe_voice_message(
    db: &DbCache,
    chat_username: &str,
    local_id: i64,
    create_time: i64,
) -> Result<VoiceTranscript> {
    let chat_username_owned = chat_username.to_string();
    if let Some((text, model)) = tokio::task::spawn_blocking({
        let chat_username = chat_username_owned.clone();
        move || read_cache_entry(&chat_username, local_id, create_time)
    })
    .await??
    {
        return Ok(VoiceTranscript {
            text,
            model,
            cached: true,
        });
    }

    let voice_blob = load_voice_blob(db, &chat_username_owned, local_id, create_time)
        .await?
        .with_context(|| format!("找不到语音数据: {}#{}", chat_username_owned, local_id))?;

    let wav_bytes = tokio::task::spawn_blocking(move || decode_silk_to_wav(&voice_blob)).await??;
    let transcript = tokio::task::spawn_blocking({
        let chat_username = chat_username_owned.clone();
        move || transcribe_wav_with_bailian(&chat_username, local_id, create_time, &wav_bytes)
    })
    .await??;

    tokio::task::spawn_blocking({
        let chat_username = chat_username_owned.clone();
        let text = transcript.text.clone();
        let model = transcript.model.clone();
        move || write_cache_entry(&chat_username, local_id, create_time, &text, &model)
    })
    .await??;

    Ok(transcript)
}

async fn load_voice_blob(
    db: &DbCache,
    chat_username: &str,
    local_id: i64,
    create_time: i64,
) -> Result<Option<Vec<u8>>> {
    let mut media_keys: Vec<String> = db
        .keys()
        .into_iter()
        .filter(|key| is_media_db_key(key))
        .collect();
    media_keys.sort();

    for rel_key in media_keys {
        let Some(path) = db.get(&rel_key).await? else {
            continue;
        };

        let chat = chat_username.to_string();
        let blob = tokio::task::spawn_blocking(move || query_voice_blob(&path, &chat, local_id, create_time)).await??;
        if blob.is_some() {
            return Ok(blob);
        }
    }

    Ok(None)
}

fn is_media_db_key(key: &str) -> bool {
    let normalized = key.replace('\\', "/");
    normalized.ends_with(".db") && normalized.contains("message/media_")
}

fn query_voice_blob(
    db_path: &Path,
    chat_username: &str,
    local_id: i64,
    create_time: i64,
) -> Result<Option<Vec<u8>>> {
    let conn = Connection::open(db_path)?;

    let direct = conn
        .query_row(
            "SELECT v.voice_data
             FROM VoiceInfo v
             JOIN Name2Id n ON n.rowid = v.chat_name_id
             WHERE n.user_name = ?1 AND v.local_id = ?2
             ORDER BY ABS(v.create_time - ?3), v.create_time DESC
             LIMIT 1",
            params![chat_username, local_id, create_time],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    if direct.is_some() {
        return Ok(direct);
    }

    if create_time <= 0 {
        return Ok(None);
    }

    let fallback = conn
        .query_row(
            "SELECT v.voice_data
             FROM VoiceInfo v
             JOIN Name2Id n ON n.rowid = v.chat_name_id
             WHERE n.user_name = ?1
               AND v.create_time BETWEEN ?2 AND ?3
             ORDER BY ABS(v.create_time - ?4), ABS(v.local_id - ?5), v.create_time DESC
             LIMIT 1",
            params![
                chat_username,
                create_time - 300,
                create_time + 300,
                create_time,
                local_id
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;

    Ok(fallback)
}

fn decode_silk_to_wav(voice_blob: &[u8]) -> Result<Vec<u8>> {
    let decoder_bin = ensure_decoder_binary()?;

    let mut child = Command::new(&decoder_bin)
        .arg("--sample-rate")
        .arg(DEFAULT_SILK_SAMPLE_RATE)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("启动 silk 解码器失败: {}", decoder_bin.display()))?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .context("无法打开 silk 解码器 stdin")?;
        stdin.write_all(voice_blob)?;
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("silk 解码失败: {}", stderr.trim());
    }
    if output.stdout.is_empty() {
        anyhow::bail!("silk 解码结果为空");
    }
    Ok(output.stdout)
}

fn decode_voice_duration_secs(voice_blob: &[u8]) -> Result<f64> {
    let wav = decode_silk_to_wav(voice_blob)?;
    if wav.len() < 44 {
        anyhow::bail!("WAV 数据长度异常");
    }
    Ok((wav.len().saturating_sub(44) as f64) / (24000.0 * 2.0))
}

fn ensure_decoder_binary() -> Result<PathBuf> {
    let tool_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools/wechat_silk_to_wav");
    let bin_dir = config::cache_dir().join("bin");
    std::fs::create_dir_all(&bin_dir)?;

    let bin_name = if cfg!(windows) {
        "wechat_silk_to_wav.exe"
    } else {
        "wechat_silk_to_wav"
    };
    let bin_path = bin_dir.join(bin_name);

    if !decoder_needs_rebuild(&bin_path, &tool_dir)? {
        return Ok(bin_path);
    }

    let output = Command::new("go")
        .arg("build")
        .arg("-o")
        .arg(&bin_path)
        .arg(".")
        .current_dir(&tool_dir)
        .output()
        .context("调用 go build 失败，请确认系统已安装 Go")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("构建 silk 解码器失败: {}", stderr.trim());
    }

    Ok(bin_path)
}

fn decoder_needs_rebuild(bin_path: &Path, tool_dir: &Path) -> Result<bool> {
    if !bin_path.exists() {
        return Ok(true);
    }

    let bin_mtime = std::fs::metadata(bin_path)?.modified()?;
    for rel in ["main.go", "go.mod", "go.sum"] {
        let path = tool_dir.join(rel);
        if !path.exists() {
            continue;
        }
        let src_mtime = std::fs::metadata(&path)?.modified()?;
        if src_mtime > bin_mtime {
            return Ok(true);
        }
    }
    Ok(false)
}

fn transcribe_wav_with_bailian(
    chat_username: &str,
    local_id: i64,
    create_time: i64,
    wav_bytes: &[u8],
) -> Result<VoiceTranscript> {
    let script_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("qwen3_asr_flash_smoketest.py");
    if !script_path.exists() {
        anyhow::bail!("找不到 ASR 脚本: {}", script_path.display());
    }

    let temp_dir = config::cache_dir().join("voice_asr_tmp");
    std::fs::create_dir_all(&temp_dir)?;

    let temp_name = format!(
        "{}-{}-{}-{}.wav",
        sanitize_for_filename(chat_username),
        local_id,
        create_time,
        std::process::id()
    );
    let temp_path = temp_dir.join(temp_name);
    std::fs::write(&temp_path, wav_bytes)?;

    let language = std::env::var("WX_ASR_LANGUAGE").unwrap_or_else(|_| DEFAULT_ASR_LANGUAGE.to_string());
    let model_override = std::env::var("WX_ASR_MODEL").ok().filter(|s| !s.trim().is_empty());

    let output = run_python_asr(&script_path, &temp_path, &language, model_override.as_deref());
    let _ = std::fs::remove_file(&temp_path);
    let output = output?;

    let response: Value = serde_json::from_slice(&output.stdout)
        .context("解析百炼 ASR 返回 JSON 失败")?;
    let text = response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .context("百炼 ASR 未返回转写文本")?;

    let model = response
        .get("model")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .or(model_override)
        .unwrap_or_else(|| DEFAULT_ASR_MODEL.to_string());

    Ok(VoiceTranscript {
        text,
        model,
        cached: false,
    })
}

fn run_python_asr(
    script_path: &Path,
    audio_path: &Path,
    language: &str,
    model_override: Option<&str>,
) -> Result<std::process::Output> {
    let mut last_err = None;
    for candidate in python_candidates() {
        let mut cmd = Command::new(&candidate);
        cmd.arg(script_path)
            .arg(audio_path)
            .arg("--language")
            .arg(language)
            .arg("--enable-itn")
            .arg("--json")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(model) = model_override {
            cmd.arg("--model").arg(model);
        }

        match cmd.output() {
            Ok(output) => {
                if output.status.success() {
                    return Ok(output);
                }

                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                anyhow::bail!(
                    "调用百炼 ASR 失败: {}\n{}",
                    stderr.trim(),
                    stdout.trim()
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                last_err = Some(format!("找不到 Python 解释器: {}", candidate));
            }
            Err(err) => {
                last_err = Some(format!("启动 {} 失败: {}", candidate, err));
            }
        }
    }

    anyhow::bail!(
        "{}",
        last_err.unwrap_or_else(|| "没有可用的 Python 解释器".to_string())
    )
}

fn python_candidates() -> Vec<String> {
    let mut candidates = Vec::new();

    for key in ["WX_ASR_PYTHON", "PYTHON"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() && !candidates.iter().any(|v| v == trimmed) {
                candidates.push(trimmed.to_string());
            }
        }
    }

    for fallback in ["python3", "python"] {
        if !candidates.iter().any(|v| v == fallback) {
            candidates.push(fallback.to_string());
        }
    }

    candidates
}

fn annotate_message_with_transcript(message: &mut Value, transcript: VoiceTranscript) {
    if let Some(obj) = message.as_object_mut() {
        obj.insert(
            "content".into(),
            Value::String(format!("[语音] {}", transcript.text)),
        );
        obj.insert("voice_text".into(), Value::String(transcript.text));
        obj.insert("voice_model".into(), Value::String(transcript.model));
        obj.insert(
            "voice_status".into(),
            Value::String(if transcript.cached {
                "cached".into()
            } else {
                "transcribed".into()
            }),
        );
        obj.insert("voice_cached".into(), Value::Bool(transcript.cached));
    }
}

fn annotate_message_with_error(message: &mut Value, err: anyhow::Error) {
    if let Some(obj) = message.as_object_mut() {
        obj.insert("voice_status".into(), Value::String("error".into()));
        obj.insert("voice_error".into(), Value::String(err.to_string()));
    }
}

fn cache_db_path() -> PathBuf {
    config::cache_dir().join("_voice_asr.db")
}

fn ensure_cache_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS voice_asr_cache (
            chat_username TEXT NOT NULL,
            local_id INTEGER NOT NULL,
            create_time INTEGER NOT NULL,
            transcript TEXT NOT NULL,
            model TEXT NOT NULL,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (chat_username, local_id, create_time)
        );",
    )?;
    Ok(())
}

fn read_cache_entry(
    chat_username: &str,
    local_id: i64,
    create_time: i64,
) -> Result<Option<(String, String)>> {
    let path = cache_db_path();
    let conn = Connection::open(path)?;
    ensure_cache_schema(&conn)?;
    let row = conn
        .query_row(
            "SELECT transcript, model
             FROM voice_asr_cache
             WHERE chat_username = ?1 AND local_id = ?2 AND create_time = ?3",
            params![chat_username, local_id, create_time],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    Ok(row)
}

fn write_cache_entry(
    chat_username: &str,
    local_id: i64,
    create_time: i64,
    transcript: &str,
    model: &str,
) -> Result<()> {
    let path = cache_db_path();
    let conn = Connection::open(path)?;
    ensure_cache_schema(&conn)?;
    conn.execute(
        "INSERT INTO voice_asr_cache (
            chat_username, local_id, create_time, transcript, model, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(chat_username, local_id, create_time)
         DO UPDATE SET
            transcript = excluded.transcript,
            model = excluded.model,
            updated_at = excluded.updated_at",
        params![
            chat_username,
            local_id,
            create_time,
            transcript,
            model,
            Local::now().timestamp()
        ],
    )?;
    Ok(())
}

fn sanitize_for_filename(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "voice".into()
    } else {
        out
    }
}

fn msg_table_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^Msg_[0-9a-f]{32}$").unwrap())
}

fn current_asr_price_per_second_usd() -> f64 {
    std::env::var("WX_ASR_PRICE_PER_SECOND")
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| *value > 0.0)
        .unwrap_or(DEFAULT_MAINLAND_PRICE_PER_SECOND_USD)
}
