//! Short-lived `codex app-server` JSONL probe for auth status (no chat API).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::provider::ProviderInstance;

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAccount {
    pub account_type: String,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub requires_openai_auth: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexProbeOutcome {
    Authenticated(CodexAccount),
    NotLoggedIn,
    NotInstalled,
    ProbeFailed(String),
}

/// Probe Codex account state via `initialize` → `initialized` → `account/read`.
pub fn probe_instance(instance: &ProviderInstance) -> CodexProbeOutcome {
    let bin = instance.binary.as_deref().unwrap_or("codex");
    if !binary_responds(bin) {
        return CodexProbeOutcome::NotInstalled;
    }
    match probe_account(bin, codex_home(instance), &instance.env) {
        Ok(account) => {
            if account.email.is_some() && !account.requires_openai_auth {
                CodexProbeOutcome::Authenticated(account)
            } else if account.email.is_some() {
                // Codex may report requiresOpenaiAuth even when an account is present.
                CodexProbeOutcome::Authenticated(account)
            } else {
                CodexProbeOutcome::NotLoggedIn
            }
        }
        Err(reason) => CodexProbeOutcome::ProbeFailed(reason),
    }
}

pub fn format_status_line(
    instance_id: &str,
    bin: &str,
    outcome: &CodexProbeOutcome,
) -> (bool, String) {
    match outcome {
        CodexProbeOutcome::Authenticated(account) => {
            let email = account.email.as_deref().unwrap_or("(unknown)");
            let plan = account
                .plan_type
                .as_deref()
                .map(|p| format!(", {p}"))
                .unwrap_or_default();
            (
                false,
                format!(
                    "✓ {instance_id}: {bin} authenticated ({account_type}, {email}{plan})",
                    account_type = account.account_type,
                    email = email,
                    plan = plan,
                ),
            )
        }
        CodexProbeOutcome::NotLoggedIn => (
            true,
            format!("✗ {instance_id}: {bin} installed but not logged in (`{bin} login`)"),
        ),
        CodexProbeOutcome::NotInstalled => (
            true,
            format!("✗ {instance_id}: {bin} not installed or not runnable"),
        ),
        CodexProbeOutcome::ProbeFailed(reason) => (
            true,
            format!("✗ {instance_id}: codex app-server probe failed ({reason})"),
        ),
    }
}

fn codex_home(instance: &ProviderInstance) -> Option<&str> {
    instance
        .shadow_home
        .as_deref()
        .or(instance.home_path.as_deref())
}

fn probe_account(
    bin: &str,
    codex_home: Option<&str>,
    extra_env: &[(String, String)],
) -> Result<CodexAccount, String> {
    let mut cmd = Command::new(bin);
    cmd.arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(home) = codex_home {
        cmd.env("CODEX_HOME", home);
    }
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "stdout unavailable".to_string())?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "stdin unavailable".to_string())?;

    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) if !line.trim().is_empty() => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    write_line(
        &mut stdin,
        r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"coxn_probe","title":"coxn","version":"0.3.2.0"}}}"#,
    )?;
    wait_for_id(&rx, 1, &mut child)?;
    write_line(&mut stdin, r#"{"method":"initialized","params":{}}"#)?;
    write_line(
        &mut stdin,
        r#"{"method":"account/read","id":2,"params":{}}"#,
    )?;
    let body = wait_for_id(&rx, 2, &mut child)?;
    let _ = child.kill();
    parse_account_response(&body)
}

fn write_line(stdin: &mut impl Write, json: &str) -> Result<(), String> {
    stdin
        .write_all(json.as_bytes())
        .and_then(|_| stdin.write_all(b"\n"))
        .and_then(|_| stdin.flush())
        .map_err(|e| format!("write failed: {e}"))
}

fn wait_for_id(rx: &mpsc::Receiver<String>, id: u64, child: &mut Child) -> Result<String, String> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let _ = child.kill();
            return Err("timeout".to_string());
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(line) => {
                if let Some(body) = line_for_id(&line, id) {
                    return Ok(body);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if child
                    .try_wait()
                    .ok()
                    .flatten()
                    .is_some_and(|status| !status.success())
                {
                    return Err("codex exited early".to_string());
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("codex stdout closed".to_string());
            }
        }
    }
}

fn line_for_id(line: &str, id: u64) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("id")?.as_u64()? != id {
        return None;
    }
    if value.get("error").is_some() {
        let message = value
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .unwrap_or("rpc error");
        return Some(format!("error:{message}"));
    }
    Some(line.to_string())
}

fn parse_account_response(line: &str) -> Result<CodexAccount, String> {
    if let Some(message) = line.strip_prefix("error:") {
        return Err(message.to_string());
    }
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("invalid json: {e}"))?;
    let account = value
        .pointer("/result/account")
        .ok_or_else(|| "missing account".to_string())?;
    Ok(CodexAccount {
        account_type: account
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        email: account
            .get("email")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        plan_type: account
            .get("planType")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        requires_openai_auth: value
            .pointer("/result/requiresOpenaiAuth")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    })
}

pub fn binary_installed(bin: &str) -> bool {
    binary_responds(bin)
}

fn binary_responds(bin: &str) -> bool {
    let Ok(mut child) = Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if start.elapsed() < Duration::from_secs(3) => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                return false;
            }
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn write_fake_codex(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("fake-codex");
        let script = r#"#!/usr/bin/env bash
set -euo pipefail
while IFS= read -r line; do
  method=$(printf '%s' "$line" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("method",""))')
  req_id=$(printf '%s' "$line" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')
  case "$method" in
    initialize)
      printf '%s\n' '{"id":1,"result":{"userAgent":"fake","codexHome":"/tmp/fake-codex-home","platformFamily":"unix","platformOs":"linux"}}'
      ;;
    account/read)
      printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt","email":"user@example.com","planType":"plus"},"requiresOpenaiAuth":false}}'
      ;;
  esac
done
"#;
        fs::write(&path, script).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn probe_parses_account_read_from_fake_binary() {
        let dir = std::env::temp_dir().join(format!("coxn-codex-probe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let fake = write_fake_codex(&dir);
        let account =
            probe_account(fake.to_str().unwrap(), None, &[]).expect("probe should succeed");
        assert_eq!(account.account_type, "chatgpt");
        assert_eq!(account.email.as_deref(), Some("user@example.com"));
        assert_eq!(account.plan_type.as_deref(), Some("plus"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_status_line_reports_authenticated_account() {
        let outcome = CodexProbeOutcome::Authenticated(CodexAccount {
            account_type: "chatgpt".to_string(),
            email: Some("user@example.com".to_string()),
            plan_type: Some("plus".to_string()),
            requires_openai_auth: false,
        });
        let (blocking, line) = format_status_line("codex-main", "codex", &outcome);
        assert!(!blocking);
        assert!(line.contains("authenticated"));
        assert!(line.contains("user@example.com"));
    }
}
