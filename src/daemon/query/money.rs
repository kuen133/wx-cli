use super::*;
use rusqlite::OptionalExtension;
use std::path::Path;

const GENERAL_DB_KEY: &str = "general/general.db";
const REDPACKET_TABLE: &str = "redEnvelopeTable";
const TRANSFER_TABLE: &str = "transferTable";

pub async fn q_redpackets(db: &DbCache, names: &Names, limit: Option<usize>) -> Result<Value> {
    let path = db
        .get(GENERAL_DB_KEY)
        .await?
        .context("无法解密 general.db")?;
    let names = names.clone();

    tokio::task::spawn_blocking(move || q_redpackets_from_path(&path, &names, limit)).await?
}

pub async fn q_transfer_events(
    db: &DbCache,
    names: &Names,
    limit: Option<usize>,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Value> {
    let path = db
        .get(GENERAL_DB_KEY)
        .await?
        .context("无法解密 general.db")?;
    let names = names.clone();

    tokio::task::spawn_blocking(move || {
        q_transfer_events_from_path(&path, &names, limit, since, until)
    })
    .await?
}

fn q_redpackets_from_path(path: &Path, names: &Names, limit: Option<usize>) -> Result<Value> {
    let conn = Connection::open(path)?;
    if !table_exists(&conn, REDPACKET_TABLE)? {
        return Ok(json!({ "count": 0, "redpackets": [] }));
    }

    let rows = load_redpacket_rows(&conn, limit)?;
    let redpackets: Vec<Value> = rows
        .into_iter()
        .map(|row| redpacket_to_value(row, names))
        .collect();

    Ok(json!({
        "count": redpackets.len(),
        "redpackets": redpackets,
    }))
}

fn q_transfer_events_from_path(
    path: &Path,
    names: &Names,
    limit: Option<usize>,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Value> {
    let conn = Connection::open(path)?;
    if !table_exists(&conn, TRANSFER_TABLE)? {
        return Ok(json!({ "count": 0, "transfers": [] }));
    }

    let rows = load_transfer_event_rows(&conn, limit, since, until)?;
    let transfers: Vec<Value> = rows
        .into_iter()
        .map(|row| transfer_event_to_value(row, names))
        .collect();

    Ok(json!({
        "count": transfers.len(),
        "transfers": transfers,
    }))
}

#[derive(Debug)]
struct RedpacketRow {
    message_server_id: Option<i64>,
    session_username: Option<String>,
    sender_username: Option<String>,
    scene_id: Option<i64>,
    hb_status: Option<i64>,
    hb_type: Option<i64>,
    receive_status: Option<i64>,
    send_id: Option<String>,
    native_url: Option<String>,
}

#[derive(Debug)]
struct TransferEventRow {
    transfer_id: Option<String>,
    transcation_id: Option<String>,
    session_username: Option<String>,
    pay_payer_username: Option<String>,
    pay_receiver_username: Option<String>,
    pay_sub_type: Option<i64>,
    begin_transfer_time: Option<i64>,
    last_modified_time: Option<i64>,
    invalid_time: Option<i64>,
    delay_confirm_flag: Option<i64>,
    bubble_clicked_flag: Option<i64>,
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

fn load_redpacket_rows(conn: &Connection, limit: Option<usize>) -> Result<Vec<RedpacketRow>> {
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let limit_clause = if let Some(limit) = limit {
        params.push(Box::new(limit as i64));
        " LIMIT ?"
    } else {
        ""
    };
    let sql = format!(
        "SELECT message_server_id, session_name, sender_user_name, scene_id,
                hb_status, hb_type, receive_status, send_id, native_url
         FROM {REDPACKET_TABLE}
         ORDER BY message_server_id DESC{limit_clause}"
    );
    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_ref.as_slice(), |row| {
            Ok(RedpacketRow {
                message_server_id: row.get(0)?,
                session_username: row.get(1)?,
                sender_username: row.get(2)?,
                scene_id: row.get(3)?,
                hb_status: row.get(4)?,
                hb_type: row.get(5)?,
                receive_status: row.get(6)?,
                send_id: row.get(7)?,
                native_url: row.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn load_transfer_event_rows(
    conn: &Connection,
    limit: Option<usize>,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Vec<TransferEventRow>> {
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(since) = since {
        clauses.push("begin_transfer_time >= ?");
        params.push(Box::new(since));
    }
    if let Some(until) = until {
        clauses.push("begin_transfer_time <= ?");
        params.push(Box::new(until));
    }

    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let limit_clause = if let Some(limit) = limit {
        params.push(Box::new(limit as i64));
        " LIMIT ?"
    } else {
        ""
    };
    let sql = format!(
        "SELECT transfer_id, transcation_id, session_name, pay_payer, pay_receiver,
                pay_sub_type, begin_transfer_time, last_modified_time, invalid_time,
                delay_confirm_flag, bubble_clicked_flag
         FROM {TRANSFER_TABLE}
         {where_clause}
         ORDER BY begin_transfer_time DESC{limit_clause}"
    );
    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_ref.as_slice(), |row| {
            Ok(TransferEventRow {
                transfer_id: row.get(0)?,
                transcation_id: row.get(1)?,
                session_username: row.get(2)?,
                pay_payer_username: row.get(3)?,
                pay_receiver_username: row.get(4)?,
                pay_sub_type: row.get(5)?,
                begin_transfer_time: row.get(6)?,
                last_modified_time: row.get(7)?,
                invalid_time: row.get(8)?,
                delay_confirm_flag: row.get(9)?,
                bubble_clicked_flag: row.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn display_name(names: &Names, username: Option<&str>) -> Option<String> {
    username.map(|username| names.display(username))
}

fn formatted_time(timestamp: Option<i64>) -> Option<String> {
    timestamp.map(|timestamp| fmt_time(timestamp, "%Y-%m-%d %H:%M"))
}

fn redpacket_to_value(row: RedpacketRow, names: &Names) -> Value {
    json!({
        "message_server_id": row.message_server_id,
        "session_name": display_name(names, row.session_username.as_deref()),
        "session_username": row.session_username,
        "sender_user_name": display_name(names, row.sender_username.as_deref()),
        "sender_username": row.sender_username,
        "scene_id": row.scene_id,
        "hb_status": row.hb_status,
        "hb_type": row.hb_type,
        "receive_status": row.receive_status,
        "send_id": row.send_id,
        "native_url": row.native_url,
    })
}

fn transfer_event_to_value(row: TransferEventRow, names: &Names) -> Value {
    json!({
        "transfer_id": row.transfer_id,
        "transcation_id": row.transcation_id,
        "session_name": display_name(names, row.session_username.as_deref()),
        "session_username": row.session_username,
        "pay_payer": display_name(names, row.pay_payer_username.as_deref()),
        "pay_payer_username": row.pay_payer_username,
        "pay_receiver": display_name(names, row.pay_receiver_username.as_deref()),
        "pay_receiver_username": row.pay_receiver_username,
        "pay_sub_type": row.pay_sub_type,
        "begin_transfer_time": row.begin_transfer_time,
        "time": formatted_time(row.begin_transfer_time),
        "last_modified_time": row.last_modified_time,
        "last_modified_at": formatted_time(row.last_modified_time),
        "invalid_time": row.invalid_time,
        "invalid_at": formatted_time(row.invalid_time),
        "delay_confirm_flag": row.delay_confirm_flag,
        "bubble_clicked_flag": row.bubble_clicked_flag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn unique_tmpdir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "wx-cli-money-test-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn create_general_db(tag: &str) -> std::path::PathBuf {
        unique_tmpdir(tag).join("general.db")
    }

    fn names() -> Names {
        Names {
            map: HashMap::from([
                ("wxid_session".to_string(), "会话显示名".to_string()),
                ("wxid_sender".to_string(), "发送人显示名".to_string()),
                ("wxid_payer".to_string(), "付款人显示名".to_string()),
                ("wxid_receiver".to_string(), "收款人显示名".to_string()),
            ]),
            md5_to_uname: HashMap::new(),
            msg_db_keys: Vec::new(),
            verify_flags: HashMap::new(),
        }
    }

    fn create_redpacket_table(conn: &Connection) {
        conn.execute(
            "CREATE TABLE redEnvelopeTable (
                message_server_id INTEGER,
                session_name TEXT,
                sender_user_name TEXT,
                scene_id INTEGER,
                hb_status INTEGER,
                hb_type INTEGER,
                receive_status INTEGER,
                send_id TEXT,
                native_url TEXT
            )",
            [],
        )
        .unwrap();
    }

    fn create_transfer_table(conn: &Connection) {
        conn.execute(
            "CREATE TABLE transferTable (
                transfer_id TEXT,
                transcation_id TEXT,
                session_name TEXT,
                pay_payer TEXT,
                pay_receiver TEXT,
                pay_sub_type INTEGER,
                begin_transfer_time INTEGER,
                last_modified_time INTEGER,
                invalid_time INTEGER,
                delay_confirm_flag INTEGER,
                bubble_clicked_flag INTEGER
            )",
            [],
        )
        .unwrap();
    }

    #[test]
    fn redpackets_list_columns_with_display_names_and_limit() {
        let path = create_general_db("redpackets");
        let conn = Connection::open(&path).unwrap();
        create_redpacket_table(&conn);
        conn.execute(
            "INSERT INTO redEnvelopeTable
             (message_server_id, session_name, sender_user_name, scene_id, hb_status, hb_type, receive_status, send_id, native_url)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                100_i64,
                "wxid_session",
                "wxid_sender",
                7_i64,
                2_i64,
                1_i64,
                3_i64,
                "send-old",
                "native://old"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO redEnvelopeTable
             (message_server_id, session_name, sender_user_name, scene_id, hb_status, hb_type, receive_status, send_id, native_url)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                200_i64,
                "wxid_session",
                "unknown_sender",
                8_i64,
                4_i64,
                5_i64,
                6_i64,
                "send-new",
                "native://new"
            ],
        )
        .unwrap();

        let value = q_redpackets_from_path(&path, &names(), Some(1)).unwrap();
        assert_eq!(value["count"], 1);
        let rows = value["redpackets"].as_array().unwrap();
        assert_eq!(rows[0]["message_server_id"], 200);
        assert_eq!(rows[0]["session_name"], "会话显示名");
        assert_eq!(rows[0]["session_username"], "wxid_session");
        assert_eq!(rows[0]["sender_user_name"], "unknown_sender");
        assert_eq!(rows[0]["sender_username"], "unknown_sender");
        assert_eq!(rows[0]["scene_id"], 8);
        assert_eq!(rows[0]["hb_status"], 4);
        assert_eq!(rows[0]["hb_type"], 5);
        assert_eq!(rows[0]["receive_status"], 6);
        assert_eq!(rows[0]["send_id"], "send-new");
        assert_eq!(rows[0]["native_url"], "native://new");
    }

    #[test]
    fn redpackets_missing_table_returns_empty_result() {
        let path = create_general_db("missing-redpacket-table");
        let _conn = Connection::open(&path).unwrap();

        let value = q_redpackets_from_path(&path, &names(), Some(10)).unwrap();
        assert_eq!(value["count"], 0);
        assert!(value["redpackets"].as_array().unwrap().is_empty());
    }

    #[test]
    fn transfer_events_filter_by_begin_time_order_and_limit() {
        let path = create_general_db("transfer-events");
        let conn = Connection::open(&path).unwrap();
        create_transfer_table(&conn);
        for (transfer_id, begin_time, sub_type) in [
            ("t-old", 1_700_000_000_i64, 1_i64),
            ("t-mid", 1_800_000_000_i64, 8_i64),
            ("t-new", 1_900_000_000_i64, 9_i64),
        ] {
            conn.execute(
                "INSERT INTO transferTable
                 (transfer_id, transcation_id, session_name, pay_payer, pay_receiver, pay_sub_type,
                  begin_transfer_time, last_modified_time, invalid_time, delay_confirm_flag, bubble_clicked_flag)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    transfer_id,
                    format!("txn-{transfer_id}"),
                    "wxid_session",
                    "wxid_payer",
                    "wxid_receiver",
                    sub_type,
                    begin_time,
                    begin_time + 60,
                    begin_time + 3600,
                    0_i64,
                    1_i64,
                ],
            )
            .unwrap();
        }

        let value = q_transfer_events_from_path(
            &path,
            &names(),
            Some(1),
            Some(1_750_000_000),
            Some(1_950_000_000),
        )
        .unwrap();
        assert_eq!(value["count"], 1);
        let rows = value["transfers"].as_array().unwrap();
        assert_eq!(rows[0]["transfer_id"], "t-new");
        assert_eq!(rows[0]["transcation_id"], "txn-t-new");
        assert_eq!(rows[0]["session_name"], "会话显示名");
        assert_eq!(rows[0]["session_username"], "wxid_session");
        assert_eq!(rows[0]["pay_payer"], "付款人显示名");
        assert_eq!(rows[0]["pay_payer_username"], "wxid_payer");
        assert_eq!(rows[0]["pay_receiver"], "收款人显示名");
        assert_eq!(rows[0]["pay_receiver_username"], "wxid_receiver");
        assert_eq!(rows[0]["pay_sub_type"], 9);
        assert_eq!(rows[0]["begin_transfer_time"], 1_900_000_000);
        assert_eq!(rows[0]["last_modified_time"], 1_900_000_060);
        assert_eq!(rows[0]["invalid_time"], 1_900_003_600);
        assert_eq!(rows[0]["delay_confirm_flag"], 0);
        assert_eq!(rows[0]["bubble_clicked_flag"], 1);
        assert!(rows[0]["time"].as_str().unwrap().contains("2030"));
    }

    #[test]
    fn transfer_events_missing_table_returns_empty_result() {
        let path = create_general_db("missing-transfer-table");
        let _conn = Connection::open(&path).unwrap();

        let value = q_transfer_events_from_path(&path, &names(), None, None, None).unwrap();
        assert_eq!(value["count"], 0);
        assert!(value["transfers"].as_array().unwrap().is_empty());
    }

    #[test]
    fn transfer_events_preserve_null_fields_without_failing() {
        let path = create_general_db("transfer-null-fields");
        let conn = Connection::open(&path).unwrap();
        create_transfer_table(&conn);
        conn.execute(
            "INSERT INTO transferTable
             (transfer_id, transcation_id, session_name, pay_payer, pay_receiver, pay_sub_type,
              begin_transfer_time, last_modified_time, invalid_time, delay_confirm_flag, bubble_clicked_flag)
             VALUES (NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
            [],
        )
        .unwrap();

        let value = q_transfer_events_from_path(&path, &names(), None, None, None).unwrap();
        assert_eq!(value["count"], 1);
        let rows = value["transfers"].as_array().unwrap();
        assert!(rows[0]["transfer_id"].is_null());
        assert!(rows[0]["pay_sub_type"].is_null());
        assert!(rows[0]["begin_transfer_time"].is_null());
    }
}
