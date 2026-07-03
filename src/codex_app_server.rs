//! JSONL client for `codex app-server` (auth probes and text-only piggyback turns).

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::model::Usage;

pub const CLIENT_NAME: &str = "coxn";
pub const CLIENT_VERSION: &str = "0.3.2.0";

const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone, Debug)]
pub struct CodexAppServerConfig {
    pub binary: String,
    pub codex_home: Option<String>,
    pub env: Vec<(String, String)>,
    #[allow(dead_code)]
    pub cwd: String,
    pub rpc_timeout: Duration,
}

impl CodexAppServerConfig {
    pub fn for_probe(
        binary: impl Into<String>,
        codex_home: Option<String>,
        env: Vec<(String, String)>,
    ) -> Self {
        Self {
            binary: binary.into(),
            codex_home,
            env,
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| ".".to_string()),
            rpc_timeout: DEFAULT_PROBE_TIMEOUT,
        }
    }

    pub fn for_turn(
        binary: impl Into<String>,
        codex_home: Option<String>,
        env: Vec<(String, String)>,
        cwd: impl AsRef<Path>,
    ) -> Self {
        Self {
            binary: binary.into(),
            codex_home,
            env,
            cwd: cwd.as_ref().display().to_string(),
            rpc_timeout: DEFAULT_TURN_TIMEOUT,
        }
    }
}

pub struct CodexAppServerSession {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<String>,
    next_id: u64,
    initialized: bool,
    timeout: Duration,
}

impl CodexAppServerSession {
    pub fn spawn(config: &CodexAppServerConfig) -> Result<Self, String> {
        let mut cmd = Command::new(&config.binary);
        cmd.arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(home) = &config.codex_home {
            cmd.env("CODEX_HOME", home);
        }
        for (key, value) in &config.env {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "stdout unavailable".to_string())?;
        let stdin = child
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

        Ok(Self {
            child,
            stdin,
            rx,
            next_id: 1,
            initialized: false,
            timeout: config.rpc_timeout,
        })
    }

    pub fn initialize(&mut self) -> Result<(), String> {
        if self.initialized {
            return Ok(());
        }
        let id = self.next_id();
        let params = serde_json::json!({
            "clientInfo": {
                "name": CLIENT_NAME,
                "title": "coxn",
                "version": CLIENT_VERSION,
            }
        });
        self.call("initialize", id, params)?;
        self.notify("initialized", serde_json::json!({}))?;
        self.initialized = true;
        Ok(())
    }

    pub fn account_read(&mut self) -> Result<CodexAccountWire, String> {
        self.initialize()?;
        let id = self.next_id();
        let line = self.call("account/read", id, serde_json::json!({}))?;
        parse_account_response(&line)
    }

    pub fn list_models(&mut self) -> Result<Vec<String>, String> {
        self.initialize()?;
        let id = self.next_id();
        let line = self.call("model/list", id, serde_json::json!({}))?;
        parse_model_list(&line)
    }

    pub fn complete_text_turn(
        &mut self,
        model: &str,
        cwd: &str,
        prompt: &str,
        on_delta: Option<&mut dyn FnMut(&str) -> bool>,
    ) -> Result<TurnCompletion, String> {
        self.initialize()?;
        let thread_id = self.start_ephemeral_thread(model, cwd)?;
        self.run_turn(&thread_id, prompt, on_delta)
    }

    fn start_ephemeral_thread(&mut self, model: &str, cwd: &str) -> Result<String, String> {
        let id = self.next_id();
        let params = serde_json::json!({
            "cwd": cwd,
            "model": model,
            "ephemeral": true,
        });
        let line = self.call("thread/start", id, params)?;
        let value: serde_json::Value =
            serde_json::from_str(&line).map_err(|e| format!("invalid thread/start json: {e}"))?;
        value
            .pointer("/result/thread/id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| "thread/start missing thread id".to_string())
    }

    fn run_turn(
        &mut self,
        thread_id: &str,
        prompt: &str,
        mut on_delta: Option<&mut dyn FnMut(&str) -> bool>,
    ) -> Result<TurnCompletion, String> {
        let id = self.next_id();
        let params = serde_json::json!({
            "threadId": thread_id,
            "input": [{"type": "text", "text": prompt}],
        });
        let _ = self.call("turn/start", id, params)?;

        let deadline = Instant::now() + self.timeout;
        let mut text = String::new();
        let mut usage = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.child.kill();
                return Err("turn timeout".to_string());
            }
            match self
                .rx
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(line) => {
                    let value: serde_json::Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if let Some(method) = value.get("method").and_then(|v| v.as_str()) {
                        match method {
                            "item/agentMessage/delta" => {
                                if let Some(delta) =
                                    value.pointer("/params/delta").and_then(|v| v.as_str())
                                {
                                    text.push_str(delta);
                                    if let Some(cb) = on_delta.as_deref_mut()
                                        && !cb(delta)
                                    {
                                        let _ = self.child.kill();
                                        break;
                                    }
                                }
                            }
                            "item/completed" => {
                                if let Some(item_text) =
                                    value.pointer("/params/item/text").and_then(|v| v.as_str())
                                    && text.is_empty()
                                {
                                    text = item_text.to_string();
                                }
                            }
                            "thread/tokenUsage/updated" => {
                                usage = parse_token_usage(&value);
                            }
                            "turn/completed" => {
                                let status = value
                                    .pointer("/params/turn/status")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("completed");
                                if status == "failed" || status == "interrupted" {
                                    let err = value
                                        .pointer("/params/turn/error/message")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(status);
                                    return Err(format!("turn {status}: {err}"));
                                }
                                break;
                            }
                            _ => {}
                        }
                    }
                    if let Some(err) = rpc_error_message(&value) {
                        return Err(err);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self
                        .child
                        .try_wait()
                        .ok()
                        .flatten()
                        .is_some_and(|status| !status.success())
                    {
                        return Err("codex exited during turn".to_string());
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if text.is_empty() {
                        return Err("codex stdout closed before turn completed".to_string());
                    }
                    break;
                }
            }
        }

        Ok(TurnCompletion { text, usage })
    }

    fn call(&mut self, method: &str, id: u64, params: serde_json::Value) -> Result<String, String> {
        let payload = serde_json::json!({
            "method": method,
            "id": id,
            "params": params,
        });
        self.write_value(&payload)?;
        self.wait_for_id(id)
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<(), String> {
        let payload = serde_json::json!({
            "method": method,
            "params": params,
        });
        self.write_value(&payload)
    }

    fn write_value(&mut self, value: &serde_json::Value) -> Result<(), String> {
        let json = serde_json::to_string(value).map_err(|e| format!("encode failed: {e}"))?;
        self.stdin
            .write_all(json.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("write failed: {e}"))
    }

    fn wait_for_id(&mut self, id: u64) -> Result<String, String> {
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.child.kill();
                return Err("rpc timeout".to_string());
            }
            match self
                .rx
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(line) => {
                    if let Some(body) = line_for_id(&line, id) {
                        if let Some(message) = body.strip_prefix("error:") {
                            return Err(message.to_string());
                        }
                        return Ok(body);
                    }
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
                        && let Some(err) = rpc_error_message(&value)
                    {
                        return Err(err);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self
                        .child
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

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

impl Drop for CodexAppServerSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAccountWire {
    pub account_type: String,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub requires_openai_auth: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnCompletion {
    pub text: String,
    pub usage: Option<Usage>,
}

pub fn binary_installed(bin: &str) -> bool {
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

fn rpc_error_message(value: &serde_json::Value) -> Option<String> {
    value
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn parse_account_response(line: &str) -> Result<CodexAccountWire, String> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("invalid json: {e}"))?;
    let account = value
        .pointer("/result/account")
        .ok_or_else(|| "missing account".to_string())?;
    Ok(CodexAccountWire {
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

fn parse_model_list(line: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("invalid json: {e}"))?;
    let models = value
        .pointer("/result/models")
        .or_else(|| value.pointer("/result/data"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| "model/list missing models".to_string())?;
    Ok(models
        .iter()
        .filter_map(|entry| {
            entry
                .get("id")
                .or_else(|| entry.get("model"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect())
}

fn parse_token_usage(value: &serde_json::Value) -> Option<Usage> {
    let usage = value.pointer("/params/tokenUsage/last")?;
    let input = usage.get("inputTokens")?.as_u64()? as u32;
    let output = usage.get("outputTokens")?.as_u64()? as u32;
    let total = usage
        .get("totalTokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(input.saturating_add(output));
    Some(Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: total,
    })
}

#[cfg(test)]
pub mod test_support {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    pub fn write_fake_codex(dir: &Path, mode: FakeCodexMode) -> PathBuf {
        let path = dir.join("fake-codex");
        let script = match mode {
            FakeCodexMode::AuthOnly => AUTH_ONLY_SCRIPT,
            FakeCodexMode::TextTurn => TEXT_TURN_SCRIPT,
        };
        fs::write(&path, script).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    pub enum FakeCodexMode {
        AuthOnly,
        TextTurn,
    }

    const AUTH_ONLY_SCRIPT: &str = r#"#!/usr/bin/env bash
set -euo pipefail
while IFS= read -r line; do
  method=$(printf '%s' "$line" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("method",""))')
  req_id=$(printf '%s' "$line" | python3 -c 'import json,sys; v=json.load(sys.stdin); print(v.get("id",""))')
  case "$method" in
    initialize)
      printf '%s\n' "{\"id\":$req_id,\"result\":{\"userAgent\":\"fake\",\"codexHome\":\"/tmp/fake-codex-home\",\"platformFamily\":\"unix\",\"platformOs\":\"linux\"}}"
      ;;
    account/read)
      printf '%s\n' "{\"id\":$req_id,\"result\":{\"account\":{\"type\":\"chatgpt\",\"email\":\"user@example.com\",\"planType\":\"plus\"},\"requiresOpenaiAuth\":false}}"
      ;;
  esac
done
"#;

    const TEXT_TURN_SCRIPT: &str = r#"#!/usr/bin/env bash
set -euo pipefail
while IFS= read -r line; do
  method=$(printf '%s' "$line" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("method",""))')
  req_id=$(printf '%s' "$line" | python3 -c 'import json,sys; v=json.load(sys.stdin); print(v.get("id",""))')
  case "$method" in
    initialize)
      printf '%s\n' "{\"id\":$req_id,\"result\":{\"userAgent\":\"fake\",\"codexHome\":\"/tmp/fake-codex-home\",\"platformFamily\":\"unix\",\"platformOs\":\"linux\"}}"
      ;;
    thread/start)
      printf '%s\n' "{\"id\":$req_id,\"result\":{\"thread\":{\"id\":\"thread-test-1\"},\"model\":\"test-model\"}}"
      ;;
    turn/start)
      printf '%s\n' "{\"id\":$req_id,\"result\":{\"turn\":{\"id\":\"turn-test-1\",\"status\":\"inProgress\"}}}"
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"delta":"PONG"}}'
      printf '%s\n' '{"method":"turn/completed","params":{"turn":{"id":"turn-test-1","status":"completed"}}}'
      ;;
    model/list)
      printf '%s\n' "{\"id\":$req_id,\"result\":{\"models\":[{\"id\":\"test-model\"},{\"id\":\"other-model\"}]}}"
      ;;
  esac
done
"#;
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::{FakeCodexMode, write_fake_codex};

    #[test]
    fn account_read_from_fake_binary() {
        let dir = std::env::temp_dir().join(format!("coxn-codex-app-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = write_fake_codex(&dir, FakeCodexMode::AuthOnly);
        let config =
            CodexAppServerConfig::for_probe(fake.to_string_lossy().to_string(), None, vec![]);
        let mut session = CodexAppServerSession::spawn(&config).expect("spawn");
        let account = session.account_read().expect("account read");
        assert_eq!(account.email.as_deref(), Some("user@example.com"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn complete_text_turn_from_fake_binary() {
        let dir = std::env::temp_dir().join(format!("coxn-codex-turn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = write_fake_codex(&dir, FakeCodexMode::TextTurn);
        let config =
            CodexAppServerConfig::for_turn(fake.to_string_lossy().to_string(), None, vec![], &dir);
        let mut session = CodexAppServerSession::spawn(&config).expect("spawn");
        let result = session
            .complete_text_turn("test-model", &dir.display().to_string(), "ping", None)
            .expect("turn");
        assert_eq!(result.text, "PONG");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
