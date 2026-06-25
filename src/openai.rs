//! OpenAI-compatible chat-completions backend.
//!
//! One backend covers LM Studio, Ollama, OpenRouter (-> Claude / GPT / Gemini /
//! Llama / ...), vLLM, and OpenAI: they all speak
//! `POST {base_url}/chat/completions`. The provider is selected by data (a
//! `{base_url, model, key}` spec), not a type. See DESIGN.adoc Phase 3.
//!
//! Text turns only for now. Tool calling over the wire needs two things coxn's
//! minimal types do not carry yet (a `tool_call_id` thread on `Message`, and
//! JSON schemas on `Tool`), and a half-correct tool path that breaks on the
//! second hop is worse than clean chat-first. With this backend the model
//! converses but does not yet call tools; the offline stub still exercises the
//! full tool and gate flow.

use serde::{Deserialize, Serialize};

use crate::model::{Message, Model, ModelError, ModelRequest, ModelResponse, Role};

/// An OpenAI-compatible chat backend bound to one model spec.
pub struct OpenAiCompatModel {
    base_url: String,
    model: String,
    api_key: Option<String>,
}

impl OpenAiCompatModel {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key,
        }
    }
}

/// The OpenAI chat role string for a coxn role.
fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage>,
    stream: bool,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
}

/// Build the chat-completions request body from coxn's request. The bare system
/// prompt (when non-empty) leads as a `system` message; the rest map by role.
fn to_wire<'a>(model: &'a str, request: &ModelRequest) -> ChatRequest<'a> {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if !request.system.is_empty() {
        messages.push(WireMessage {
            role: "system",
            content: request.system.clone(),
        });
    }
    for m in &request.messages {
        messages.push(WireMessage {
            role: role_str(m.role),
            content: m.content.clone(),
        });
    }
    ChatRequest {
        model,
        messages,
        stream: false,
    }
}

/// Extract the assistant text from a chat-completions response.
fn from_wire(response: ChatResponse) -> Result<ModelResponse, ModelError> {
    let content = response
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .ok_or_else(|| ModelError::Backend("model returned no message content".to_string()))?;
    Ok(ModelResponse {
        message: Message::new(Role::Assistant, content),
        tool_calls: Vec::new(),
    })
}

impl Model for OpenAiCompatModel {
    async fn call(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        // ureq is blocking; the pump already blocks on a turn, so this is fine
        // for the single-threaded loop. (Revisit with an async client if the TUI
        // needs to stay live during a call.)
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = to_wire(&self.model, &request);

        let mut builder = ureq::post(&url);
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", &format!("Bearer {key}"));
        }
        let mut response = builder
            .send_json(&body)
            .map_err(|e| ModelError::Backend(format!("request to {url} failed: {e}")))?;
        let parsed: ChatResponse = response
            .body_mut()
            .read_json()
            .map_err(|e| ModelError::Backend(format!("decoding response failed: {e}")))?;
        from_wire(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(system: &str, turns: &[(Role, &str)]) -> ModelRequest {
        ModelRequest {
            system: system.to_string(),
            messages: turns
                .iter()
                .map(|(role, text)| Message::new(*role, *text))
                .collect(),
            tools: Vec::new(),
        }
    }

    #[test]
    fn to_wire_leads_with_system_then_maps_roles() {
        let req = request("be terse", &[(Role::User, "hi"), (Role::Assistant, "yo")]);
        let wire = to_wire("local", &req);
        assert_eq!(wire.model, "local");
        assert!(!wire.stream);
        let roles: Vec<&str> = wire.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec!["system", "user", "assistant"]);
        assert_eq!(wire.messages[0].content, "be terse");
    }

    #[test]
    fn to_wire_omits_empty_system() {
        let req = request("", &[(Role::User, "hi")]);
        let wire = to_wire("local", &req);
        assert_eq!(
            wire.messages.iter().map(|m| m.role).collect::<Vec<_>>(),
            vec!["user"]
        );
    }

    #[test]
    fn request_serializes_to_openai_shape() {
        let req = request("sys", &[(Role::User, "q")]);
        let json = serde_json::to_value(to_wire("m", &req)).unwrap();
        assert_eq!(json["model"], "m");
        assert_eq!(json["stream"], false);
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][1]["role"], "user");
        assert_eq!(json["messages"][1]["content"], "q");
    }

    #[test]
    fn from_wire_pulls_the_first_choice_text() {
        let json = r#"{"choices":[{"message":{"role":"assistant","content":"hello there"}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        let out = from_wire(resp).unwrap();
        assert_eq!(out.message.role, Role::Assistant);
        assert_eq!(out.message.content, "hello there");
    }

    #[test]
    fn from_wire_errors_on_empty_choices() {
        let resp: ChatResponse = serde_json::from_str(r#"{"choices":[]}"#).unwrap();
        assert!(matches!(from_wire(resp), Err(ModelError::Backend(_))));
    }
}
