use anyhow::{Context, Result};
use chrono::{Local, TimeZone, Timelike};
use regex::Regex;
use roxmltree::{Document, Node};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::config;

use super::cache::DbCache;
use super::search_index;
use super::voice_asr;

mod attachment;
mod biz;
mod content;
mod favorites;
mod image_cache;
mod members;
mod message;
mod names;
mod search;
mod sender;
mod sessions;
mod sns;
mod stats;
mod transfer;
mod xml;

pub use attachment::{q_attachments, q_extract};
pub use biz::q_biz_articles;
pub(crate) use content::{decompress_message, fmt_type};
pub use favorites::q_favorites;
pub use members::q_members;
pub(crate) use message::msg_table_re;
pub use message::{q_history, q_new_messages};
pub use names::{chat_type_of, load_names, q_contacts, Names};
pub use search::q_search;
pub(crate) use sender::load_id2u;
pub use sessions::{q_sessions, q_unread};
pub use sns::{q_friend_requests, q_sns_feed, q_sns_notifications, q_sns_search};
pub use stats::q_stats;
pub use transfer::q_transfers;

use content::{decompress_or_str, fmt_content, fmt_time, get_content_bytes, strip_group_prefix};
#[cfg(test)]
use content::{parse_appmsg, parse_sysmsg};
use image_cache::existing_image_paths;
#[cfg(test)]
use image_cache::{extract_embedded_image_bytes, image_cache_candidates};
#[cfg(test)]
use message::query_messages;
use message::{ensure_create_time_index, find_msg_tables};
use names::resolve_username;
#[cfg(test)]
use search::search_in_table;
use sender::{add_sender_identity, sender_label, sender_username};
#[cfg(test)]
use sns::{parse_post_media, parse_post_xml};
use transfer::{format_cents_with_symbol, parse_transfer_appmsg_xml};
#[cfg(test)]
use transfer::{summarize_transfer_messages, TransferAppMsg, TransferMessage};
use xml::{clean_inline_text, extract_cdata, extract_xml_attr, extract_xml_text, unescape_html};

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("wx-cli-{name}-{nanos}.db"))
    }

    fn transfer_xml(
        transfer_id: &str,
        paysubtype: &str,
        amount: &str,
        receiver_username: &str,
        description: &str,
    ) -> String {
        format!(
            r#"<msg><appmsg><title><![CDATA[微信转账]]></title><des><![CDATA[{description}]]></des><type>2000</type><wcpayinfo><feedesc><![CDATA[¥{amount}]]></feedesc><paysubtype>{paysubtype}</paysubtype><transferid><![CDATA[{transfer_id}]]></transferid><receiver_username><![CDATA[{receiver_username}]]></receiver_username></wcpayinfo></appmsg></msg>"#
        )
    }

    fn transfer_message(
        local_id: i64,
        timestamp: i64,
        sender_username: &str,
        transfer_id: &str,
        paysubtype: &str,
        amount_cents: i64,
        receiver_username: &str,
        description: &str,
    ) -> TransferMessage {
        TransferMessage {
            local_id,
            timestamp,
            sender_username: sender_username.to_string(),
            app: TransferAppMsg {
                transfer_id: transfer_id.to_string(),
                title: "微信转账".to_string(),
                description: description.to_string(),
                paysubtype: paysubtype.to_string(),
                receiver_username: receiver_username.to_string(),
                amount_cents: Some(amount_cents),
            },
        }
    }

    #[test]
    fn ensure_create_time_index_creates_msg_table_index_best_effort() {
        let path = temp_db_path("create-time-index");
        let conn = Connection::open(&path).expect("open temp db");
        let table = "Msg_1234567890abcdef1234567890abcdef";
        conn.execute(
            &format!(
                "CREATE TABLE {table} (
                    local_id INTEGER PRIMARY KEY,
                    create_time INTEGER,
                    message_content TEXT
                )"
            ),
            [],
        )
        .expect("create message table");
        conn.execute(
            &format!(
                "INSERT INTO {table} (local_id, create_time, message_content)
                 VALUES (1, 300, 'newer'), (2, 100, 'older')"
            ),
            [],
        )
        .expect("insert rows");

        ensure_create_time_index(&conn, table);

        let index_name = format!("idx_{table}_ct");
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                [&index_name],
                |row| row.get(0),
            )
            .expect("query sqlite_master");
        assert_eq!(exists, 1);

        ensure_create_time_index(&conn, "Msg_does_not_exist_000000000000000000");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parse_transfer_appmsg_extracts_fields() {
        let xml = transfer_xml(
            "1000050001202512270126284994376",
            "8",
            "4075.00",
            "kuen133",
            "收到转账4075.00元",
        );
        let parsed = parse_transfer_appmsg_xml(&xml).expect("should parse transfer appmsg");

        assert_eq!(parsed.transfer_id, "1000050001202512270126284994376");
        assert_eq!(parsed.paysubtype, "8");
        assert_eq!(parsed.receiver_username, "kuen133");
        assert_eq!(parsed.amount_cents, Some(407_500));
    }

    #[test]
    fn parse_appmsg_formats_transfer_preview() {
        let xml = transfer_xml("t1", "8", "4075.00", "kuen133", "收到转账4075.00元");
        assert_eq!(parse_appmsg(&xml).as_deref(), Some("[转账] ￥4075.00"));
    }

    #[test]
    fn fmt_content_formats_contact_card_nickname() {
        let xml = r#"<msg username="wxid_alice" nickname="Alice" alias="alice"/>"#;
        assert_eq!(fmt_content(1, 42, xml, false), "[名片] Alice");
    }

    #[test]
    fn fmt_content_contact_card_without_nickname_falls_back() {
        let xml = r#"<msg username="wxid_alice" alias="alice"/>"#;
        assert_eq!(fmt_content(1, 42, xml, false), "[名片]");
    }

    #[test]
    fn fmt_content_formats_location_poiname() {
        let xml = r#"<msg><location x="31.2304" y="121.4737" poiname="人民广场" label="上海市黄浦区"/></msg>"#;
        assert_eq!(fmt_content(1, 48, xml, false), "[位置] 人民广场");
    }

    #[test]
    fn fmt_content_location_without_name_falls_back() {
        let xml = r#"<msg><location x="31.2304" y="121.4737"/></msg>"#;
        assert_eq!(fmt_content(1, 48, xml, false), "[位置]");
    }

    #[test]
    fn image_cache_candidate_uses_month_session_and_message_identity() {
        let base = std::path::Path::new("/tmp/xwechat_files/account");
        let paths = image_cache_candidates(
            base,
            "Msg_34051634e027d564babcd7caadd3281a",
            44280,
            1777879275,
        );

        assert_eq!(
            paths[0],
            base.join("cache/2026-05/Message/34051634e027d564babcd7caadd3281a/Thumb/44280_1777879275_thumb.jpg")
        );
    }

    #[test]
    fn extract_embedded_image_bytes_finds_jpeg_payload_inside_dat() {
        let mut input = b"wechat-v2-prefix".to_vec();
        input.extend_from_slice(&[0xff, 0xd8, 0xff, 0xe0, b'J', b'F', b'I', b'F']);

        let extracted = extract_embedded_image_bytes(&input).expect("expected jpeg payload");
        assert_eq!(&extracted[..3], &[0xff, 0xd8, 0xff]);
    }

    #[test]
    fn query_messages_includes_stable_group_sender_identity() {
        let path = temp_db_path("query-messages-stable-sender");
        {
            let conn = Connection::open(&path).expect("open temp db");
            conn.execute("CREATE TABLE Name2Id (user_name TEXT)", [])
                .expect("create Name2Id table");
            conn.execute(
                "INSERT INTO Name2Id(rowid, user_name) VALUES (?1, ?2)",
                rusqlite::params![42_i64, "wxid_alice"],
            )
            .expect("insert Name2Id row");
            conn.execute(
                "CREATE TABLE Msg_test (
                    local_id INTEGER,
                    local_type INTEGER,
                    create_time INTEGER,
                    real_sender_id INTEGER,
                    message_content TEXT,
                    WCDB_CT_message_content INTEGER
                )",
                [],
            )
            .expect("create message table");
            conn.execute(
                "INSERT INTO Msg_test VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![1_i64, 1_i64, 1775146911_i64, 42_i64, "hello", 0_i64],
            )
            .expect("insert text message");
        }

        let names = HashMap::from([("wxid_alice".to_string(), "Alice Contact".to_string())]);
        let rows = query_messages(
            &path,
            "Msg_test",
            "123@chatroom",
            true,
            &names,
            None,
            None,
            None,
            10,
            0,
            None,
        )
        .expect("query messages");

        let _ = std::fs::remove_file(&path);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["sender"].as_str(), Some("Alice Contact"));
        assert_eq!(rows[0]["sender_username"].as_str(), Some("wxid_alice"));
        assert_eq!(rows[0]["from_wxid"].as_str(), Some("wxid_alice"));
        assert_eq!(
            rows[0]["sender_contact_display"].as_str(),
            Some("Alice Contact")
        );
    }

    #[test]
    fn search_in_table_includes_stable_group_sender_identity() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute("CREATE TABLE Name2Id (user_name TEXT)", [])
            .expect("create Name2Id table");
        conn.execute(
            "INSERT INTO Name2Id(rowid, user_name) VALUES (?1, ?2)",
            rusqlite::params![42_i64, "wxid_alice"],
        )
        .expect("insert Name2Id row");
        conn.execute(
            "CREATE TABLE Msg_test (
                local_id INTEGER,
                local_type INTEGER,
                create_time INTEGER,
                real_sender_id INTEGER,
                message_content TEXT,
                WCDB_CT_message_content INTEGER
            )",
            [],
        )
        .expect("create message table");
        conn.execute(
            "INSERT INTO Msg_test VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![1_i64, 1_i64, 1775146911_i64, 42_i64, "needle", 0_i64],
        )
        .expect("insert text message");

        let names = HashMap::from([("wxid_alice".to_string(), "Alice Contact".to_string())]);
        let rows = search_in_table(
            &conn,
            "Msg_test",
            "123@chatroom",
            true,
            &names,
            "needle",
            None,
            None,
            None,
            10,
        )
        .expect("search messages");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["sender"].as_str(), Some("Alice Contact"));
        assert_eq!(rows[0]["sender_username"].as_str(), Some("wxid_alice"));
        assert_eq!(rows[0]["from_wxid"].as_str(), Some("wxid_alice"));
        assert_eq!(
            rows[0]["sender_contact_display"].as_str(),
            Some("Alice Contact")
        );
    }

    #[test]
    fn summarize_transfer_messages_dedupes_and_computes_totals() {
        let summary = summarize_transfer_messages(
            "wangchenfei123",
            vec![
                transfer_message(
                    12,
                    2,
                    "kuen133",
                    "t-in",
                    "3",
                    407_500,
                    "wangchenfei123",
                    "转账给对方4075.00元",
                ),
                transfer_message(
                    3,
                    1,
                    "wangchenfei123",
                    "t-in",
                    "8",
                    407_500,
                    "kuen133",
                    "收到转账4075.00元",
                ),
                transfer_message(
                    20,
                    3,
                    "kuen133",
                    "t-out",
                    "1",
                    50_000,
                    "wangchenfei123",
                    "转账给对方500.00元",
                ),
                transfer_message(
                    21,
                    4,
                    "wangchenfei123",
                    "t-out",
                    "3",
                    50_000,
                    "kuen133",
                    "已收款500.00元",
                ),
            ],
        );

        assert_eq!(summary.transfers.len(), 2);
        assert_eq!(summary.summary.received_total_cents, 407_500);
        assert_eq!(summary.summary.sent_total_cents, 50_000);
        assert_eq!(summary.summary.received_count, 1);
        assert_eq!(summary.summary.sent_count, 1);
        assert_eq!(
            summary
                .monthly
                .get("1970-01")
                .map(|b| b.received_total_cents),
            Some(407_500)
        );
        assert_eq!(
            summary.monthly.get("1970-01").map(|b| b.sent_total_cents),
            Some(50_000)
        );
        assert_eq!(summary.skipped, 0);
        assert!(summary.excluded_transfers.is_empty());
        assert_eq!(summary.transfers[0]["transfer_id"].as_str(), Some("t-in"));
        assert_eq!(summary.transfers[0]["direction"].as_str(), Some("received"));
        assert_eq!(summary.transfers[1]["transfer_id"].as_str(), Some("t-out"));
        assert_eq!(summary.transfers[1]["direction"].as_str(), Some("sent"));
    }

    #[test]
    fn summarize_transfer_messages_excludes_status_only_rows() {
        let summary = summarize_transfer_messages(
            "wangchenfei123",
            vec![transfer_message(
                30,
                5,
                "wangchenfei123",
                "t-orphan",
                "4",
                150_000,
                "kuen133",
                "收到转账1500.00元",
            )],
        );

        assert!(summary.transfers.is_empty());
        assert_eq!(summary.summary.received_total_cents, 0);
        assert_eq!(summary.summary.sent_total_cents, 0);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.excluded_transfers.len(), 1);
        assert_eq!(
            summary.excluded_transfers[0]["reason"].as_str(),
            Some("missing_initiator_card")
        );
        assert_eq!(
            summary.excluded_transfers[0]["transfer_id"].as_str(),
            Some("t-orphan")
        );
    }

    #[test]
    fn summarize_transfer_messages_excludes_refunded_transfers() {
        let summary = summarize_transfer_messages(
            "wangchenfei123",
            vec![
                transfer_message(
                    40,
                    6,
                    "kuen133",
                    "t-refund",
                    "1",
                    200_000,
                    "wangchenfei123",
                    "收到转账2000.00元",
                ),
                transfer_message(
                    41,
                    7,
                    "wangchenfei123",
                    "t-refund",
                    "4",
                    200_000,
                    "kuen133",
                    "收到转账2000.00元",
                ),
            ],
        );

        assert!(summary.transfers.is_empty());
        assert_eq!(summary.summary.received_total_cents, 0);
        assert_eq!(summary.summary.sent_total_cents, 0);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.excluded_transfers.len(), 1);
        assert_eq!(
            summary.excluded_transfers[0]["reason"].as_str(),
            Some("returned_by_receiver")
        );
        assert_eq!(
            summary.excluded_transfers[0]["transfer_id"].as_str(),
            Some("t-refund")
        );
    }

    #[test]
    fn parse_sysmsg_strips_custom_link_markup() {
        let xml = r#"<sysmsg type="paymsg"><paymsg><content><![CDATA[你有一笔待接收的<_wc_custom_link_ href="weixin://">转账</_wc_custom_link_>]]></content></paymsg></sysmsg>"#;
        assert_eq!(
            parse_sysmsg(xml).as_deref(),
            Some("[系统] 你有一笔待接收的转账")
        );
    }

    fn sns_post_xml(
        create_time: &str,
        desc: &str,
        username_tag: Option<&str>,
        media: usize,
        location: Option<&str>,
    ) -> String {
        let username = username_tag
            .map(|u| format!("<username>{}</username>", u))
            .unwrap_or_default();
        let media_tags = "<media><type>2</type></media>".repeat(media);
        let content_object = if media > 0 {
            format!(
                "<ContentObject><mediaList>{}</mediaList></ContentObject>",
                media_tags
            )
        } else {
            String::new()
        };
        let loc = location
            .map(|poi| {
                format!(
                    r#"<location poiName="{}" longitude="0" latitude="0" />"#,
                    poi
                )
            })
            .unwrap_or_default();
        format!(
            "<TimelineObject>{}<createTime>{}</createTime><contentDesc>{}</contentDesc>{}{}</TimelineObject>",
            username, create_time, desc, content_object, loc
        )
    }

    #[test]
    fn parse_post_xml_prefers_column_username_and_extracts_media() {
        let xml = sns_post_xml("1700000002", "post", Some("wxid_xml"), 3, Some("Wuxi"));
        let parsed = parse_post_xml(4, "wxid_column", &xml);
        assert_eq!(parsed.author_username, "wxid_column");
        assert_eq!(parsed.create_time, 1700000002);
        assert_eq!(parsed.content, "post");
        assert_eq!(parsed.location, "Wuxi");
        assert_eq!(parsed.media.len(), 3);
    }

    #[test]
    fn parse_post_xml_falls_back_to_xml_username_when_column_missing() {
        let xml = sns_post_xml("1700000001", "world", Some("wxid_xml_only"), 0, None);
        let parsed = parse_post_xml(2, "", &xml);
        assert_eq!(parsed.author_username, "wxid_xml_only");
    }

    #[test]
    fn parse_post_xml_fallback_decodes_entities_for_broken_xml() {
        let xml = "<TimelineObject><createTime>1700000007</createTime><contentDesc>A &amp; B</contentDesc><location poiName=\"Wuxi &amp; Lake\" /><not valid xml";
        let parsed = parse_post_xml(7, "wxid_fallback", xml);
        assert_eq!(parsed.author_username, "wxid_fallback");
        assert_eq!(parsed.content, "A & B");
        assert_eq!(parsed.location, "Wuxi & Lake");
    }

    #[test]
    fn parse_post_media_extracts_structured_fields() {
        let xml = r#"
<TimelineObject>
  <ContentObject>
    <mediaList>
      <media>
        <type>2</type>
        <url md5="abc" key="uk" token="ut" enc_idx="1">https://example.com/full.jpg</url>
        <thumb key="tk" token="tt" enc_idx="2">https://example.com/thumb.jpg</thumb>
        <size width="1080" height="720" totalSize="2048" />
      </media>
    </mediaList>
  </ContentObject>
</TimelineObject>
"#;
        let media = parse_post_media(xml);
        assert_eq!(media.len(), 1);
        assert_eq!(
            media[0]["url"].as_str(),
            Some("https://example.com/full.jpg")
        );
        assert_eq!(
            media[0]["thumb"].as_str(),
            Some("https://example.com/thumb.jpg")
        );
        assert_eq!(media[0]["width"].as_i64(), Some(1080));
        assert_eq!(media[0]["md5"].as_str(), Some("abc"));
    }
}
