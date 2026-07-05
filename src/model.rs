//! The provider-neutral model seam.
//!
//! One `call_model()` seam, no provider lock-in. Anthropic-specific features
//! (prompt caching, budgets, effort) are one provider profile behind this
//! seam, not the design center. The default system prompt is bare: the
//! zero-default-context floor.
//!
//! The types here are provider-neutral by construction: a request is a system
//! prompt plus a message history plus the tool names exposed this turn; a
//! response is an assistant message plus any tool calls. Nothing in this shape
//! names a provider. Real backends implement [`Model`]; [`StubModel`] is the
//! offline default for the pump MVP.

// Wired into the pump in P1.6 / P1.7; until then these are defined ahead of use.
#![allow(dead_code)]

use std::fmt;
use std::future::Future;

use serde::{Deserialize, Serialize};

/// The zero-default-context floor: coxn adds nothing to the model's own default
/// system prompt. No task means a bare prompt; a named task is what adds
/// context, and only aden's scope manifest defines what (the bloat arbiter).
///
/// This is empty on purpose. The one sanctioned growth is the optional, opt-in
/// nudge line ("aden tools available via search"), which is not MVP. Any other
/// content here is a deliberate deviation from a stated law; flag it first.
pub const DEFAULT_SYSTEM_PROMPT: &str = "";

/// A role in a conversation turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A single message in the conversation. `tool_calls` and `tool_call_id` carry
/// the linkage a function-calling provider needs: an assistant message records
/// the calls it requested, and a tool message records which call it answers.
/// Both default empty, so a plain text turn is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Tool calls the assistant requested in this message (assistant role).
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// The id of the tool call this message answers (tool role).
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

impl Message {
    /// A plain message with no tool linkage.
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// An assistant message carrying the tool calls it requested.
    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }

    /// A tool-result message answering a specific tool call.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// A tool call the model wants the pump to run. `arguments` is the raw,
/// provider-neutral payload (opaque to the seam; the tool dispatch interprets
/// it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A tool definition exposed to the model this turn: name, description, and a
/// JSON Schema for its arguments. Built by the registry from the advertised
/// tools; a function-calling provider sends these so the model can call them.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A reasoning-effort level, sent to providers that support it (mapped to the
/// `reasoning_effort` field). `Off` omits the field, so providers that ignore
/// it are unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingLevel {
    Off,
    Low,
    Medium,
    High,
}

impl ThinkingLevel {
    /// Parse a `/think` argument, accepting common spellings.
    pub fn parse(s: &str) -> Option<ThinkingLevel> {
        match s.trim().to_lowercase().as_str() {
            "off" | "none" => Some(ThinkingLevel::Off),
            "low" => Some(ThinkingLevel::Low),
            "med" | "medium" => Some(ThinkingLevel::Medium),
            "high" => Some(ThinkingLevel::High),
            _ => None,
        }
    }

    /// The wire `reasoning_effort` value, or `None` for `Off` (omit the field).
    pub fn effort(self) -> Option<&'static str> {
        match self {
            ThinkingLevel::Off => None,
            ThinkingLevel::Low => Some("low"),
            ThinkingLevel::Medium => Some("medium"),
            ThinkingLevel::High => Some("high"),
        }
    }

    /// A short label for the status line / confirmations.
    pub fn label(self) -> &'static str {
        match self {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
        }
    }
}

/// A request to a model: the bare system prompt, the conversation so far, the
/// tool definitions exposed this turn, and the reasoning-effort level (when set).
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub thinking: Option<ThinkingLevel>,
}

/// Token usage a backend reports for a turn (provider-neutral). Drives the
/// context meter; `prompt_tokens` is the size of the context that was sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// A model's response: the assistant message, any tool calls to run, and the
/// token usage when the backend reports it (`None` otherwise).
#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub message: Message,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
}

/// An error from a model backend.
#[derive(Debug)]
pub enum ModelError {
    /// The backend failed (network, auth, decode, ...). Provider-neutral string.
    Backend(String),
}

impl ModelError {
    /// Whether the error looks transient and worth retrying (rate limit, server
    /// unavailable, a dropped or timed-out connection). A bad request (400, a bad
    /// model name, a model that will not load) is NOT transient -- retrying it
    /// just fails the same way. Heuristic over the provider-neutral message.
    pub fn is_transient(&self) -> bool {
        let ModelError::Backend(msg) = self;
        let m = msg.to_lowercase();
        const TRANSIENT: [&str; 7] = [
            "429",
            "503",
            "502",
            "connection reset",
            "connection refused",
            "timed out",
            "timeout",
        ];
        // "failed to load" (a model that will not load) is a 400-class error here,
        // not a transient one; do not retry it.
        if m.contains("failed to load") || m.contains("400") {
            return false;
        }
        TRANSIENT.iter().any(|s| m.contains(s))
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelError::Backend(msg) => write!(f, "model backend error: {msg}"),
        }
    }
}

impl std::error::Error for ModelError {}

/// The provider abstraction: the single seam coxn calls. A real provider
/// (Anthropic, OpenAI, local, ...) is one implementation; the pump never names
/// one. The returned future is `Send` so the seam composes with any runtime.
pub trait Model {
    /// Whether this backend accepts coxn tool definitions on each request.
    /// CLI piggyback backends that only do text turns return `false`.
    fn supports_tool_calling(&self) -> bool {
        true
    }

    fn call(
        &self,
        request: ModelRequest,
    ) -> impl Future<Output = Result<ModelResponse, ModelError>> + Send;

    /// Stream a turn, invoking `on_delta` with each assistant text fragment as it
    /// arrives, and returning the assembled response (full text + any tool calls)
    /// at the end. `on_delta` returns `false` to request cancellation; the
    /// backend stops reading and returns what it has so far. The default is
    /// non-streaming: call [`Model::call`] and emit the whole content once, so a
    /// backend opts into streaming by overriding this. Not `Send` (the sink is a
    /// borrowed `&mut dyn FnMut`); coxn's loop is single-threaded, which is also
    /// why the blocking read can drive the sink directly.
    async fn stream(
        &self,
        request: ModelRequest,
        io: &mut dyn crate::pump::TurnIo,
    ) -> Result<ModelResponse, ModelError> {
        let response = self.call(request).await?;
        if !response.message.content.is_empty() && io.on_idle() {
            io.on_delta(&response.message.content);
        }
        Ok(response)
    }
}

/// The named seam the pump drives. Thin wrapper over [`Model::call`] so the
/// pump reads in the DESIGN's vocabulary (`call_model`) and stays oblivious to
/// which backend is behind it.
pub async fn call_model<M: Model>(
    model: &M,
    request: ModelRequest,
) -> Result<ModelResponse, ModelError> {
    model.call(request).await
}

/// The offline default backend for the pump MVP: deterministic, no provider,
/// no network. Echoes the last user message back as the assistant turn. Real
/// providers implement [`Model`] behind the same seam, with no lock-in.
pub struct StubModel;

impl Model for StubModel {
    async fn call(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.as_str())
            .unwrap_or("");
        Ok(ModelResponse {
            message: Message::new(Role::Assistant, format!("stub: {last_user}")),
            tool_calls: Vec::new(),
            usage: None,
        })
    }
}

/// The runtime-selected model backend. The pump is monomorphized over this enum,
/// so the actual model is chosen at runtime (per agent / per role) rather than
/// baked into the type. That is what lets sub-agents and workflows mix models
/// without reworking the seam. The OpenAI-compatible HTTP backend lands here as
/// a new variant in P3.2; for now the backends are the offline stub and the
/// OpenAI-compatible HTTP backend (LM Studio / Ollama / OpenRouter / ...).
pub enum AnyModel {
    Stub(StubModel),
    OpenAiCompat(crate::openai::OpenAiCompatModel),
    Ollama(crate::ollama::OllamaModel),
    CodexPiggyback(crate::codex_model::CodexPiggybackModel),
    ClaudeCliPiggyback(crate::claude_cli::ClaudeCliPiggybackModel),
    GrokCliPiggyback(crate::grok_cli::GrokCliPiggybackModel),
}

impl Model for AnyModel {
    fn supports_tool_calling(&self) -> bool {
        match self {
            AnyModel::Stub(model) => model.supports_tool_calling(),
            AnyModel::OpenAiCompat(model) => model.supports_tool_calling(),
            AnyModel::Ollama(model) => model.supports_tool_calling(),
            AnyModel::CodexPiggyback(model) => model.supports_tool_calling(),
            AnyModel::ClaudeCliPiggyback(model) => model.supports_tool_calling(),
            AnyModel::GrokCliPiggyback(model) => model.supports_tool_calling(),
        }
    }

    async fn call(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        match self {
            AnyModel::Stub(model) => model.call(request).await,
            AnyModel::OpenAiCompat(model) => model.call(request).await,
            AnyModel::Ollama(model) => model.call(request).await,
            AnyModel::CodexPiggyback(model) => model.call(request).await,
            AnyModel::ClaudeCliPiggyback(model) => model.call(request).await,
            AnyModel::GrokCliPiggyback(model) => model.call(request).await,
        }
    }

    // Delegate streaming to the active variant so the OpenAI-compatible backend's
    // SSE streaming is used (the default would buffer the whole reply).
    async fn stream(
        &self,
        request: ModelRequest,
        io: &mut dyn crate::pump::TurnIo,
    ) -> Result<ModelResponse, ModelError> {
        match self {
            AnyModel::Stub(model) => model.stream(request, io).await,
            AnyModel::OpenAiCompat(model) => model.stream(request, io).await,
            AnyModel::Ollama(model) => model.stream(request, io).await,
            AnyModel::CodexPiggyback(model) => model.stream(request, io).await,
            AnyModel::ClaudeCliPiggyback(model) => model.stream(request, io).await,
            AnyModel::GrokCliPiggyback(model) => model.stream(request, io).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with(user: &str) -> ModelRequest {
        ModelRequest {
            system: String::new(),
            messages: vec![Message::new(Role::User, user)],
            tools: Vec::new(),
            thinking: None,
        }
    }

    /// Guards the zero-default-context floor: the default system prompt adds
    /// nothing. If this fails, coxn grew context by default; that is a law
    /// deviation and must be intentional.
    #[test]
    fn default_system_prompt_is_the_bare_floor() {
        assert!(
            DEFAULT_SYSTEM_PROMPT.is_empty(),
            "zero-default-context floor: coxn must add nothing by default"
        );
    }

    #[tokio::test]
    async fn stub_echoes_last_user_message() {
        let resp = call_model(&StubModel, request_with("hello"))
            .await
            .expect("stub never errors");
        assert_eq!(resp.message.role, Role::Assistant);
        assert_eq!(resp.message.content, "stub: hello");
        assert!(resp.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn stub_uses_the_latest_user_turn() {
        let req = ModelRequest {
            system: String::new(),
            messages: vec![
                Message::new(Role::User, "first"),
                Message::new(Role::Assistant, "stub: first"),
                Message::new(Role::User, "second"),
            ],
            tools: Vec::new(),
            thinking: None,
        };
        let resp = call_model(&StubModel, req)
            .await
            .expect("stub never errors");
        assert_eq!(resp.message.content, "stub: second");
    }

    #[test]
    fn thinking_level_parses_and_maps_to_effort() {
        assert_eq!(ThinkingLevel::parse("HIGH"), Some(ThinkingLevel::High));
        assert_eq!(ThinkingLevel::parse("med"), Some(ThinkingLevel::Medium));
        assert_eq!(ThinkingLevel::parse("off"), Some(ThinkingLevel::Off));
        assert_eq!(ThinkingLevel::parse("nonsense"), None);
        // Off omits the wire field; the rest map to a level string.
        assert_eq!(ThinkingLevel::Off.effort(), None);
        assert_eq!(ThinkingLevel::High.effort(), Some("high"));
    }

    #[test]
    fn transient_errors_are_classified_for_retry() {
        let t = |s: &str| ModelError::Backend(s.to_string()).is_transient();
        assert!(t(
            "http://x/chat/completions returned 503 Service Unavailable"
        ));
        assert!(t("returned 429 Too Many Requests"));
        assert!(t("request to x failed: connection reset by peer"));
        assert!(t("reading response failed: timed out"));
        // Not transient: bad requests and unloadable models.
        assert!(!t("returned 400 Bad Request: invalid model"));
        assert!(!t(r#"returned 400: { "error": "Failed to load model" }"#));
        assert!(!t("model returned an empty message"));
    }

    #[tokio::test]
    async fn any_model_dispatches_to_its_backend() {
        let model = AnyModel::Stub(StubModel);
        let resp = call_model(&model, request_with("hi"))
            .await
            .expect("stub never errors");
        assert_eq!(resp.message.content, "stub: hi");
    }
}
