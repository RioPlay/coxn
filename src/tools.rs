//! Thin tool dispatch.
//!
//! Maps a model-requested tool call to a result. Commodity machinery kept
//! deliberately thin. aden's tools are not injected up front; they are
//! discovered by intent through a deferred-loading seam (Phase 2). The one
//! built-in here is a placeholder that proves the dispatch path.
//!
//! Handlers are synchronous for the MVP. I/O-bound tools (real aden calls) may
//! want async dispatch later; revisit in Phase 2 rather than speculatively now.

use crate::model::ToolCall;

/// The outcome of running a tool: the text fed back to the model as a Tool
/// message, or an error string. Provider-neutral.
pub type ToolResult = Result<String, String>;

/// A tool the pump can dispatch: a stable name and a handler over the raw,
/// opaque arguments carried by a [`ToolCall`].
pub trait Tool {
    /// The name the model calls this tool by.
    fn name(&self) -> &str;
    /// Run the tool against its raw arguments payload.
    fn run(&self, arguments: &str) -> ToolResult;
}

/// A thin registry: a set of tools dispatched by name. No schemas, no
/// discovery logic here; that is the deferred-loading seam's job (Phase 2).
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool. Later registrations of the same name do not replace earlier
    /// ones; dispatch resolves the first match.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// The names of the registered tools, for populating a request's tool list.
    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    /// Dispatch a tool call to its handler. An unknown tool is an error fed
    /// back to the model, not a panic.
    pub fn dispatch(&self, call: &ToolCall) -> ToolResult {
        match self.tools.iter().find(|t| t.name() == call.name) {
            Some(tool) => tool.run(&call.arguments),
            None => Err(format!("unknown tool: {}", call.name)),
        }
    }
}

/// A trivial built-in tool: returns its arguments verbatim. A placeholder that
/// proves the dispatch path; the real tools are aden's, discovered on demand.
pub struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn run(&self, arguments: &str) -> ToolResult {
        Ok(arguments.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "t1".to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
        }
    }

    fn registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(Box::new(EchoTool));
        r
    }

    #[test]
    fn dispatches_to_the_named_tool() {
        let r = registry();
        assert_eq!(r.dispatch(&call("echo", "hi")), Ok("hi".to_string()));
    }

    #[test]
    fn unknown_tool_is_an_error_not_a_panic() {
        let r = registry();
        assert_eq!(
            r.dispatch(&call("nope", "")),
            Err("unknown tool: nope".to_string())
        );
    }

    #[test]
    fn names_lists_registered_tools() {
        assert_eq!(registry().names(), vec!["echo".to_string()]);
    }
}
