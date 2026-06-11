use std::fs;
use std::process::Command;

fn aihub_bin() -> String {
    std::env::var("CARGO_BIN_EXE_aihub").expect("Cargo should expose the aihub test binary path")
}

fn fixture_registry() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "wx-cli-aihub-test-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    fs::write(
        &path,
        r#"{
  "version": 2,
  "schema_name": "aihub_catalog_v2",
  "updated_at": "2026-04-25T10:20:07+08:00",
  "registry_identity": {
    "preferred_display_name": "Test AIHub",
    "preferred_spoken_name": "Test AIHub",
    "preferred_slug": "test-aihub",
    "purpose": "test fixture"
  },
  "catalog": {
    "hosts": {
      "m4-mac": {
        "display_name": "M4 Mac mini",
        "kind": "host",
        "network": {"host": "192.168.1.12", "hostname": "m4.local", "ports": {"ssh": 22}},
        "platform": {"os": "macOS"},
        "roles": ["general_compute"],
        "access": {
          "primary_method": "ssh",
          "methods": {
            "ssh": {
              "enabled": true,
              "protocol": "ssh",
              "port": 22,
              "user": "kuen",
              "password": "fixture-host-password",
              "auth": "password"
            }
          }
        },
        "verification": {"status": "verified", "capabilities": ["ssh_password_auth"]},
        "notes": "fixture host"
      }
    },
    "providers": {
      "ai302_main": {
        "display_name": "302.AI main",
        "type": "openai_compatible_multi_provider",
        "base_url": "https://api.302.ai/v1",
        "api_key": "fixture-provider-secret",
        "auth_header": "Authorization: Bearer <api_key>",
        "chat_completions_endpoint": "/chat/completions",
        "default_model": "gpt-5.5",
        "last_test_status": "ok",
        "notes": "fixture provider"
      }
    }
  }
}
"#,
    )
    .unwrap();
    (dir, path)
}

#[test]
fn path_uses_registry_env_var() {
    let (_dir, registry) = fixture_registry();

    let output = Command::new(aihub_bin())
        .arg("path")
        .env("LAN_AI_REGISTRY_PATH", &registry)
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), registry.to_string_lossy());
}

#[test]
fn provider_output_redacts_secrets_by_default() {
    let (_dir, registry) = fixture_registry();

    let output = Command::new(aihub_bin())
        .args(["--registry", registry.to_str().unwrap(), "provider", "ai302_main"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("302.AI main"));
    assert!(stdout.contains("<redacted>"));
    assert!(!stdout.contains("fixture-provider-secret"));
}

#[test]
fn overview_summarizes_registry_counts() {
    let (_dir, registry) = fixture_registry();

    let output = Command::new(aihub_bin())
        .args(["--registry", registry.to_str().unwrap(), "overview"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("Test AIHub"));
    assert!(stdout.contains("hosts: 1"));
    assert!(stdout.contains("providers: 1"));
}
