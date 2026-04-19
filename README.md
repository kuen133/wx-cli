<div align="center">

# wx-cli

**从命令行查询本地微信数据**

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey.svg)](#安装)
[![Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org)

会话 · 聊天记录 · 搜索 · 联系人 · 群成员 · 收藏 · 统计 · 导出 · **朋友圈**

</div>

---

## 🍴 这是一个 fork

Forked from **[jackwener/wx-cli](https://github.com/jackwener/wx-cli)** @ `v0.1.9` (commit `697d3fc`)，遵循 upstream 的 Apache-2.0 协议。感谢原作者的工作 🙏

**本 fork 相对 upstream 的改动：**

| 类型 | 改动 |
|---|---|
| 🐛 fix | WeChat 4.1.x 多账号场景下，upstream 检测错账号目录 → 修成按 `db_storage/session/` 内文件的最新 mtime 排 |
| 🐛 fix | upstream 把 `biz_message_*.db` 过滤掉了，导致 `wx history "公众号名"` 报"找不到消息记录" → 加回来 |
| 🐛 fix | 链接标题输出被 `<![CDATA[...]]>` 包着 → 剥掉 |
| ✨ feat | `wx moments` 查朋友圈（文字/媒体/过滤） |
| ✨ feat | `wx moments-inbox` 别人评论/点赞我的通知 |
| ✨ feat | `wx friend-requests` 好友申请历史（申请话术、来源、方向） |
| ⚡ perf | `wx search` 25-40× 提速：自建 trigram FTS5 索引（upstream 用 LIKE 全表扫；微信的 `message_fts.db` 用私有 `MMFtsTokenizer`，标准 SQLite 打不开） |

看详细变更：[commit log](../../commits) · [原始 upstream](https://github.com/jackwener/wx-cli)

---

## AI Agent Skill

通过 [skills CLI](https://github.com/vercel-labs/skills) 一键安装到 Claude Code、Cursor、Codex 等 agent：

```bash
npx skills add jackwener/wx-cli
```

或全局安装：

```bash
npx skills add jackwener/wx-cli -g
```

安装后 agent 会自动读取 `SKILL.md`，了解如何安装和调用 wx-cli。

---

## 特性

- **零依赖安装** — 单一 Rust 二进制，一行命令装完
- **毫秒级响应** — 后台 daemon 持久缓存解密数据库，mtime 不变则复用
- **AI 友好** — 默认 YAML 输出，更省 token & 易读；`--json` 可切换为 JSON（方便 `jq` 处理等）
- **完全本地** — 数据不出本机，实时解密，无需全量预解密

---

## 安装

**npm（推荐，全平台）**

```bash
npm install -g @jackwener/wx-cli
```

**macOS / Linux（curl）**

```bash
curl -fsSL https://raw.githubusercontent.com/jackwener/wx-cli/main/install.sh | bash
```

**Windows**（PowerShell，以管理员身份运行）

```powershell
irm https://raw.githubusercontent.com/jackwener/wx-cli/main/install.ps1 | iex
```

<details>
<summary>其他安装方式</summary>

**手动下载**

从 [Releases](https://github.com/jackwener/wx-cli/releases) 下载对应平台文件：

| 平台 | 文件 |
|------|------|
| macOS Apple Silicon | `wx-macos-arm64` |
| macOS Intel | `wx-macos-x86_64` |
| Linux x86_64 | `wx-linux-x86_64` |
| Linux arm64 | `wx-linux-arm64` |
| Windows x86_64 | `wx-windows-x86_64.exe` |

macOS / Linux：`chmod +x wx && sudo mv wx /usr/local/bin/`

**从源码构建**

```bash
git clone git@github.com:jackwener/wx-cli.git && cd wx-cli
cargo build --release
# 产物：target/release/wx（Windows: wx.exe）
```

</details>

---

## 快速开始

保持微信运行，然后初始化（只需一次）：

**macOS**（需要先对微信做 ad-hoc 签名，才能扫描其内存）

```bash
# 1. 签名（只需做一次，WeChat 更新后重做）
codesign --force --deep --sign - /Applications/WeChat.app

# 2. 重启微信，等待完全登录
killall WeChat && open /Applications/WeChat.app

# 3. 初始化
sudo wx init
```

> 如果 `codesign` 报 `signature in use`，先执行：
> ```bash
> codesign --remove-signature "/Applications/WeChat.app/Contents/Frameworks/vlc_plugins/librtp_mpeg4_plugin.dylib"
> codesign --force --deep --sign - /Applications/WeChat.app
> ```

**Linux**

```bash
sudo wx init
```

**Windows**（以管理员身份运行 PowerShell）

```powershell
wx init
```

验证安装：

```bash
wx sessions
```

能看到最近会话即表示一切正常。daemon 在首次调用时自动启动。

---

## 命令

### 消息

```bash
wx sessions                                      # 最近 20 个会话
wx unread                                        # 有未读消息的会话
wx unread --filter private,group                 # 只看真人未读（过滤公众号/折叠入口）
wx new-messages                                  # 上次检查后的新消息（增量）
wx history "张三"                                # 最近 50 条记录
wx history "AI群" --since 2026-04-01 --until 2026-04-15
wx history "张三" --type voice --with-asr        # 把未缓存语音补转文字，之后普通 history 也能直接显示
wx asr-backfill --dry-run                        # 先统计历史语音条数、总时长和预估费用
wx asr-backfill                                  # 全量预转写历史语音到本地缓存
wx transfers "张三"                              # 转账台账：逐笔记录 + 汇总
wx transfers "张三" --month 2026-04            # 快速看某个月
wx transfers "张三" --summary-only             # 只看汇总表
wx transfers "张三" --since 2026-01-01          # 自定义时间范围
wx search "关键词"                               # 全库搜索
wx search "会议" --in "工作群" --since 2026-01-01
```

会话/消息输出里都带 `chat_type` 字段，取值为 `private` / `group` / `official_account` / `folded`。`official_account` 涵盖公众号、订阅号、服务号及 `mphelper` / `qqsafe` 等系统通知；`folded` 对应微信里的"订阅号折叠"和"折叠群聊"两个聚合入口。

`wx transfers` 会自动去重同一笔转账在聊天里出现的多条卡片，输出逐笔 `transfers`、总汇总 `summary`，以及按月聚合的 `monthly_rows`。适合直接查询“这个月谁给我转了多少 / 我给他转了多少”。

`wx history` 会优先读取本地语音转写缓存；如果某条语音已经预转写过，普通聊天记录输出里就会直接显示 `[语音] 转写文本`。

`wx history --with-asr` 会在遇到尚未缓存的语音消息时，从微信的 `media_*.db` 里直接读取语音 blob，自动解码成 WAV，再调用百炼 `Qwen3-ASR-Flash` 转写。转写结果会缓存在 `~/.wx-cli/cache/_voice_asr.db`，同一条语音下次读取通常不需要再次请求模型。

`wx asr-backfill` 用来一次性把历史聊天里的语音批量预转写到本地缓存。建议先跑 `wx asr-backfill --dry-run` 看待处理条数、总时长和预估费用，再决定要不要正式执行。

首次使用 ASR 时会本地编译一个 `silk -> wav` helper，因此机器上需要有 `Go`；调用百炼脚本时需要 `python3`。如果终端没有显式设置 `DASHSCOPE_API_KEY`，脚本会继续尝试读取本机 AI Hub 里的 `aliyun_bailian_main` 配置。

### 联系人 & 群组

```bash
wx contacts                  # 联系人列表
wx contacts --query "李"     # 按名字搜索
wx members "AI交流群"        # 群成员列表
```

### 收藏 & 统计

```bash
wx favorites                          # 全部收藏
wx favorites --type image             # 按类型筛选（text/image/article/card/video）
wx favorites --query "关键词"         # 搜索收藏内容
wx stats "AI群"                       # 聊天统计
wx stats "AI群" --since 2026-01-01   # 指定时间范围
```

### 导出

```bash
wx export "张三" --format markdown -o chat.md
wx export "AI群" --since 2026-01-01 --format json
```

### 输出格式

默认输出 YAML，更省 token & 易读；`--json` 可切换为 JSON（方便 `jq` 处理等）：

```bash
wx sessions --json
wx search "关键词" --json | jq '.[0].content'
wx new-messages --json
```

### Daemon 管理

```bash
wx daemon status
wx daemon stop
wx daemon logs --follow
```

### 百炼语音转文字冒烟测试

如果你想先验证阿里云百炼 `Qwen3-ASR-Flash` 能不能识别微信语音，再决定要不要把 ASR 正式接进 `wx-cli`，可以先跑仓库里的最小测试脚本：

```bash
export DASHSCOPE_API_KEY=sk-xxxx
python3 examples/qwen3_asr_flash_smoketest.py /absolute/path/to/voice.amr --language zh --enable-itn
```

默认走北京地域 OpenAI 兼容地址 `https://dashscope.aliyuncs.com/compatible-mode/v1`。如果你用新加坡地域，把 `--base-url` 改成 `https://dashscope-intl.aliyuncs.com/compatible-mode/v1`。

如果当前终端没有 `DASHSCOPE_API_KEY`，脚本也会尝试从本机 AI Hub 配置里的 `aliyun_bailian_main` provider 自动读取百炼 key。

这个脚本会把本地音频编码成 Data URL 后直接发给 `qwen3-asr-flash`，适合验证微信短语音是否能被正确转写。对于大于 `10 MB` 或明显偏长的录音，建议改用 `qwen3-asr-flash-filetrans`。

---

## 架构

```
wx (CLI) ──Unix socket──▶ wx-daemon (后台进程)
                              │
                    ┌─────────┴──────────┐
               DBCache               联系人缓存
           (mtime 感知复用)
```

daemon 首次解密后将数据库和 mtime 持久化到 `~/.wx-cli/cache/`。重启后 mtime 未变则直接复用，无需重解密。

```
~/.wx-cli/
├── config.json       # 配置
├── all_keys.json     # 数据库密钥
├── daemon.sock       # Unix socket
├── daemon.pid / .log
└── cache/
    ├── _mtimes.json  # mtime 索引
    └── *.db          # 解密后的数据库
```

---

## 原理

微信 4.x 使用 SQLCipher 4 加密本地数据库（AES-256-CBC + HMAC-SHA512，PBKDF2 256,000 次迭代）。WCDB 在进程内存中缓存派生后的 raw key，格式为 `x'<64hex_key><32hex_salt>'`。

wx-cli 通过 macOS Mach VM API（`mach_vm_region` + `mach_vm_read`）或 Linux `/proc/<pid>/mem` 扫描微信进程内存，匹配该模式提取密钥，daemon 按需解密并缓存。

---

## 致谢

本项目受 [ylytdeng/wechat-decrypt](https://github.com/ylytdeng/wechat-decrypt) 启发，在其基础上进行了重新设计与实现。感谢原作者的研究与探索。

---

## 免责声明

本工具仅用于学习和研究目的，用于解密**自己的**微信数据。请遵守相关法律法规，不得用于未经授权的数据访问。
