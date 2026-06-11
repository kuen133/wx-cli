/// 输出格式
pub enum Fmt {
    Yaml,
    Json,
}

#[derive(Clone, Copy, Debug)]
pub struct OutputOpts {
    pub json: bool,
    pub with_meta: bool,
    pub debug_source: bool,
}

impl OutputOpts {
    pub fn request_flags(self) -> (bool, bool) {
        (self.with_meta || self.debug_source, self.debug_source)
    }
}

/// 默认 YAML，--json 时输出 JSON
pub fn resolve(json: bool) -> Fmt {
    if json {
        Fmt::Json
    } else {
        Fmt::Yaml
    }
}

pub fn print_value(value: &serde_json::Value, fmt: &Fmt) -> anyhow::Result<()> {
    match fmt {
        Fmt::Json => println!("{}", serde_json::to_string_pretty(value)?),
        Fmt::Yaml => print!("{}", serde_yaml::to_string(value)?),
    }
    Ok(())
}

pub fn print_response(data: &serde_json::Value, opts: &OutputOpts) -> anyhow::Result<()> {
    print_value(data, &resolve(opts.json))
}

pub fn emit_warnings(data: &serde_json::Value) {
    for line in warning_lines(data) {
        eprintln!("[wx] 警告：{}", line);
    }
}

pub fn warning_lines(data: &serde_json::Value) -> Vec<String> {
    let mut lines = Vec::new();
    let meta = match data.get("meta") {
        Some(v) if v.is_object() => v,
        _ => return lines,
    };

    let unknown_shards: Vec<String> = meta
        .get("unknown_shards")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    if !unknown_shards.is_empty() {
        lines.push(format!(
            "磁盘上发现 daemon 不认识的分片 {}，结果可能不完整；运行 `wx init --force` 重新提取密钥。",
            unknown_shards.join(", ")
        ));
    }

    lines
}
