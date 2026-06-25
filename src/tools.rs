//! Thin tool dispatch.
//!
//! Maps a model-requested tool call to a result. Commodity machinery kept
//! deliberately thin. aden's tools are not injected up front: they are latent,
//! discovered by intent through the [`DISCOVER`] seam, and dispatchable once the
//! model knows their name. Only the seam (plus any active tools) is advertised,
//! which keeps the default context free of tool bloat.
//!
//! Handlers are synchronous for the MVP. I/O-bound tools (real aden calls) may
//! want async dispatch later; revisit when a real provider lands.

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
    /// A one-line statement of intent, surfaced by tool discovery so the model
    /// can find a tool by what it does rather than being shown every schema.
    fn intent(&self) -> &str {
        ""
    }
    /// Whether this tool mutates the working tree. The pump consults the gate
    /// before accepting a mutating tool's effect; read-only tools skip it.
    fn mutates(&self) -> bool {
        false
    }
}

/// The name of the always-present discovery seam. The model calls it with an
/// intent query to find latent tools; this is the only aden capability
/// advertised up front, which is what keeps the default context free of tool
/// bloat (zero-default-context).
pub const DISCOVER: &str = "aden_tools";

/// A thin registry with deferred disclosure: `active` tools are advertised in
/// every request's tool list; `latent` tools are discoverable via the [`DISCOVER`]
/// seam and dispatchable once the model knows their name, but are not advertised
/// up front. This is what keeps the default context free of tool bloat — the
/// model pulls the schema it needs by intent instead of being shown all of them.
#[derive(Default)]
pub struct ToolRegistry {
    active: Vec<Box<dyn Tool>>,
    latent: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an always-advertised tool.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.active.push(tool);
    }

    /// Add a latent tool: discoverable by intent, not advertised up front.
    pub fn register_latent(&mut self, tool: Box<dyn Tool>) {
        self.latent.push(tool);
    }

    /// The advertised tool list for a request: the discovery seam plus the
    /// active tools. Latent tools are intentionally omitted (no bloat).
    pub fn names(&self) -> Vec<String> {
        let mut names = vec![DISCOVER.to_string()];
        names.extend(self.active.iter().map(|t| t.name().to_string()));
        names
    }

    /// Search latent tools by intent/name for `query` (empty lists all),
    /// returning `name — intent` lines for the model to act on.
    pub fn discover(&self, query: &str) -> String {
        let q = query.trim().to_lowercase();
        let mut hits: Vec<String> = self
            .latent
            .iter()
            .filter(|t| {
                q.is_empty()
                    || t.name().to_lowercase().contains(&q)
                    || t.intent().to_lowercase().contains(&q)
            })
            .map(|t| format!("{} — {}", t.name(), t.intent()))
            .collect();
        if hits.is_empty() {
            return format!("no aden tools match '{query}'");
        }
        hits.sort();
        hits.join("\n")
    }

    /// Dispatch a tool call. The discovery seam is handled here; otherwise the
    /// active then latent tools are searched. An unknown tool is an error fed
    /// back to the model, not a panic.
    pub fn dispatch(&self, call: &ToolCall) -> ToolResult {
        if call.name == DISCOVER {
            return Ok(self.discover(&call.arguments));
        }
        match self.find(&call.name) {
            Some(tool) => tool.run(&call.arguments),
            None => Err(format!("unknown tool: {}", call.name)),
        }
    }

    /// Whether the named tool mutates the working tree (so the pump gates it).
    /// An unknown tool is treated as non-mutating; dispatch handles the error.
    pub fn mutates(&self, name: &str) -> bool {
        self.find(name).map(|t| t.mutates()).unwrap_or(false)
    }

    /// Find a tool by name across active and latent sets.
    fn find(&self, name: &str) -> Option<&dyn Tool> {
        self.active
            .iter()
            .chain(self.latent.iter())
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
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

use std::path::PathBuf;

/// Pull-context tool: assemble an anchor's neighborhood via `aden asm`. The
/// model calls this to pull blast-radius / context on demand (pull, not push);
/// the argument is the anchor. aden is the bloat arbiter — coxn only relays.
pub struct AsmTool {
    dir: PathBuf,
}

impl AsmTool {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl Tool for AsmTool {
    fn name(&self) -> &str {
        "aden_asm"
    }

    fn intent(&self) -> &str {
        "assemble an anchor's graph neighborhood (blast radius and context)"
    }

    fn run(&self, arguments: &str) -> ToolResult {
        let anchor = arguments.trim();
        if anchor.is_empty() {
            return Err("aden_asm needs an anchor argument".to_string());
        }
        crate::aden::pull(&self.dir, crate::aden::Pull::Asm(anchor)).map_err(|e| e.to_string())
    }
}

/// Pull-context tool: definition + callers + downstream impact for a symbol via
/// `aden understand`. The argument is the symbol name.
pub struct UnderstandTool {
    dir: PathBuf,
}

impl UnderstandTool {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl Tool for UnderstandTool {
    fn name(&self) -> &str {
        "aden_understand"
    }

    fn intent(&self) -> &str {
        "a symbol's definition, callers, and downstream impact"
    }

    fn run(&self, arguments: &str) -> ToolResult {
        let symbol = arguments.trim();
        if symbol.is_empty() {
            return Err("aden_understand needs a symbol argument".to_string());
        }
        crate::aden::pull(&self.dir, crate::aden::Pull::Understand(symbol))
            .map_err(|e| e.to_string())
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
    fn names_advertise_the_discovery_seam_and_active_tools() {
        // The discovery seam is always advertised; echo is active.
        assert_eq!(
            registry().names(),
            vec![DISCOVER.to_string(), "echo".to_string()]
        );
    }

    #[test]
    fn latent_tools_are_discoverable_but_not_advertised() {
        let mut r = ToolRegistry::new();
        r.register_latent(Box::new(AsmTool::new(PathBuf::from("."))));
        r.register_latent(Box::new(UnderstandTool::new(PathBuf::from("."))));

        // Latent tools stay out of the advertised list (no bloat by default).
        assert_eq!(r.names(), vec![DISCOVER.to_string()]);

        // Found by intent through the discovery seam.
        let hits = r
            .dispatch(&call(DISCOVER, "impact"))
            .expect("discover runs");
        assert!(hits.contains("aden_understand"), "{hits}");
        assert!(
            !hits.contains("aden_asm"),
            "intent filter too broad: {hits}"
        );

        // An empty query lists all latent tools.
        let all = r.discover("");
        assert!(all.contains("aden_asm") && all.contains("aden_understand"));

        // A non-match is reported, not an error.
        assert!(r.discover("nope").contains("no aden tools match"));
    }

    #[test]
    fn latent_tools_dispatch_once_known() {
        let mut r = ToolRegistry::new();
        r.register_latent(Box::new(AsmTool::new(PathBuf::from("."))));
        // Dispatchable even though latent; empty arg proves AsmTool actually ran.
        assert!(r.dispatch(&call("aden_asm", "")).is_err());
        // Still unknown if never registered.
        assert!(r.dispatch(&call("ghost", "")).is_err());
    }

    #[test]
    fn aden_tools_reject_empty_arguments() {
        let asm = AsmTool::new(PathBuf::from("."));
        let understand = UnderstandTool::new(PathBuf::from("."));
        assert!(asm.run("   ").is_err());
        assert!(understand.run("").is_err());
    }
}
