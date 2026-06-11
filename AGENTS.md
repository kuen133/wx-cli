# wx-cli Agent Rules

## 每次改完代码后必须做的事

1. **`cargo check`** — 改任何 `.rs` 文件后立刻运行，不通过不提交
2. **改了跨平台代码时加运行跨平台 check：**
   ```bash
   cargo check --target x86_64-unknown-linux-gnu
   cargo check --target x86_64-pc-windows-msvc
   ```
3. **改了 `Cargo.toml` 版本号时：** `cargo update --workspace`

## 禁止行为

- 不能在 `cargo check` 失败的情况下 commit
- 不能只在 macOS 本地 check 就认为跨平台没问题
- 不能改完 `Cargo.toml` 不更新 `Cargo.lock` 就打 tag

## 常见陷阱

| 陷阱 | 正确做法 |
|------|----------|
| `libc::__error()` 在 `#[cfg(unix)]` 里 | 用 `std::io::Error::last_os_error()` |
| 把通用 dep 放到 `[target.cfg(windows).dependencies]` 后面 | TOML section 是贪婪的，通用 dep 必须在 target section 之前 |
| 改版本号忘更新 Cargo.lock | `cargo update --workspace` |
| Windows 代码用 trait method 忘 import trait | `use std::os::windows::process::CommandExt` 等 |
| `#[cfg(windows)]` 里引用了未定义的函数 | 跨平台 check 会发现 |

## Push 规则

- remote 名称：`wx-cli`，使用 SSH
- 每次 commit 后立刻 push
- 打 tag 用 `git tag vX.Y.Z && git push wx-cli vX.Y.Z`

## AIHub / 局域网AI总表

当用户提到 AIHub、局域网AI总表、家庭AI总表、AI总表、局域网AI配置表、大模型、provider、API key、调用模型、Gemini、Kimi、MiniMax、百炼、智谱、302.AI、OpenRouter、ChatGPT Pro、Claude Code、电脑、主机、SSH、NAS、M4、Windows、Proxmox、fnOS、远程访问时，第一步先读取本机 AIHub 注册表。

优先路径：

1. `$LAN_AI_REGISTRY_PATH`
2. `~/.config/局域网AI总表.json`
3. `~/.config/aihub/aihub.json`
4. `~/Library/Application Support/AIHub/aihub.json`

读取时只取需要的 subtree：

- 模型/API：`.catalog.providers`
- 主机/电脑/远程访问：`.catalog.hosts`
- 路由规则：对应 provider 的 `routing_guidance`、`routing_notes`、`notes`
- 注册表发现规则：`.discovery_contract`

默认不要在回答中明文展示 `api_key`、`password`、`token`、OAuth、私钥或其他密钥。需要调用模型或访问主机时，可以读取密钥用于本地命令；除非用户明确要求展示密钥，否则输出必须脱敏。远程破坏性操作前先确认。

优先使用 `aihub` CLI：

```bash
aihub overview
aihub providers
aihub provider ai302_main
aihub hosts
aihub host m4-mac
aihub chat --provider ai302_main --prompt "hello"
aihub ssh m4-mac -- hostname
```
