use super::*;
use crate::daemon::meta::{self, Meta};

pub(super) fn build_query_meta(
    db: &DbCache,
    names: &Names,
    chat_latest: Option<(i64, String)>,
    session_last_timestamp: Option<i64>,
    shards_scanned: usize,
    shards_hit: usize,
    windowed: bool,
) -> Meta {
    let (chat_latest_timestamp, chat_latest_db) = match chat_latest {
        Some((ts, rel_key)) => (Some(ts), Some(rel_key)),
        None => (None, None),
    };
    build_query_meta_parts(
        db,
        names,
        chat_latest_timestamp,
        chat_latest_db,
        session_last_timestamp,
        shards_scanned,
        shards_hit,
        windowed,
    )
}

pub(super) fn build_query_meta_parts(
    db: &DbCache,
    names: &Names,
    chat_latest_timestamp: Option<i64>,
    chat_latest_db: Option<String>,
    session_last_timestamp: Option<i64>,
    shards_scanned: usize,
    shards_hit: usize,
    windowed: bool,
) -> Meta {
    meta::build_meta(
        db.db_dir(),
        &names.msg_db_keys,
        chat_latest_timestamp,
        chat_latest_db,
        session_last_timestamp,
        shards_scanned,
        shards_hit,
        windowed,
    )
}

pub(super) fn attach_meta(mut response: Value, meta: Meta) -> Value {
    if let Some(obj) = response.as_object_mut() {
        obj.insert(
            "meta".to_string(),
            serde_json::to_value(meta).unwrap_or(Value::Null),
        );
    }
    response
}

pub(super) fn latest_from_sourced_messages(rows: &[(String, Value)]) -> Option<(i64, String)> {
    rows.iter()
        .filter_map(|(rel_key, row)| row["timestamp"].as_i64().map(|ts| (ts, rel_key.clone())))
        .max_by_key(|(ts, _)| *ts)
}

pub(super) async fn session_last_timestamp(db: &DbCache, username: &str) -> Option<i64> {
    let session_path = match db.get("session/session.db").await {
        Ok(Some(path)) => path,
        _ => return None,
    };
    let username = username.to_string();

    match tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&session_path)?;
        let ts = conn
            .query_row(
                "SELECT last_timestamp FROM SessionTable WHERE username=?1 LIMIT 1",
                rusqlite::params![username],
                |row| row.get::<_, i64>(0),
            )
            .ok();
        Ok::<_, anyhow::Error>(ts)
    })
    .await
    {
        Ok(Ok(Some(ts))) if ts > 0 => Some(ts),
        _ => None,
    }
}

pub(super) fn single_resolved_chat(chats: Option<&Vec<String>>, names: &Names) -> Option<String> {
    let resolved: Vec<String> = chats?
        .iter()
        .filter_map(|chat| resolve_username(chat, names))
        .collect();
    if resolved.len() == 1 {
        resolved.into_iter().next()
    } else {
        None
    }
}

pub(super) async fn locate_result_shards(
    db: &DbCache,
    names: &Names,
    rows: &[Value],
) -> (usize, Option<String>) {
    let mut wanted: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
    let mut latest: Option<(i64, String, i64)> = None;

    for row in rows {
        let Some(username) = row
            .get("chat_uname")
            .and_then(Value::as_str)
            .or_else(|| row.get("username").and_then(Value::as_str))
        else {
            continue;
        };
        let Some(local_id) = row["local_id"].as_i64() else {
            continue;
        };
        let Some(ts) = row["timestamp"].as_i64() else {
            continue;
        };

        let table = format!("Msg_{:x}", md5::compute(username.as_bytes()));
        if !msg_table_re().is_match(&table) {
            continue;
        }
        wanted
            .entry(table.clone())
            .or_default()
            .push((local_id, ts));
        if latest.as_ref().map_or(true, |(cur, _, _)| ts > *cur) {
            latest = Some((ts, table, local_id));
        }
    }

    let Some((latest_ts, latest_table, latest_local_id)) = latest else {
        return (0, None);
    };

    let mut shards_hit = 0usize;
    let mut latest_db = None;
    for rel_key in &names.msg_db_keys {
        let path = match db.get(rel_key).await {
            Ok(Some(path)) => path,
            _ => continue,
        };
        let rel_key_for_task = rel_key.clone();
        let wanted_for_task = wanted.clone();
        let latest_table_for_task = latest_table.clone();

        let found = tokio::task::spawn_blocking(move || -> Result<(bool, bool)> {
            let conn = Connection::open(&path)?;
            let mut shard_hit = false;
            let mut latest_hit = false;

            for (table, pairs) in wanted_for_task {
                let table_exists: Option<i64> = conn
                    .query_row(
                        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                        rusqlite::params![&table],
                        |row| row.get(0),
                    )
                    .ok();
                if table_exists.is_none() {
                    continue;
                }

                for (local_id, ts) in pairs {
                    let exists: Option<i64> = conn
                        .query_row(
                            &format!(
                                "SELECT 1 FROM [{}] WHERE local_id=?1 AND create_time=?2 LIMIT 1",
                                table
                            ),
                            rusqlite::params![local_id, ts],
                            |row| row.get(0),
                        )
                        .ok();
                    if exists.is_some() {
                        shard_hit = true;
                        if table == latest_table_for_task
                            && local_id == latest_local_id
                            && ts == latest_ts
                        {
                            latest_hit = true;
                        }
                    }
                    if latest_hit {
                        break;
                    }
                }
            }

            Ok((shard_hit, latest_hit))
        })
        .await;

        if let Ok(Ok((true, latest_hit))) = found {
            shards_hit += 1;
            if latest_hit && latest_db.is_none() {
                latest_db = Some(rel_key_for_task);
            }
        }
    }

    (shards_hit, latest_db)
}
