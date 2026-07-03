//! Codex CLI piggyback backend (`codex app-server` text-only turns).
//!
//! MVP scope: text generation through Codex's native app-server protocol. coxn
//! tool definitions are not forwarded; the pump keeps gate/approval on coxn tools.

use std::path::Path;

use crate::codex_app_server::{CodexAppServerConfig, CodexAppServerSession};
use crate::model::{
    Message, Model, ModelError, ModelRequest, ModelResponse, Role, ThinkingLevel,
};

pub const CODEX_ENDPOINT_SCHEME: &str = "codex-cli://";

pub fn codex_binary_from_endpoint(base_url: &str) -> Option<&str> {
    base_url.strip_prefix(CODEX_ENDPOINT_SCHEME)
}

/// List models advertised by a Codex instance's `model/list` RPC.
pub fn list_models(
    binary: &str,
    codex_home: Option<&str>,
    env: &[(String, String)],
) -> Option<Vec<String>> {
    let config = CodexAppServerConfig::for_probe(
        binary.to_string(),
        codex_home.map(str::to_string),
        env.to_vec(),
    );
    let mut session = CodexAppServerSession::spawn(&config).ok()?;
    session.list_models().ok()
}

/// Piggyback model that drives Codex through `app-server` JSONL.
pub struct CodexPiggybackModel {
    binary: String,
    model: String,
    codex_home: Option<String>,
    env: Vec<(String, String)>,
    cwd: String,
}

impl CodexPiggybackModel {
    pub fn new(
        binary: impl Into<String>,
        model: impl Into<String>,
        codex_home: Option<String>,
        env: Vec<(String, String)>,
        cwd: impl AsRef<Path>,
    ) -> Self {
        Self {
            binary: binary.into(),
            model: model.into(),
            codex_home,
            env,
            cwd: cwd.as_ref().display().to_string(),
        }
    }

    fn run_turn(
        &self,
        request: &ModelRequest,
        on_delta: Option<&mut dyn FnMut(&str) -> bool>,
    ) -> Result<ModelResponse, ModelError> {
        let prompt = flatten_request(request);
        let config = CodexAppServerConfig::for_turn(
            self.binary.clone(),
            self.codex_home.clone(),
            self.env.clone(),
            &self.cwd,
        );
        let mut session = CodexAppServerSession::spawn(&config)
            .map_err(|e| ModelError::Backend(format!("codex app-server spawn failed: {e}")))?;
        let completion = session
            .complete_text_turn(&self.model, &self.cwd, &prompt, on_delta)
            .map_err(|e| ModelError::Backend(format!("codex turn failed: {e}")))?;
        Ok(ModelResponse {
            message: Message::new(Role::Assistant, completion.text),
            tool_calls: Vec::new(),
            usage: completion.usage,
        })
    }
}

impl Model for CodexPiggybackModel {
    fn supports_tool_calling(&self) -> bool {
        false
    }

    async fn call(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.run_turn(&request, None)
    }

    async fn stream(
        &self,
        request: ModelRequest,
        on_delta: &mut dyn FnMut(&str) -> bool,
    ) -> Result<ModelResponse, ModelError> {
        self.run_turn(&request, Some(on_delta))
    }
}

fn flatten_request(request: &ModelRequest) -> String {
    let mut parts = Vec::new();
    if !request.system.is_empty() {
        parts.push(format!("System:\n{}", request.system.trim()));
    }
    if let Some(level) = request.thinking
        && level != ThinkingLevel::Off
    {
        parts.push(format!("Reasoning effort: {}", level.label()));
    }
    for message in &request.messages {
        let role = match message.role {
            Role::System => "System",
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
        };
        let mut body = message.content.trim().to_string();
        if message.role == Role::Tool {
            if let Some(id) = &message.tool_call_id {
                body = format!("(tool result for {id})\n{body}");
            }
        }
        if !message.tool_calls.is_empty() {
            let calls = message
                .tool_calls
                .iter()
                .map(|tc| format!("{}({})", tc.name, tc.arguments))
                .collect::<Vec<_>>()
                .join(", ");
            if body.is_empty() {
                body = format!("(requested tools: {calls})");
            } else {
                body.push_str(&format!("\n(requested tools: {calls})"));
            }
        }
        parts.push(format!("{role}:\n{body}"));
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_app_server::test_support::{FakeCodexMode, write_fake_codex};

    #[test]
    fn flatten_request_includes_roles_and_system() {
        let request = ModelRequest {
            system: "be concise".to_string(),
            messages: vec![
                Message::new(Role::User, "hello"),
                Message::assistant("hi", vec![]),
            ],
            tools: vec![],
            thinking: None,
        };
        let prompt = flatten_request(&request);
        assert!(prompt.contains("System:\nbe concise"));
        assert!(prompt.contains("User:\nhello"));
        assert!(prompt.contains("Assistant:\nhi"));
    }

    #[tokio::test]
    async fn piggyback_turn_from_fake_binary() {
        let dir = std::env::temp_dir().join(format!("coxn-codex-model-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = write_fake_codex(&dir, FakeCodexMode::TextTurn);
        let model =
            CodexPiggybackModel::new(fake.to_string_lossy(), "test-model", None, vec![], &dir);
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

    #[test]
    fn does_not_support_tool_calling() {
        let model = CodexPiggybackModel::new("codex", "test-model", None, vec![], ".");
        assert!(!model.supports_tool_calling());
    }
}
