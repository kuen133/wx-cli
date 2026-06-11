pub mod asr_backfill;
pub mod attachments;
pub mod avatars;
pub mod biz_articles;
pub mod contacts;
pub mod daemon_cmd;
pub mod export;
pub mod extract;
pub mod favorites;
pub mod files;
pub mod friend_requests;
pub mod history;
mod init;
pub mod members;
pub mod new_messages;
pub mod output;
pub mod search;
pub mod sessions;
pub mod sns_feed;
pub mod sns_notifications;
pub mod sns_search;
pub mod stats;
pub mod transfers;
pub mod transport;
pub mod unread;

use self::output::OutputOpts;
use anyhow::Result;
use clap::{Parser, Subcommand};

/// wx — 微信本地数据 CLI
#[derive(Parser)]
#[command(name = "wx", version = env!("CARGO_PKG_VERSION"), about = "wx — 微信本地数据 CLI")]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 初始化：检测数据目录并扫描加密密钥
    Init {
        /// 强制重新扫描（覆盖已有配置）
        #[arg(long)]
        force: bool,
    },
    /// 列出最近会话
    Sessions {
        /// 会话数量
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 查看聊天记录
    History {
        /// 聊天对象名称（支持模糊匹配）
        chat: String,
        /// 消息数量
        #[arg(short = 'n', long, default_value = "50")]
        limit: usize,
        /// 分页偏移
        #[arg(long, default_value = "0")]
        offset: usize,
        /// 起始时间 YYYY-MM-DD
        #[arg(long)]
        since: Option<String>,
        /// 结束时间 YYYY-MM-DD
        #[arg(long)]
        until: Option<String>,
        /// 消息类型过滤 [text|image|voice|video|sticker|location|link|file|call|system]
        #[arg(long = "type", value_name = "TYPE",
              value_parser = ["text","image","voice","video","sticker","location","link","file","call","system"])]
        msg_type: Option<String>,
        /// 对尚未缓存的语音消息调用百炼 ASR；已缓存的语音会自动显示文字
        #[arg(long)]
        with_asr: bool,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 查看某个联系人的转账台账与月度汇总
    Transfers {
        /// 联系人名称（支持模糊匹配）
        chat: String,
        /// 快速筛选月份 YYYY-MM（例如 2026-04）
        #[arg(long, conflicts_with_all = ["since", "until"])]
        month: Option<String>,
        /// 起始时间 YYYY-MM-DD
        #[arg(long)]
        since: Option<String>,
        /// 结束时间 YYYY-MM-DD
        #[arg(long)]
        until: Option<String>,
        /// 只输出汇总表，不输出逐笔明细
        #[arg(long)]
        summary_only: bool,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 全量预转写历史语音到本地缓存，后续查聊天记录时可直接显示文字
    #[command(name = "asr-backfill")]
    AsrBackfill {
        /// 起始时间 YYYY-MM-DD
        #[arg(long)]
        since: Option<String>,
        /// 结束时间 YYYY-MM-DD
        #[arg(long)]
        until: Option<String>,
        /// 最多处理前 N 条语音消息（按时间升序）
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// 只统计待处理数量、时长和预估费用，不实际调用在线 ASR
        #[arg(long)]
        dry_run: bool,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 搜索消息
    Search {
        /// 搜索关键词
        keyword: String,
        /// 限定聊天（可多次指定）
        #[arg(long = "in", value_name = "CHAT")]
        chats: Vec<String>,
        /// 结果数量
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
        /// 起始时间 YYYY-MM-DD
        #[arg(long)]
        since: Option<String>,
        /// 结束时间 YYYY-MM-DD
        #[arg(long)]
        until: Option<String>,
        /// 消息类型过滤 [text|image|voice|video|sticker|location|link|file|call|system]
        #[arg(long = "type", value_name = "TYPE",
              value_parser = ["text","image","voice","video","sticker","location","link","file","call","system"])]
        msg_type: Option<String>,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 查看联系人
    Contacts {
        /// 按名字过滤
        #[arg(short = 'q', long)]
        query: Option<String>,
        /// 显示数量
        #[arg(short = 'n', long, default_value = "50")]
        limit: usize,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 导出聊天记录到文件
    Export {
        /// 聊天对象名称
        chat: String,
        /// 起始时间 YYYY-MM-DD
        #[arg(long)]
        since: Option<String>,
        /// 结束时间 YYYY-MM-DD
        #[arg(long)]
        until: Option<String>,
        /// 最多导出条数
        #[arg(short = 'n', long, default_value = "500")]
        limit: usize,
        /// 输出格式 [markdown|txt|json|yaml]
        #[arg(short = 'f', long, default_value = "markdown", value_parser = ["markdown", "txt", "json", "yaml"])]
        format: String,
        /// 输出文件（默认 stdout）
        #[arg(short = 'o', long)]
        output: Option<String>,
    },
    /// 显示有未读消息的会话
    Unread {
        /// 显示数量
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
        /// 按会话类型过滤，逗号分隔。示例：--filter private,group 只看真人的未读
        #[arg(long, value_name = "TYPES", value_delimiter = ',',
              value_parser = ["all", "private", "group", "official", "folded"])]
        filter: Vec<String>,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 查看群成员
    Members {
        /// 群聊名称（支持模糊匹配）
        chat: String,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 获取自上次检查以来的新消息
    NewMessages {
        /// 显示数量上限
        #[arg(short = 'n', long, default_value = "200")]
        limit: usize,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 聊天统计分析
    Stats {
        /// 聊天对象名称（支持模糊匹配）
        chat: String,
        /// 起始时间 YYYY-MM-DD
        #[arg(long)]
        since: Option<String>,
        /// 结束时间 YYYY-MM-DD
        #[arg(long)]
        until: Option<String>,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 查看微信收藏内容
    Favorites {
        /// 显示数量
        #[arg(short = 'n', long, default_value = "50")]
        limit: usize,
        /// 类型过滤 [text|image|article|card|video]
        #[arg(long = "type", value_name = "TYPE",
              value_parser = ["text","image","article","card","video"])]
        fav_type: Option<String>,
        /// 内容关键词搜索
        #[arg(short = 'q', long)]
        query: Option<String>,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 列出或导出微信头像
    Avatars {
        /// 精确匹配联系人/群 username
        #[arg(long)]
        username: Option<String>,
        /// 导出目录；未指定时只列头像元数据
        #[arg(long)]
        out: Option<String>,
        /// 限制返回/导出条数
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 列出本地文件/图片/视频索引
    Files {
        /// 类型过滤 [image|video|file]
        #[arg(long = "type", value_name = "TYPE",
              value_parser = ["image","video","file"])]
        file_type: Option<String>,
        /// 限制返回条数
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// 起始 modify_time（unix 秒，或 YYYY-MM-DD / YYYY-MM-DD HH:MM）
        #[arg(long)]
        since: Option<String>,
        /// 结束 modify_time（unix 秒，或 YYYY-MM-DD / YYYY-MM-DD HH:MM）
        #[arg(long)]
        until: Option<String>,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 朋友圈互动通知：别人对我的朋友圈点赞/评论 + 我评过的帖子下的跟帖
    #[command(name = "moments-inbox", alias = "sns-notifications")]
    SnsNotifications {
        /// 显示数量
        #[arg(short = 'n', long, default_value = "50")]
        limit: usize,
        /// 起始时间 YYYY-MM-DD
        #[arg(long)]
        since: Option<String>,
        /// 结束时间 YYYY-MM-DD
        #[arg(long)]
        until: Option<String>,
        /// 包含已读通知（默认仅未读）
        #[arg(long)]
        include_read: bool,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 朋友圈时间线：按时间/作者筛选本地缓存的朋友圈
    #[command(name = "moments", alias = "sns-feed")]
    SnsFeed {
        /// 显示数量
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
        /// 起始时间 YYYY-MM-DD
        #[arg(long)]
        since: Option<String>,
        /// 结束时间 YYYY-MM-DD
        #[arg(long)]
        until: Option<String>,
        /// 只看指定作者（昵称 / 备注名 / 微信 ID，模糊匹配）
        #[arg(long)]
        user: Option<String>,
        /// 关键词搜索（匹配正文）
        #[arg(short = 'q', long)]
        query: Option<String>,
        /// 附带媒体 URL 列表（兼容参数；JSON 输出已包含结构化媒体字段）
        #[arg(long)]
        with_media: bool,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 查询公众号文章推送（本地缓存）
    BizArticles {
        /// 显示数量
        #[arg(short = 'n', long, default_value = "50")]
        limit: usize,
        /// 限定公众号（名称模糊匹配）
        #[arg(long)]
        account: Option<String>,
        /// 起始时间 YYYY-MM-DD
        #[arg(long)]
        since: Option<String>,
        /// 结束时间 YYYY-MM-DD
        #[arg(long)]
        until: Option<String>,
        /// 只看有未读的公众号，每个公众号取最新 1 篇
        #[arg(long)]
        unread: bool,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 朋友圈全文搜索：匹配正文关键词
    SnsSearch {
        /// 关键词
        keyword: String,
        /// 结果数量
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
        /// 起始时间 YYYY-MM-DD
        #[arg(long)]
        since: Option<String>,
        /// 结束时间 YYYY-MM-DD
        #[arg(long)]
        until: Option<String>,
        /// 限定作者（昵称 / 备注名 / 微信 ID）
        #[arg(long)]
        user: Option<String>,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 列出某会话的图片附件，返回不透明 attachment_id
    Attachments {
        /// 会话名称（联系人显示名 / wxid / @chatroom username 都可以）
        chat: String,
        /// 类型（当前仅支持 image）
        #[arg(long = "kind", value_name = "KIND",
              value_parser = ["image", "img"])]
        kinds: Vec<String>,
        /// 显示数量
        #[arg(short = 'n', long, default_value = "50")]
        limit: usize,
        /// 分页偏移
        #[arg(long, default_value = "0")]
        offset: usize,
        /// 起始时间 YYYY-MM-DD
        #[arg(long)]
        since: Option<String>,
        /// 结束时间 YYYY-MM-DD
        #[arg(long)]
        until: Option<String>,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 把单个 attachment_id 对应的资源解密写到指定文件路径
    Extract {
        /// 由 `wx attachments` 输出的不透明 ID（base64url 字符串）
        attachment_id: String,
        /// 输出文件路径（绝对或相对当前工作目录均可；扩展名建议保留为 .jpg 等）
        #[arg(short = 'o', long)]
        output: String,
        /// 目标已存在时覆盖
        #[arg(long)]
        overwrite: bool,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 好友申请历史
    #[command(name = "friend-requests")]
    FriendRequests {
        /// 显示数量
        #[arg(short = 'n', long, default_value = "50")]
        limit: usize,
        /// 起始时间 YYYY-MM-DD
        #[arg(long)]
        since: Option<String>,
        /// 结束时间 YYYY-MM-DD
        #[arg(long)]
        until: Option<String>,
        /// 方向过滤：incoming / outgoing（默认全部）
        #[arg(long, value_parser = ["incoming", "outgoing", "received", "sent"])]
        direction: Option<String>,
        /// 输出 JSON（默认 YAML）
        #[arg(long)]
        json: bool,
    },
    /// 管理 wx-daemon
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCommands,
    },
}

#[derive(Subcommand)]
pub enum DaemonCommands {
    /// 查看 daemon 运行状态
    Status,
    /// 停止 daemon
    Stop,
    /// 查看 daemon 日志
    Logs {
        /// 持续输出（tail -f）
        #[arg(short = 'f', long)]
        follow: bool,
        /// 显示最近 N 行
        #[arg(short = 'n', long, default_value = "50")]
        lines: usize,
    },
}

pub fn run() {
    let cli = Cli::parse();
    if let Err(e) = dispatch(cli) {
        eprintln!("错误: {}", e);
        std::process::exit(1);
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Init { force } => init::cmd_init(force),
        Commands::Sessions { limit, json } => sessions::cmd_sessions(limit, json),
        Commands::History {
            chat,
            limit,
            offset,
            since,
            until,
            msg_type,
            with_asr,
            json,
        } => history::cmd_history(chat, limit, offset, since, until, msg_type, with_asr, json),
        Commands::Transfers {
            chat,
            month,
            since,
            until,
            summary_only,
            json,
        } => transfers::cmd_transfers(chat, month, since, until, summary_only, json),
        Commands::AsrBackfill {
            since,
            until,
            limit,
            dry_run,
            json,
        } => asr_backfill::cmd_asr_backfill(limit, since, until, dry_run, json),
        Commands::Search {
            keyword,
            chats,
            limit,
            since,
            until,
            msg_type,
            json,
        } => search::cmd_search(keyword, chats, limit, since, until, msg_type, json),
        Commands::Contacts { query, limit, json } => contacts::cmd_contacts(query, limit, json),
        Commands::Export {
            chat,
            since,
            until,
            limit,
            format,
            output,
        } => export::cmd_export(chat, since, until, limit, format, output),
        Commands::Unread {
            limit,
            filter,
            json,
        } => unread::cmd_unread(limit, filter, json),
        Commands::Members { chat, json } => members::cmd_members(chat, json),
        Commands::NewMessages { limit, json } => new_messages::cmd_new_messages(limit, json),
        Commands::Stats {
            chat,
            since,
            until,
            json,
        } => stats::cmd_stats(chat, since, until, json),
        Commands::Favorites {
            limit,
            fav_type,
            query,
            json,
        } => favorites::cmd_favorites(limit, fav_type, query, json),
        Commands::Avatars {
            username,
            out,
            limit,
            json,
        } => avatars::cmd_avatars(username, out, limit, json),
        Commands::Files {
            file_type,
            limit,
            since,
            until,
            json,
        } => files::cmd_files(file_type, limit, since, until, json),
        Commands::SnsNotifications {
            limit,
            since,
            until,
            include_read,
            json,
        } => sns_notifications::cmd_sns_notifications(limit, since, until, include_read, json),
        Commands::SnsFeed {
            limit,
            since,
            until,
            user,
            query,
            with_media: _,
            json,
        } => {
            if let Some(keyword) = query {
                sns_search::cmd_sns_search(keyword, limit, since, until, user, json)
            } else {
                sns_feed::cmd_sns_feed(limit, since, until, user, json)
            }
        }
        Commands::BizArticles {
            limit,
            account,
            since,
            until,
            unread,
            json,
        } => biz_articles::cmd_biz_articles(limit, account, since, until, unread, json),
        Commands::SnsSearch {
            keyword,
            limit,
            since,
            until,
            user,
            json,
        } => sns_search::cmd_sns_search(keyword, limit, since, until, user, json),
        Commands::Attachments {
            chat,
            kinds,
            limit,
            offset,
            since,
            until,
            json,
        } => attachments::cmd_attachments(
            chat,
            kinds,
            limit,
            offset,
            since,
            until,
            OutputOpts {
                json,
                with_meta: false,
                debug_source: false,
            },
        ),
        Commands::Extract {
            attachment_id,
            output,
            overwrite,
            json,
        } => extract::cmd_extract(attachment_id, output, overwrite, json),
        Commands::FriendRequests {
            limit,
            since,
            until,
            direction,
            json,
        } => friend_requests::cmd_friend_requests(limit, since, until, direction, json),
        Commands::Daemon { cmd } => daemon_cmd::cmd_daemon(cmd),
    }
}
