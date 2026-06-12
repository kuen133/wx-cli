use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct NewMessageCursor {
    pub create_time: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NewMessageCursorWire {
    Legacy(i64),
    Current {
        create_time: i64,
        #[serde(default)]
        local_id: Option<i64>,
    },
}

impl<'de> Deserialize<'de> for NewMessageCursor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match NewMessageCursorWire::deserialize(deserializer)? {
            NewMessageCursorWire::Legacy(create_time) => Self { create_time, local_id: None },
            NewMessageCursorWire::Current { create_time, local_id } => Self { create_time, local_id },
        })
    }
}

/// CLI 向 daemon 发送的请求（换行符分隔 JSON，与 Python 版兼容）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Sessions {
        #[serde(default = "default_limit_20")]
        limit: usize,
    },
    History {
        chat: String,
        #[serde(default = "default_limit_50")]
        limit: usize,
        #[serde(default)]
        offset: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        msg_type: Option<i64>,
        #[serde(default)]
        with_asr: bool,
    },
    Transfers {
        chat: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<i64>,
    },
    Redpackets {
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    TransferEvents {
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<i64>,
    },
    Search {
        keyword: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        chats: Option<Vec<String>>,
        #[serde(default = "default_limit_20")]
        limit: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        msg_type: Option<i64>,
    },
    Contacts {
        #[serde(skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        #[serde(default = "default_limit_50")]
        limit: usize,
    },
    Unread {
        #[serde(default = "default_limit_20")]
        limit: usize,
        /// 按会话类型过滤：private / group / official / folded / all，支持多选
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<Vec<String>>,
    },
    Members {
        chat: String,
    },
    NewMessages {
        /// 上次检查时各会话的消息游标（兼容旧格式 username -> ts）
        /// None 表示首次运行，会返回 new_state 供下次使用
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<HashMap<String, NewMessageCursor>>,
        #[serde(default = "default_limit_200")]
        limit: usize,
    },
    Stats {
        chat: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<i64>,
    },
    Favorites {
        #[serde(default = "default_limit_50")]
        limit: usize,
        /// 类型过滤：1=文本,2=图片,5=文章,19=名片,20=视频
        #[serde(skip_serializing_if = "Option::is_none")]
        fav_type: Option<i64>,
        /// 内容关键词搜索
        #[serde(skip_serializing_if = "Option::is_none")]
        query: Option<String>,
    },
    Avatars {
        /// 精确匹配联系人/群 username
        #[serde(skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        /// 导出目录；未指定时只列元数据
        #[serde(skip_serializing_if = "Option::is_none")]
        out: Option<String>,
        /// 限制返回/导出条数；未指定时不限制
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    Files {
        /// 类型过滤：image / video / file
        #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
        file_type: Option<String>,
        /// 限制返回条数；未指定时不限制
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<i64>,
    },
    /// 朋友圈互动通知（点赞 + 评论）
    SnsNotifications {
        #[serde(default = "default_limit_50")]
        limit: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<i64>,
        /// 包含已读通知（默认仅未读）
        #[serde(default)]
        include_read: bool,
    },
    /// 朋友圈时间线（按时间 / 作者筛选帖子）
    SnsFeed {
        #[serde(default = "default_limit_20")]
        limit: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<i64>,
        /// 作者昵称 / 备注名 / 微信 username，模糊匹配
        #[serde(skip_serializing_if = "Option::is_none")]
        user: Option<String>,
    },
    /// 查询公众号文章推送（biz_message_0.db）
    BizArticles {
        #[serde(default = "default_limit_50")]
        limit: usize,
        /// 公众号名称过滤（模糊匹配 display name，None = 全部）
        #[serde(skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<i64>,
        /// 只看有未读消息的公众号，每个公众号取最新 1 篇
        #[serde(default)]
        unread: bool,
    },
    /// 朋友圈全文搜索（匹配 contentDesc）
    SnsSearch {
        keyword: String,
        #[serde(default = "default_limit_20")]
        limit: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        user: Option<String>,
    },
    /// 列出某个会话里的图片附件
    /// 输出每条带 `attachment_id`（不透明 base64url 句柄），传给 `Extract` 时取回本体
    Attachments {
        chat: String,
        /// 类型过滤：当前仅支持 image
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kinds: Option<Vec<String>>,
        #[serde(default = "default_limit_50")]
        limit: usize,
        #[serde(default)]
        offset: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<i64>,
        #[serde(default, skip_serializing_if = "is_false")]
        with_meta: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        debug_source: bool,
    },
    /// 提取（解密）单个附件的本体到指定路径
    Extract {
        /// `Attachments` 返回的不透明 ID
        attachment_id: String,
        /// 写入的绝对路径（daemon 直接写盘，不经 socket 传 binary）
        output: String,
        /// 已存在时是否覆盖
        #[serde(default)]
        overwrite: bool,
    },
    FriendRequests {
        #[serde(default = "default_limit_50")]
        limit: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<i64>,
        /// 只看自己发出的 / 收到的（默认全部）：incoming / outgoing
        #[serde(skip_serializing_if = "Option::is_none")]
        direction: Option<String>,
    },
}

/// daemon 的响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(flatten)]
    pub data: Value,
}

impl Response {
    pub fn ok(data: Value) -> Self {
        Self {
            ok: true,
            error: None,
            data,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            data: Value::Null,
        }
    }

    pub fn to_json_line(&self) -> anyhow::Result<String> {
        let s = serde_json::to_string(self)?;
        Ok(s + "\n")
    }
}

fn default_limit_20() -> usize {
    20
}
fn default_limit_50() -> usize {
    50
}
fn default_limit_200() -> usize {
    200
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_messages_request_accepts_legacy_second_state() {
        let req: Request = serde_json::from_str(
            r#"{"cmd":"new_messages","state":{"room@chatroom":1775146911},"limit":10}"#,
        )
        .expect("legacy state should parse");
        let Request::NewMessages { state, limit } = req else { panic!("wrong request variant") };
        assert_eq!(limit, 10);
        assert_eq!(
            state.unwrap().get("room@chatroom").copied(),
            Some(NewMessageCursor { create_time: 1_775_146_911, local_id: None })
        );
    }
}

fn is_false(v: &bool) -> bool {
    !*v
}
