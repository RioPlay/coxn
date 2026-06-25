//! OpenAI-compatible chat-completions backend.
//!
//! One backend covers LM Studio, Ollama, OpenRouter (-> Claude / GPT / Gemini /
//! Llama / ...), vLLM, and OpenAI: they all speak
//! `POST {base_url}/chat/completions`. The provider is selected by data (a
//! `{base_url, model, key}` spec), not a type. See DESIGN.adoc Phase 3.
//!
//! Text and tool turns, non-streaming. The advertised tool defs are sent as
//! OpenAI `function` tools; assistant `tool_calls` and `tool` results thread
//! through the conversation, and tool calls are parsed back out of the response.
//! Streaming (and Ollama's native `/api/chat`, whose OpenAI-compat layer drops
//! tool calls under streaming) is a later profile.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::model::{Message, Model, ModelError, ModelRequest, ModelResponse, Role, ToolCall};

/// Well-known local providers, probed in order for zero-config startup.
const LOCAL_CANDIDATES: [&str; 2] = ["http://localhost:11434/v1", "http://localhost:1234/v1"];

/// Probe the well-known local providers (Ollama, LM Studio) and return the
/// `(base_url, model)` of the first that responds with at least one model.
/// `None` when nothing is running. This is the local-first, zero-config path.
pub fn detect() -> Option<(String, String)> {
    LOCAL_CANDIDATES
        .iter()
        .find_map(|base| first_model(base).map(|m| (base.to_string(), m)))
}

/// The first model id advertised by an OpenAI-compatible `/models` endpoint, or
/// `None` if the endpoint is unreachable or empty. A short timeout keeps a
/// stalled server from blocking startup; a refused connection fails instantly.
fn first_model(base_url: &str) -> Option<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(800)))
        .build()
        .into();
    let mut response = agent.get(format!("{base_url}/models")).call().ok()?;
    let models: ModelsResponse = response.body_mut().read_json().ok()?;
    models.data.into_iter().next().map(|m| m.id)
}

#[derive(Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    stream: bool,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

/// A tool/function call, in OpenAI's `{id, type:"function", function:{name,
/// arguments}}` shape. `arguments` is a JSON string. Used both directions.
#[derive(Serialize, Deserialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: WireFunction,
}

#[derive(Serialize, Deserialize)]
struct WireFunction {
    name: String,
    arguments: String,
}

/// A tool definition offered to the model (request side).
#[derive(Serialize)]
struct WireTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireToolFunction,
}

#[derive(Serialize)]
struct WireToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
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
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

/// Map a coxn tool call to the wire shape.
fn wire_call(tc: &ToolCall) -> WireToolCall {
    WireToolCall {
        id: tc.id.clone(),
        kind: "function".to_string(),
        function: WireFunction {
            name: tc.name.clone(),
            arguments: tc.arguments.clone(),
        },
    }
}

/// Build the chat-completions request body from coxn's request. The bare system
/// prompt (when non-empty) leads as a `system` message; the rest map by role,
/// carrying any tool calls (assistant) and answered id (tool). Advertised tool
/// defs become OpenAI `function` tools.
fn to_wire<'a>(model: &'a str, request: &ModelRequest) -> ChatRequest<'a> {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if !request.system.is_empty() {
        messages.push(WireMessage {
            role: "system",
            content: Some(request.system.clone()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }
    for m in &request.messages {
        // A tool-call-only assistant turn has no text; send null content there.
        let content = if m.content.is_empty() && !m.tool_calls.is_empty() {
            None
        } else {
            Some(m.content.clone())
        };
        messages.push(WireMessage {
            role: role_str(m.role),
            content,
            tool_calls: m.tool_calls.iter().map(wire_call).collect(),
            tool_call_id: m.tool_call_id.clone(),
        });
    }
    let tools = request
        .tools
        .iter()
        .map(|d| WireTool {
            kind: "function",
            function: WireToolFunction {
                name: d.name.clone(),
                description: d.description.clone(),
                parameters: d.parameters.clone(),
            },
        })
        .collect();
    ChatRequest {
        model,
        messages,
        tools,
        stream: false,
    }
}

/// Extract the assistant text and any tool calls from a chat-completions
/// response. A tool-call-only response (null content) is valid.
fn from_wire(response: ChatResponse) -> Result<ModelResponse, ModelError> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ModelError::Backend("model returned no choices".to_string()))?;
    let content = choice.message.content.unwrap_or_default();
    let tool_calls: Vec<ToolCall> = choice
        .message
        .tool_calls
        .into_iter()
        .map(|tc| ToolCall {
            id: tc.id,
            name: tc.function.name,
            arguments: tc.function.arguments,
        })
        .collect();
    if content.is_empty() && tool_calls.is_empty() {
        return Err(ModelError::Backend(
            "model returned an empty message".to_string(),
        ));
    }
    Ok(ModelResponse {
        message: Message::new(Role::Assistant, content),
        tool_calls,
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
    use crate::model::ToolDef;

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
        assert_eq!(wire.messages[0].content.as_deref(), Some("be terse"));
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

    #[test]
    fn request_carries_tools_and_tool_call_linkage() {
        let mut req = request("", &[(Role::User, "go")]);
        req.messages.push(Message::assistant(
            "",
            vec![ToolCall {
                id: "c1".into(),
                name: "aden_asm".into(),
                arguments: r#"{"anchor":"foo"}"#.into(),
            }],
        ));
        req.messages.push(Message::tool_result("c1", "result text"));
        req.tools = vec![ToolDef {
            name: "aden_asm".into(),
            description: "assemble".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let json = serde_json::to_value(to_wire("m", &req)).unwrap();
        // Tool defs become OpenAI function tools.
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "aden_asm");
        // The assistant turn carries its tool call (null content omitted).
        let assistant = &json["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant.get("content").is_none());
        assert_eq!(assistant["tool_calls"][0]["id"], "c1");
        assert_eq!(assistant["tool_calls"][0]["type"], "function");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "aden_asm");
        // The tool result carries the answered id and role.
        let tool = &json["messages"][2];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "c1");
        assert_eq!(tool["content"], "result text");
    }

    #[test]
    fn models_list_parses_first_id() {
        let json = r#"{"object":"list","data":[{"id":"llama3.1","object":"model"},{"id":"qwen"}]}"#;
        let models: ModelsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(
            models.data.into_iter().next().map(|m| m.id),
            Some("llama3.1".to_string())
        );
    }

    #[test]
    fn from_wire_parses_tool_calls() {
        let json = r#"{"choices":[{"message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"c1","type":"function",
            "function":{"name":"aden_asm","arguments":"{\"anchor\":\"foo\"}"}}]}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        let out = from_wire(resp).unwrap();
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].id, "c1");
        assert_eq!(out.tool_calls[0].name, "aden_asm");
        assert_eq!(out.tool_calls[0].arguments, r#"{"anchor":"foo"}"#);
    }
}
