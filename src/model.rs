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

/// A role in a conversation turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A single message in the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    /// Convenience constructor.
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

/// A tool call the model wants the pump to run. `arguments` is the raw,
/// provider-neutral payload (opaque to the seam; the tool dispatch interprets
/// it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A request to a model: the bare system prompt, the conversation so far, and
/// the names of the tools exposed this turn. Tool *schemas* are not carried
/// here; deferred discovery (Phase 2) decides which load.
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<String>,
}

/// A model's response: the assistant message plus any tool calls to run.
#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub message: Message,
    pub tool_calls: Vec<ToolCall>,
}

/// An error from a model backend.
#[derive(Debug)]
pub enum ModelError {
    /// The backend failed (network, auth, decode, ...). Provider-neutral string.
    Backend(String),
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
    fn call(
        &self,
        request: ModelRequest,
    ) -> impl Future<Output = Result<ModelResponse, ModelError>> + Send;
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
        })
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
        }
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
        };
        let resp = call_model(&StubModel, req)
            .await
            .expect("stub never errors");
        assert_eq!(resp.message.content, "stub: second");
    }
}
