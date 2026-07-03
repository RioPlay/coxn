//! Claude Code CLI piggyback (`claude -p` + `stream-json`).

use std::path::Path;
use std::process::Command;

use crate::cli_ndjson::{
    NdjsonTurnResult, StreamControl, apply_instance_env, flatten_request, run_ndjson_turn,
    usage_from_object,
};
use crate::model::{Message, Model, ModelError, ModelRequest, ModelResponse, Role};
use crate::pump::TurnIo;

pub const CLAUDE_CLI_SCHEME: &str = "claude-cli://";

pub fn binary_from_endpoint(base_url: &str) -> Option<&str> {
    base_url.strip_prefix(CLAUDE_CLI_SCHEME)
}

/// Best-effort model list; falls back to the configured model name.
pub fn list_models(
    binary: &str,
    home_path: Option<&str>,
    env: &[(String, String)],
) -> Option<Vec<String>> {
    let mut cmd = Command::new(binary);
    cmd.arg("models")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    apply_instance_env(&mut cmd, home_path, env);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let models: Vec<String> = text
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('[') {
                return None;
            }
            Some(trimmed.to_string())
        })
        .collect();
    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

pub fn probe_logged_in(binary: &str, home_path: Option<&str>, env: &[(String, String)]) -> bool {
    let mut cmd = Command::new(binary);
    cmd.arg("models")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    apply_instance_env(&mut cmd, home_path, env);
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

pub struct ClaudeCliPiggybackModel {
    pub binary: String,
    pub model: String,
    pub home_path: Option<String>,
    pub env: Vec<(String, String)>,
    pub cwd: String,
}

impl ClaudeCliPiggybackModel {
    pub fn new(
        binary: impl Into<String>,
        model: impl Into<String>,
        home_path: Option<String>,
        env: Vec<(String, String)>,
        cwd: impl AsRef<Path>,
    ) -> Self {
        Self {
            binary: binary.into(),
            model: model.into(),
            home_path,
            env,
            cwd: cwd.as_ref().display().to_string(),
        }
    }

    fn run_turn(
        &self,
        request: &ModelRequest,
        io: &mut dyn TurnIo,
    ) -> Result<NdjsonTurnResult, ModelError> {
        let prompt = flatten_request(request);
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&self.cwd)
            .arg("--bare")
            .arg("-p")
            .arg(&prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--include-partial-messages")
            .arg("--model")
            .arg(&self.model)
            .arg("--permission-mode")
            .arg("dontAsk")
            .arg("--disallowed-tools")
            .arg("Bash,Edit,Read");
        apply_instance_env(&mut cmd, self.home_path.as_deref(), &self.env);
        run_ndjson_turn(cmd, io, |v, r, io| on_claude_line(v, r, io)).map_err(ModelError::Backend)
    }
}

impl Model for ClaudeCliPiggybackModel {
    fn supports_tool_calling(&self) -> bool {
        false
    }

    async fn call(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.run_turn(&request, &mut crate::pump::SilentIo)
            .map(to_response)
    }

    async fn stream(
        &self,
        request: ModelRequest,
        io: &mut dyn TurnIo,
    ) -> Result<ModelResponse, ModelError> {
        self.run_turn(&request, io).map(to_response)
    }
}

fn to_response(result: NdjsonTurnResult) -> ModelResponse {
    ModelResponse {
        message: Message::new(Role::Assistant, result.text),
        tool_calls: Vec::new(),
        usage: result.usage,
    }
}

fn on_claude_line(
    value: &serde_json::Value,
    result: &mut NdjsonTurnResult,
    io: &mut dyn TurnIo,
) -> Result<StreamControl, String> {
    let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "stream_event"
            if value.pointer("/event/delta/type").and_then(|v| v.as_str())
                == Some("text_delta") =>
        {
            if let Some(delta) = value.pointer("/event/delta/text").and_then(|v| v.as_str()) {
                result.text.push_str(delta);
                if !io.on_delta(delta) {
                    return Ok(StreamControl::Done);
                }
            }
        }
        "assistant" => {
            if let Some(err) = value.get("error").and_then(|v| v.as_str()) {
                let detail = assistant_text(value).unwrap_or_default();
                return Err(format!("claude: {err}: {detail}"));
            }
            if let Some(usage) = value.pointer("/message/usage").and_then(usage_from_object) {
                result.usage = Some(usage);
            }
            if let Some(text) = assistant_text(value) {
                if result.text.is_empty() {
                    result.text = text;
                }
            }
        }
        "result" => {
            if value.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                let msg = value
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("turn failed");
                return Err(msg.to_string());
            }
            if let Some(usage) = value.get("usage").and_then(usage_from_object) {
                result.usage = Some(usage);
            }
            if result.text.is_empty() {
                if let Some(text) = value.get("result").and_then(|v| v.as_str()) {
                    result.text = text.to_string();
                }
            }
            return Ok(StreamControl::Done);
        }
        _ => {}
    }
    Ok(StreamControl::Continue)
}

fn assistant_text(value: &serde_json::Value) -> Option<String> {
    value
        .pointer("/message/content")
        .and_then(|v| v.as_array())?
        .iter()
        .filter_map(|block| {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                block
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            } else {
                None
            }
        })
        .reduce(|mut acc, part| {
            acc.push_str(&part);
            acc
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_ndjson::test_support::{
        fake_cli_test_lock, unique_temp_dir, write_executable_script,
    };

    #[tokio::test]
    async fn stream_turn_from_fake_binary() {
        let _guard = fake_cli_test_lock();
        let dir = unique_temp_dir("coxn-claude-cli");
        let body = r#"echo '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"P"}}}'
echo '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"ONG"}}}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"PONG","usage":{"input_tokens":10,"output_tokens":2}}'"#;
        let fake = write_executable_script(&dir, "fake-claude", body);
        let model =
            ClaudeCliPiggybackModel::new(fake.to_string_lossy(), "test-model", None, vec![], &dir);
        let response = model
            .call(ModelRequest {
                system: String::new(),
                messages: vec![Message::new(Role::User, "ping")],
                tools: vec![],
                thinking: None,
            })
            .await
            .expect("call");
        assert_eq!(response.message.content, "PONG");
        let usage = response.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
