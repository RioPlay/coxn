//! Grok Build CLI piggyback (`grok -p` + `streaming-json`).

use std::path::Path;
use std::process::Command;

use crate::cli_ndjson::{
    NdjsonTurnResult, StreamControl, apply_instance_env, flatten_request, run_ndjson_turn,
    usage_from_object,
};
use crate::model::{Message, Model, ModelError, ModelRequest, ModelResponse, Role};
use crate::pump::TurnIo;

pub const GROK_CLI_SCHEME: &str = "grok-cli://";

pub fn binary_from_endpoint(base_url: &str) -> Option<&str> {
    base_url.strip_prefix(GROK_CLI_SCHEME)
}

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
    let mut models = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        else {
            continue;
        };
        let name = rest
            .trim_start_matches('*')
            .split_whitespace()
            .next()?
            .trim_end_matches("(default)");
        models.push(name.to_string());
    }
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

pub struct GrokCliPiggybackModel {
    pub binary: String,
    pub model: String,
    pub home_path: Option<String>,
    pub env: Vec<(String, String)>,
    pub cwd: String,
}

impl GrokCliPiggybackModel {
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
            .arg("-p")
            .arg(&prompt)
            .arg("--output-format")
            .arg("streaming-json")
            .arg("--always-approve")
            .arg("--no-alt-screen")
            .arg("--no-subagents")
            .arg("-m")
            .arg(&self.model);
        apply_instance_env(&mut cmd, self.home_path.as_deref(), &self.env);
        run_ndjson_turn(cmd, io, |v, r, io| on_grok_line(v, r, io)).map_err(ModelError::Backend)
    }
}

impl Model for GrokCliPiggybackModel {
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

fn on_grok_line(
    value: &serde_json::Value,
    result: &mut NdjsonTurnResult,
    io: &mut dyn TurnIo,
) -> Result<StreamControl, String> {
    match value.get("type").and_then(|v| v.as_str()) {
        Some("text") => {
            if let Some(delta) = value.get("data").and_then(|v| v.as_str()) {
                result.text.push_str(delta);
                if !io.on_delta(delta) {
                    return Ok(StreamControl::Done);
                }
            }
        }
        Some("end") => {
            if let Some(usage) = value.get("usage").and_then(usage_from_object) {
                result.usage = Some(usage);
            }
            return Ok(StreamControl::Done);
        }
        Some("usage") => {
            if let Some(usage) = value
                .get("usage")
                .and_then(usage_from_object)
                .or_else(|| value.pointer("/data/usage").and_then(usage_from_object))
            {
                result.usage = Some(usage);
            }
        }
        Some("error") => {
            let msg = value
                .get("message")
                .or_else(|| value.get("data"))
                .and_then(|v| v.as_str())
                .unwrap_or("grok turn failed");
            return Err(msg.to_string());
        }
        // Skip reasoning/thought tokens — coxn reasoning hide applies at render time.
        Some("thought") => {}
        _ => {}
    }
    Ok(StreamControl::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_ndjson::test_support::{
        fake_cli_test_lock, unique_temp_dir, write_executable_script,
    };

    #[tokio::test]
    async fn stream_turn_parses_usage_from_end_event() {
        let _guard = fake_cli_test_lock();
        let dir = unique_temp_dir("coxn-grok-usage");
        let body = r#"echo '{"type":"text","data":"ok"}'
echo '{"type":"end","usage":{"input_tokens":42,"output_tokens":7}}'"#;
        let fake = write_executable_script(&dir, "fake-grok", body);
        let model =
            GrokCliPiggybackModel::new(fake.to_string_lossy(), "test-model", None, vec![], &dir);
        let response = model
            .call(ModelRequest {
                system: String::new(),
                messages: vec![Message::new(Role::User, "ping")],
                tools: vec![],
                thinking: None,
            })
            .await
            .expect("call");
        let usage = response.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 42);
        assert_eq!(usage.completion_tokens, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stream_turn_from_fake_binary() {
        let _guard = fake_cli_test_lock();
        let dir = unique_temp_dir("coxn-grok-cli");
        let body = r#"echo '{"type":"text","data":"P"}'
echo '{"type":"text","data":"ONG"}'
echo '{"type":"end","stopReason":"EndTurn"}'"#;
        let fake = write_executable_script(&dir, "fake-grok", body);
        let model =
            GrokCliPiggybackModel::new(fake.to_string_lossy(), "test-model", None, vec![], &dir);
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
