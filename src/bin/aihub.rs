use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(name = "aihub", about = "Read and use the local AIHub / 局域网AI总表 registry")]
struct Cli {
    #[arg(long, global = true, value_name = "PATH")]
    registry: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Print the resolved AIHub registry path.
    Path,
    /// Print a compact registry summary.
    Overview(JsonArgs),
    /// List known hosts.
    Hosts(JsonArgs),
    /// Show one host entry.
    Host(EntryArgs),
    /// List known model/API providers.
    Providers(JsonArgs),
    /// Show one provider entry.
    Provider(EntryArgs),
    /// Search host and provider entries.
    Search(SearchArgs),
    /// Call a model provider from the registry.
    Chat(ChatArgs),
    /// Run a command on a registered SSH host.
    Ssh(SshArgs),
}

#[derive(Args)]
struct JsonArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct EntryArgs {
    id: String,

    #[arg(long)]
    json: bool,

    #[arg(long)]
    reveal_secrets: bool,
}

#[derive(Args)]
struct SearchArgs {
    query: String,

    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ChatArgs {
    #[arg(long)]
    provider: String,

    #[arg(long)]
    model: Option<String>,

    #[arg(long)]
    prompt: String,

    #[arg(long)]
    raw: bool,
}

#[derive(Args)]
struct SshArgs {
    host_id: String,

    /// Open an interactive SSH session. Without this, an empty command defaults to `hostname`.
    #[arg(long)]
    interactive: bool,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("aihub: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let (path, registry) = load_registry(cli.registry.as_deref())?;

    match cli.command {
        Commands::Path => println!("{}", path.display()),
        Commands::Overview(args) => print_overview(&path, &registry, args.json)?,
        Commands::Hosts(args) => print_hosts(&registry, args.json)?,
        Commands::Host(args) => print_entry(&registry, "/catalog/hosts", &args.id, args.reveal_secrets)?,
        Commands::Providers(args) => print_providers(&registry, args.json)?,
        Commands::Provider(args) => {
            print_entry(&registry, "/catalog/providers", &args.id, args.reveal_secrets)?
        }
        Commands::Search(args) => search_registry(&registry, &args.query, args.json)?,
        Commands::Chat(args) => chat(&registry, args)?,
        Commands::Ssh(args) => ssh(&registry, args)?,
    }

    Ok(())
}

fn load_registry(explicit_path: Option<&Path>) -> Result<(PathBuf, Value)> {
    let path = resolve_registry_path(explicit_path)?;
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read registry {}", path.display()))?;
    let registry = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse registry JSON {}", path.display()))?;
    Ok((path, registry))
}

fn resolve_registry_path(explicit_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit_path {
        let expanded = expand_path(path);
        if expanded.exists() {
            return Ok(expanded);
        }
        bail!("explicit registry path does not exist: {}", expanded.display());
    }

    let mut candidates = Vec::new();
    if let Ok(path) = env::var("LAN_AI_REGISTRY_PATH") {
        candidates.push(PathBuf::from(path));
    }
    candidates.extend([
        PathBuf::from("~/.config/局域网AI总表.json"),
        PathBuf::from("~/.config/aihub/aihub.json"),
        PathBuf::from("~/Library/Application Support/AIHub/aihub.json"),
        PathBuf::from("/opt/FN_90_System/AppData/AIRegistry/局域网AI总表.json"),
    ]);

    for candidate in candidates {
        let expanded = expand_path(&candidate);
        if expanded.exists() {
            return Ok(expanded);
        }
    }

    bail!(
        "AIHub registry not found; set LAN_AI_REGISTRY_PATH or create ~/.config/局域网AI总表.json"
    );
}

fn expand_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = text.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(text.as_ref())
}

fn print_overview(path: &Path, registry: &Value, as_json: bool) -> Result<()> {
    let hosts = section_object(registry, "/catalog/hosts")?;
    let providers = section_object(registry, "/catalog/providers")?;
    let summary = json!({
        "path": path,
        "display_name": value_str(registry, "/registry_identity/preferred_display_name").unwrap_or("AIHub"),
        "schema_name": value_str(registry, "/schema_name"),
        "version": registry.pointer("/version").cloned().unwrap_or(Value::Null),
        "updated_at": value_str(registry, "/updated_at"),
        "hosts": hosts.len(),
        "providers": providers.len(),
    });

    if as_json {
        print_json(&summary)?;
        return Ok(());
    }

    println!(
        "{}",
        value_str(&summary, "/display_name").unwrap_or("AIHub")
    );
    println!("path: {}", path.display());
    if let Some(schema_name) = value_str(&summary, "/schema_name") {
        println!("schema: {schema_name}");
    }
    if let Some(updated_at) = value_str(&summary, "/updated_at") {
        println!("updated_at: {updated_at}");
    }
    println!("hosts: {}", hosts.len());
    println!("providers: {}", providers.len());
    Ok(())
}

fn print_hosts(registry: &Value, as_json: bool) -> Result<()> {
    let entries = sorted_entries(registry, "/catalog/hosts")?;
    if as_json {
        let values = entries
            .iter()
            .map(|(id, host)| host_summary(id, host))
            .collect::<Vec<_>>();
        print_json(&Value::Array(values))?;
        return Ok(());
    }

    for (id, host) in entries {
        let name = value_str(host, "/display_name").unwrap_or(id);
        let os = value_str(host, "/platform/os").unwrap_or("-");
        let network_host = value_str(host, "/network/host").unwrap_or("-");
        let status = value_str(host, "/verification/status").unwrap_or("unknown");
        let method = value_str(host, "/access/primary_method").unwrap_or("-");
        println!("{id}\t{name}\t{network_host}\t{os}\t{method}\t{status}");
    }
    Ok(())
}

fn print_providers(registry: &Value, as_json: bool) -> Result<()> {
    let entries = sorted_entries(registry, "/catalog/providers")?;
    if as_json {
        let values = entries
            .iter()
            .map(|(id, provider)| provider_summary(id, provider))
            .collect::<Vec<_>>();
        print_json(&Value::Array(values))?;
        return Ok(());
    }

    for (id, provider) in entries {
        let name = value_str(provider, "/display_name").unwrap_or(id);
        let provider_type = value_str(provider, "/type")
            .or_else(|| value_str(provider, "/service"))
            .unwrap_or("-");
        let model = preferred_model(provider, None).unwrap_or_else(|| "-".to_string());
        let status = value_str(provider, "/last_test_status").unwrap_or("-");
        println!("{id}\t{name}\t{provider_type}\t{model}\t{status}");
    }
    Ok(())
}

fn print_entry(registry: &Value, section: &str, id: &str, reveal_secrets: bool) -> Result<()> {
    let entry = entry(registry, section, id)?;
    let output = if reveal_secrets {
        entry.clone()
    } else {
        sanitize(entry)
    };
    print_json(&output)
}

fn search_registry(registry: &Value, query: &str, as_json: bool) -> Result<()> {
    let needle = query.to_lowercase();
    let mut matches = Vec::new();

    for (kind, section) in [("host", "/catalog/hosts"), ("provider", "/catalog/providers")] {
        for (id, value) in sorted_entries(registry, section)? {
            let sanitized = sanitize(value);
            let haystack = serde_json::to_string(&sanitized)?.to_lowercase();
            if id.to_lowercase().contains(&needle) || haystack.contains(&needle) {
                matches.push(json!({
                    "kind": kind,
                    "id": id,
                    "display_name": value_str(value, "/display_name"),
                }));
            }
        }
    }

    if as_json {
        print_json(&Value::Array(matches))?;
        return Ok(());
    }

    if matches.is_empty() {
        println!("no matches");
        return Ok(());
    }

    for item in matches {
        let kind = value_str(&item, "/kind").unwrap_or("-");
        let id = value_str(&item, "/id").unwrap_or("-");
        let display_name = value_str(&item, "/display_name").unwrap_or("-");
        println!("{kind}\t{id}\t{display_name}");
    }
    Ok(())
}

fn chat(registry: &Value, args: ChatArgs) -> Result<()> {
    let provider = entry(registry, "/catalog/providers", &args.provider)?;
    let provider_type = value_str(provider, "/type").unwrap_or("");
    let service = value_str(provider, "/service").unwrap_or("");

    if args.provider == "gemini_cli_main" || service.contains("Gemini CLI") {
        return chat_with_gemini_cli(provider, args);
    }

    if provider_type == "google_generative_language_api" {
        return chat_with_google(provider, args);
    }

    if provider_type == "file_bridge_llm_api" {
        return chat_with_helper(provider, args);
    }

    if provider_type.starts_with("interactive_subscription") {
        let guidance = value_str(provider, "/routing_guidance")
            .or_else(|| value_str(provider, "/routing_notes"))
            .unwrap_or("interactive subscription providers are not portable API keys");
        bail!("{guidance}");
    }

    if provider_type.contains("openai_compatible")
        || provider.pointer("/chat_completions_endpoint").is_some()
    {
        return chat_with_openai_compatible(provider, args);
    }

    bail!(
        "provider {} is not supported by `aihub chat` yet",
        args.provider
    );
}

fn chat_with_openai_compatible(provider: &Value, args: ChatArgs) -> Result<()> {
    let base_url = value_str(provider, "/base_url").context("provider is missing base_url")?;
    let endpoint = value_str(provider, "/chat_completions_endpoint").unwrap_or("/chat/completions");
    let api_key = value_str(provider, "/api_key").context("provider is missing api_key")?;
    let model = preferred_model(provider, args.model.as_deref())
        .context("provider has no default model; pass --model")?;
    let url = join_url(base_url, endpoint);
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": args.prompt}],
    });
    let output = curl_post_json(
        &url,
        vec![
            format!("Authorization: Bearer {api_key}"),
            "Content-Type: application/json".to_string(),
        ],
        &body,
    )?;
    print_chat_output(&output, args.raw, ChatProtocol::OpenAi)
}

fn chat_with_google(provider: &Value, args: ChatArgs) -> Result<()> {
    let base_url = value_str(provider, "/base_url").context("provider is missing base_url")?;
    let api_key = value_str(provider, "/api_key").context("provider is missing api_key")?;
    let model = preferred_model(provider, args.model.as_deref())
        .context("provider has no default model; pass --model")?;
    let template = value_str(provider, "/generate_content_endpoint_template")
        .unwrap_or("/models/{model}:generateContent");
    let endpoint = template.replace("{model}", &model);
    let url = format!("{}{}?key={}", base_url.trim_end_matches('/'), endpoint, api_key);
    let body = json!({
        "contents": [{"parts": [{"text": args.prompt}]}],
    });
    let output = curl_post_json(&url, vec!["Content-Type: application/json".to_string()], &body)?;
    print_chat_output(&output, args.raw, ChatProtocol::Google)
}

fn chat_with_gemini_cli(provider: &Value, args: ChatArgs) -> Result<()> {
    let binary = value_str(provider, "/binary/wrapper_path")
        .or_else(|| value_str(provider, "/binary/real_binary"))
        .unwrap_or("gemini");
    let binary = expand_path(Path::new(binary));
    let model = preferred_model(provider, args.model.as_deref());

    let mut command = Command::new(binary);
    if let Some(model) = model {
        command.args(["-m", &model]);
    }
    command.args(["-p", &args.prompt]);
    let output = command.output().context("failed to run Gemini CLI")?;
    if !output.status.success() {
        bail!(
            "Gemini CLI failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

fn chat_with_helper(provider: &Value, args: ChatArgs) -> Result<()> {
    let helper = value_str(provider, "/helper").context("provider is missing helper")?;
    let helper = expand_path(Path::new(helper));
    let output = Command::new("python3")
        .arg(helper)
        .args(["ask", "--prompt", &args.prompt])
        .output()
        .context("failed to run provider helper")?;
    if !output.status.success() {
        bail!(
            "provider helper failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

fn ssh(registry: &Value, args: SshArgs) -> Result<()> {
    let host = entry(registry, "/catalog/hosts", &args.host_id)?;
    let ssh = host
        .pointer("/access/methods/ssh")
        .context("host does not define access.methods.ssh")?;
    let target = value_str(host, "/network/host")
        .or_else(|| value_str(host, "/network/hostname"))
        .context("host is missing network.host and network.hostname")?;
    let user = value_str(ssh, "/user").context("ssh method is missing user")?;
    let password = value_str(ssh, "/password").context("ssh method is missing password")?;
    let port = ssh.pointer("/port").and_then(Value::as_i64).unwrap_or(22);
    let mut remote_command = args.command;

    if remote_command.is_empty() && !args.interactive {
        remote_command.push("hostname".to_string());
    }

    let script = write_temp_file("aihub-ssh", EXPECT_SSH)?;
    let mut command = Command::new("expect");
    command
        .arg(&script)
        .arg("30")
        .arg(target)
        .arg(user)
        .arg(port.to_string())
        .env("AIHUB_SSH_PASSWORD", password)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for part in remote_command {
        command.arg(part);
    }

    let status = command.status().context("failed to run expect for ssh")?;
    let _ = fs::remove_file(script);
    if !status.success() {
        bail!("ssh command failed with status {status}");
    }
    Ok(())
}

const EXPECT_SSH: &str = r#"
set timeout [lindex $argv 0]
set host [lindex $argv 1]
set user [lindex $argv 2]
set port [lindex $argv 3]
set remote_cmd [lrange $argv 4 end]
set password $env(AIHUB_SSH_PASSWORD)

if {[llength $remote_cmd] > 0} {
  spawn ssh -p $port -o StrictHostKeyChecking=no -o PreferredAuthentications=password -o PubkeyAuthentication=no $user@$host {*}$remote_cmd
} else {
  spawn ssh -p $port -o StrictHostKeyChecking=no -o PreferredAuthentications=password -o PubkeyAuthentication=no $user@$host
}

expect {
  -re {(?i)yes/no} { send "yes\r"; exp_continue }
  -re {(?i)password:} { send "$password\r" }
  timeout { puts stderr "ssh timeout"; exit 124 }
}

if {[llength $remote_cmd] > 0} {
  expect eof
  lassign [wait] pid spawnid os_error exit_code
  exit $exit_code
} else {
  interact
  lassign [wait] pid spawnid os_error exit_code
  exit $exit_code
}
"#;

enum ChatProtocol {
    OpenAi,
    Google,
}

fn print_chat_output(output: &str, raw: bool, protocol: ChatProtocol) -> Result<()> {
    if raw {
        println!("{output}");
        return Ok(());
    }

    let value = serde_json::from_str::<Value>(output).context("provider returned non-JSON output")?;
    let text = match protocol {
        ChatProtocol::OpenAi => extract_openai_text(&value),
        ChatProtocol::Google => extract_google_text(&value),
    };

    if let Some(text) = text {
        println!("{text}");
        return Ok(());
    }

    print_json(&value)
}

fn extract_openai_text(value: &Value) -> Option<String> {
    value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/choices/0/text").and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

fn extract_google_text(value: &Value) -> Option<String> {
    let parts = value.pointer("/candidates/0/content/parts")?.as_array()?;
    let text = parts
        .iter()
        .filter_map(|part| part.pointer("/text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn curl_post_json(url: &str, headers: Vec<String>, body: &Value) -> Result<String> {
    let body_path = write_temp_file("aihub-request", &serde_json::to_string(body)?)?;
    let mut config = String::new();
    config.push_str("silent\n");
    config.push_str("show-error\n");
    config.push_str("fail-with-body\n");
    config.push_str("request = \"POST\"\n");
    config.push_str(&format!("url = \"{}\"\n", curl_config_quote(url)));
    for header in headers {
        config.push_str(&format!("header = \"{}\"\n", curl_config_quote(&header)));
    }
    config.push_str(&format!(
        "data-binary = \"@{}\"\n",
        curl_config_quote(&body_path.to_string_lossy())
    ));

    let mut child = Command::new("curl")
        .arg("--config")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run curl")?;
    {
        let mut stdin = child.stdin.take().context("failed to open curl stdin")?;
        stdin.write_all(config.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    let _ = fs::remove_file(body_path);

    if !output.status.success() {
        bail!(
            "curl failed: {}{}",
            String::from_utf8_lossy(&output.stderr).trim(),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn curl_config_quote(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn join_url(base_url: &str, endpoint: &str) -> String {
    if endpoint.starts_with('/') {
        format!("{}{}", base_url.trim_end_matches('/'), endpoint)
    } else {
        format!("{}/{}", base_url.trim_end_matches('/'), endpoint)
    }
}

fn preferred_model(provider: &Value, explicit: Option<&str>) -> Option<String> {
    if let Some(model) = explicit {
        return Some(model.to_string());
    }

    [
        "/default_model",
        "/target_model",
        "/default_model_in_settings",
        "/recommended_models/daily_driver",
        "/recommended_models/chatgpt_latest",
        "/recommended_models/gpt_latest",
        "/recommended_models_via_cli/daily_driver_verified_working",
    ]
    .iter()
    .find_map(|path| value_str(provider, path).map(ToOwned::to_owned))
}

fn host_summary(id: &str, host: &Value) -> Value {
    json!({
        "id": id,
        "display_name": value_str(host, "/display_name"),
        "host": value_str(host, "/network/host"),
        "hostname": value_str(host, "/network/hostname"),
        "os": value_str(host, "/platform/os"),
        "primary_method": value_str(host, "/access/primary_method"),
        "verification": value_str(host, "/verification/status"),
    })
}

fn provider_summary(id: &str, provider: &Value) -> Value {
    json!({
        "id": id,
        "display_name": value_str(provider, "/display_name"),
        "type": value_str(provider, "/type"),
        "service": value_str(provider, "/service"),
        "base_url": value_str(provider, "/base_url"),
        "default_model": preferred_model(provider, None),
        "last_test_status": value_str(provider, "/last_test_status"),
    })
}

fn section_object<'a>(registry: &'a Value, path: &str) -> Result<&'a serde_json::Map<String, Value>> {
    registry
        .pointer(path)
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("registry is missing object at {path}"))
}

fn sorted_entries<'a>(registry: &'a Value, section: &str) -> Result<Vec<(&'a String, &'a Value)>> {
    let mut entries = section_object(registry, section)?.iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(right.0));
    Ok(entries)
}

fn entry<'a>(registry: &'a Value, section: &str, id: &str) -> Result<&'a Value> {
    section_object(registry, section)?
        .get(id)
        .ok_or_else(|| anyhow!("unknown {} id: {id}", section.trim_start_matches("/catalog/")))
}

fn value_str<'a>(value: &'a Value, path: &str) -> Option<&'a str> {
    value.pointer(path).and_then(Value::as_str)
}

fn sanitize(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sanitize).collect()),
        Value::Object(object) => {
            let mut sanitized = serde_json::Map::new();
            for (key, item) in object {
                if is_secret_key(key) {
                    sanitized.insert(key.clone(), Value::String("<redacted>".to_string()));
                } else {
                    sanitized.insert(key.clone(), sanitize(item));
                }
            }
            Value::Object(sanitized)
        }
        _ => value.clone(),
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "api_key",
        "password",
        "token",
        "secret",
        "private_key",
        "access_key",
        "secret_key",
        "credential",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn write_temp_file(prefix: &str, contents: &str) -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
    fs::write(&path, contents)?;
    Ok(path)
}
