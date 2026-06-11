use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TransferAppMsg {
    pub(super) transfer_id: String,
    pub(super) title: String,
    pub(super) description: String,
    pub(super) paysubtype: String,
    pub(super) receiver_username: String,
    pub(super) amount_cents: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferDirection {
    Sent,
    Received,
}

impl TransferDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Received => "received",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferOutcome {
    Completed,
    Refunded,
    Pending,
    Unknown,
}

impl TransferOutcome {
    fn reason(self, direction: TransferDirection) -> &'static str {
        match (self, direction) {
            (Self::Completed, _) => "completed",
            (Self::Refunded, TransferDirection::Sent) => "returned_by_receiver",
            (Self::Refunded, TransferDirection::Received) => "not_collected_or_returned",
            (Self::Pending, _) => "pending_confirmation",
            (Self::Unknown, _) => "unknown_final_state",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TransferMessage {
    pub(super) local_id: i64,
    pub(super) timestamp: i64,
    pub(super) sender_username: String,
    pub(super) app: TransferAppMsg,
}

#[derive(Debug, Default)]
pub(super) struct TransferBucket {
    pub(super) sent_count: usize,
    pub(super) sent_total_cents: i64,
    pub(super) received_count: usize,
    pub(super) received_total_cents: i64,
}

impl TransferBucket {
    fn record(&mut self, direction: TransferDirection, amount_cents: i64) {
        match direction {
            TransferDirection::Sent => {
                self.sent_count += 1;
                self.sent_total_cents += amount_cents;
            }
            TransferDirection::Received => {
                self.received_count += 1;
                self.received_total_cents += amount_cents;
            }
        }
    }

    fn to_value(&self, period: &str) -> Value {
        json!({
            "period": period,
            "sent_count": self.sent_count,
            "sent_total": format_cents(self.sent_total_cents),
            "sent_total_cents": self.sent_total_cents,
            "received_count": self.received_count,
            "received_total": format_cents(self.received_total_cents),
            "received_total_cents": self.received_total_cents,
        })
    }
}

#[derive(Debug, Default)]
pub(super) struct TransferSummary {
    pub(super) transfers: Vec<Value>,
    pub(super) excluded_transfers: Vec<Value>,
    pub(super) summary: TransferBucket,
    pub(super) monthly: BTreeMap<String, TransferBucket>,
    pub(super) skipped: usize,
}

/// 查询某个联系人与你之间的转账台账
pub async fn q_transfers(
    db: &DbCache,
    names: &Names,
    chat: &str,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Value> {
    let username =
        resolve_username(chat, names).with_context(|| format!("找不到联系人: {}", chat))?;
    let display = names.display(&username);
    let chat_type = chat_type_of(&username, names);

    if chat_type != "private" {
        anyhow::bail!("目前只支持私聊联系人的转账统计");
    }

    let tables = find_msg_tables(db, names, &username).await?;
    if tables.is_empty() {
        return Ok(json!({
            "chat": display,
            "username": username,
            "is_group": false,
            "chat_type": chat_type,
            "count": 0,
            "sent_total": "0.00",
            "sent_total_cents": 0,
            "received_total": "0.00",
            "received_total_cents": 0,
            "sent_count": 0,
            "received_count": 0,
            "skipped": 0,
            "summary": TransferBucket::default().to_value("all"),
            "monthly_rows": [],
            "excluded_transfers": [],
            "transfers": [],
        }));
    }

    let mut all_msgs: Vec<TransferMessage> = Vec::new();
    for (db_path, table_name) in &tables {
        let path = db_path.clone();
        let tname = table_name.clone();
        let uname = username.clone();
        let msgs = tokio::task::spawn_blocking(move || {
            query_transfer_messages(&path, &tname, &uname, since, until)
        })
        .await??;
        all_msgs.extend(msgs);
    }

    let summary = summarize_transfer_messages(&username, all_msgs);
    let monthly_rows: Vec<Value> = summary
        .monthly
        .iter()
        .map(|(period, bucket)| bucket.to_value(period))
        .collect();

    Ok(json!({
        "chat": display,
        "username": username,
        "is_group": false,
        "chat_type": chat_type,
        "count": summary.transfers.len(),
        "sent_total": format_cents(summary.summary.sent_total_cents),
        "sent_total_cents": summary.summary.sent_total_cents,
        "received_total": format_cents(summary.summary.received_total_cents),
        "received_total_cents": summary.summary.received_total_cents,
        "sent_count": summary.summary.sent_count,
        "received_count": summary.summary.received_count,
        // 兼容上一版字段名，避免外部脚本马上断掉
        "paid_total": format_cents(summary.summary.sent_total_cents),
        "paid_total_cents": summary.summary.sent_total_cents,
        "skipped": summary.skipped,
        "summary": summary.summary.to_value("all"),
        "monthly_rows": monthly_rows,
        "excluded_transfers": summary.excluded_transfers,
        "transfers": summary.transfers,
    }))
}

fn query_transfer_messages(
    db_path: &std::path::Path,
    table: &str,
    chat_username: &str,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Vec<TransferMessage>> {
    let conn = Connection::open(db_path)?;
    ensure_create_time_index(&conn, table);
    let id2u = load_id2u(&conn);

    let mut clauses = vec!["(local_type & 4294967295) = 49".to_string()];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(s) = since {
        clauses.push("create_time >= ?".into());
        params.push(Box::new(s));
    }
    if let Some(u) = until {
        clauses.push("create_time <= ?".into());
        params.push(Box::new(u));
    }
    let where_clause = format!("WHERE {}", clauses.join(" AND "));
    let sql = format!(
        "SELECT local_id, create_time, real_sender_id,
                message_content, WCDB_CT_message_content
         FROM [{}] {} ORDER BY create_time ASC, local_id ASC",
        table, where_clause
    );

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_ref.as_slice(), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                get_content_bytes(row, 3),
                row.get::<_, i64>(4).unwrap_or(0),
            ))
        })?
        .filter_map(|r| r.ok())
        .collect::<Vec<_>>();

    let mut result = Vec::new();
    for (local_id, ts, real_sender_id, content_bytes, ct) in rows {
        let content = decompress_message(&content_bytes, ct);
        let Some(app) = parse_transfer_appmsg_xml(&content) else {
            continue;
        };

        let mut sender_username = id2u.get(&real_sender_id).cloned().unwrap_or_default();
        if sender_username.is_empty() {
            sender_username = infer_sender_from_receiver(chat_username, &app.receiver_username);
        }

        result.push(TransferMessage {
            local_id,
            timestamp: ts,
            sender_username,
            app,
        });
    }

    Ok(result)
}

pub(super) fn summarize_transfer_messages(
    chat_username: &str,
    messages: Vec<TransferMessage>,
) -> TransferSummary {
    let mut grouped: HashMap<String, Vec<TransferMessage>> = HashMap::new();
    for msg in messages {
        grouped
            .entry(msg.app.transfer_id.clone())
            .or_default()
            .push(msg);
    }

    let mut summary = TransferSummary::default();
    for (_, mut group) in grouped {
        group.sort_by_key(|msg| (msg.timestamp, msg.local_id));

        let initiator = group
            .iter()
            .find(|msg| matches!(msg.app.paysubtype.as_str(), "1" | "8"));
        let Some(representative) = initiator else {
            let fallback = group.first();
            let amount_cents = fallback
                .and_then(|msg| msg.app.amount_cents)
                .or_else(|| group.iter().find_map(|msg| msg.app.amount_cents));
            let direction = fallback
                .and_then(|msg| determine_transfer_direction(chat_username, msg))
                .or_else(|| {
                    group
                        .iter()
                        .find_map(|msg| determine_transfer_direction(chat_username, msg))
                });

            let mut source_local_ids: Vec<i64> = group.iter().map(|msg| msg.local_id).collect();
            source_local_ids.sort_unstable();
            source_local_ids.dedup();

            summary.excluded_transfers.push(json!({
                "time": fallback.map(|msg| fmt_time(msg.timestamp, "%Y-%m-%d %H:%M")).unwrap_or_default(),
                "timestamp": fallback.map(|msg| msg.timestamp).unwrap_or(0),
                "direction": direction.map(|d| d.as_str()).unwrap_or("unknown"),
                "amount": amount_cents.map(format_cents).unwrap_or_else(|| "0.00".into()),
                "amount_display": amount_cents.map(format_cents_with_symbol).unwrap_or_else(|| "￥0.00".into()),
                "amount_cents": amount_cents.unwrap_or(0),
                "transfer_id": fallback.map(|msg| msg.app.transfer_id.clone()).unwrap_or_default(),
                "reason": "missing_initiator_card",
                "source_local_ids": source_local_ids,
            }));
            summary.skipped += 1;
            continue;
        };

        let amount_cents = representative
            .app
            .amount_cents
            .or_else(|| group.iter().find_map(|msg| msg.app.amount_cents));
        let direction = determine_transfer_direction(chat_username, representative).or_else(|| {
            group
                .iter()
                .find_map(|msg| determine_transfer_direction(chat_username, msg))
        });

        let (amount_cents, direction) = match (amount_cents, direction) {
            (Some(amount_cents), Some(direction)) => (amount_cents, direction),
            _ => {
                summary.skipped += 1;
                continue;
            }
        };

        let mut source_local_ids: Vec<i64> = group.iter().map(|msg| msg.local_id).collect();
        source_local_ids.sort_unstable();
        source_local_ids.dedup();
        let period = fmt_time(representative.timestamp, "%Y-%m");
        let final_subtype = group
            .iter()
            .rev()
            .find(|msg| !matches!(msg.app.paysubtype.as_str(), "1" | "8"))
            .map(|msg| msg.app.paysubtype.as_str())
            .unwrap_or_default();
        let outcome = classify_transfer_outcome(final_subtype);

        if outcome == TransferOutcome::Completed {
            summary.summary.record(direction, amount_cents);
            summary
                .monthly
                .entry(period.clone())
                .or_default()
                .record(direction, amount_cents);
        } else {
            summary.excluded_transfers.push(json!({
                "time": fmt_time(representative.timestamp, "%Y-%m-%d %H:%M"),
                "timestamp": representative.timestamp,
                "direction": direction.as_str(),
                "amount": format_cents(amount_cents),
                "amount_display": format_cents_with_symbol(amount_cents),
                "amount_cents": amount_cents,
                "transfer_id": representative.app.transfer_id.clone(),
                "reason": outcome.reason(direction),
                "final_subtype": final_subtype,
                "source_local_ids": source_local_ids,
            }));
            summary.skipped += 1;
            continue;
        }

        summary.transfers.push(json!({
            "time": fmt_time(representative.timestamp, "%Y-%m-%d %H:%M"),
            "timestamp": representative.timestamp,
            "month": period,
            "direction": direction.as_str(),
            "final_subtype": final_subtype,
            "amount": format_cents(amount_cents),
            "amount_display": format_cents_with_symbol(amount_cents),
            "amount_cents": amount_cents,
            "transfer_id": representative.app.transfer_id.clone(),
            "title": representative.app.title.clone(),
            "description": representative.app.description.clone(),
            "source_local_ids": source_local_ids,
        }));
    }

    summary
        .transfers
        .sort_by_key(|item| item["timestamp"].as_i64().unwrap_or(0));
    summary
        .excluded_transfers
        .sort_by_key(|item| item["timestamp"].as_i64().unwrap_or(0));
    summary
}

fn classify_transfer_outcome(final_subtype: &str) -> TransferOutcome {
    match final_subtype {
        "3" => TransferOutcome::Completed,
        "4" => TransferOutcome::Refunded,
        "" => TransferOutcome::Pending,
        _ => TransferOutcome::Unknown,
    }
}

fn determine_transfer_direction(
    chat_username: &str,
    msg: &TransferMessage,
) -> Option<TransferDirection> {
    if !msg.sender_username.is_empty() {
        return Some(if msg.sender_username == chat_username {
            TransferDirection::Received
        } else {
            TransferDirection::Sent
        });
    }
    if !msg.app.receiver_username.is_empty() {
        return Some(if msg.app.receiver_username == chat_username {
            TransferDirection::Sent
        } else {
            TransferDirection::Received
        });
    }
    None
}

fn infer_sender_from_receiver(chat_username: &str, receiver_username: &str) -> String {
    if receiver_username.is_empty() {
        return String::new();
    }
    if receiver_username == chat_username {
        String::new()
    } else {
        chat_username.to_string()
    }
}

pub(super) fn parse_transfer_appmsg_xml(text: &str) -> Option<TransferAppMsg> {
    let atype = extract_xml_text(text, "type")?;
    if atype != "2000" {
        return None;
    }

    let transfer_id = extract_xml_text(text, "transferid").unwrap_or_default();
    if transfer_id.is_empty() {
        return None;
    }

    let description = extract_xml_text(text, "des").unwrap_or_default();
    let feedesc = extract_xml_text(text, "feedesc").unwrap_or_default();
    let amount_cents = parse_amount_cents(&feedesc).or_else(|| parse_amount_cents(&description));

    Some(TransferAppMsg {
        transfer_id,
        title: extract_xml_text(text, "title").unwrap_or_default(),
        description,
        paysubtype: extract_xml_text(text, "paysubtype").unwrap_or_default(),
        receiver_username: extract_xml_text(text, "receiver_username").unwrap_or_default(),
        amount_cents,
    })
}

fn parse_amount_cents(text: &str) -> Option<i64> {
    for capture in [
        transfer_amount_currency_re().captures(text),
        transfer_amount_yuan_re().captures(text),
        transfer_amount_generic_re().captures(text),
    ] {
        let Some(caps) = capture else {
            continue;
        };
        let amount = caps.get(1)?.as_str();
        if let Some(cents) = decimal_amount_to_cents(amount) {
            return Some(cents);
        }
    }
    None
}

fn transfer_amount_currency_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[¥￥]\s*([0-9][0-9,]*(?:\.[0-9]{1,2})?)").unwrap())
}

fn transfer_amount_yuan_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"([0-9][0-9,]*(?:\.[0-9]{1,2})?)\s*元").unwrap())
}

fn transfer_amount_generic_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"([0-9][0-9,]*(?:\.[0-9]{1,2})?)").unwrap())
}

fn decimal_amount_to_cents(raw: &str) -> Option<i64> {
    let normalized = raw.trim().replace(',', "");
    if normalized.is_empty() {
        return None;
    }

    let (whole, frac) = normalized.split_once('.').unwrap_or((&normalized, ""));
    let whole = whole.parse::<i64>().ok()?;
    let frac = match frac.len() {
        0 => "00".to_string(),
        1 => format!("{}0", frac),
        _ => frac.chars().take(2).collect::<String>(),
    };
    let frac = frac.parse::<i64>().ok()?;
    whole.checked_mul(100)?.checked_add(frac)
}

pub(super) fn format_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!(
        "{}{whole}.{frac:02}",
        sign,
        whole = abs / 100,
        frac = abs % 100
    )
}

pub(super) fn format_cents_with_symbol(cents: i64) -> String {
    format!("￥{}", format_cents(cents))
}
