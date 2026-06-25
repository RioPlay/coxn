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

use std::path::{Path, PathBuf};

use crate::model::{ToolCall, ToolDef};

/// Read a string argument from a tool-call payload. If `arguments` is a JSON
/// object (what a function-calling provider sends), return its `key` field;
/// otherwise treat the whole trimmed string as the value (bare-string args from
/// the stub or tests). This lets one tool serve both call styles.
fn arg(arguments: &str, key: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(arguments) {
        // A JSON object: return the named field, or empty if absent.
        Ok(v) if v.is_object() => v
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        // Not a JSON object (a bare string from the stub/tests): use it whole.
        _ => arguments.trim().to_string(),
    }
}

/// A JSON Schema for a tool taking a single required string parameter.
fn one_string_param(name: &str, description: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { name: { "type": "string", "description": description } },
        "required": [name],
    })
}

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
    /// JSON Schema for the tool's arguments, sent to a function-calling provider.
    /// Defaults to an empty object (no parameters).
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    /// Whether this tool mutates the working tree. The pump consults the gate
    /// before accepting a mutating tool's effect; read-only tools skip it.
    fn mutates(&self) -> bool {
        false
    }
    /// The file this call will write, for a mutating tool, so the pump can
    /// snapshot it before applying and restore it if the gate blocks the edit.
    /// `None` for tools that touch no single file (the default).
    fn target_path(&self, _arguments: &str) -> Option<PathBuf> {
        None
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

    /// Add an always-advertised tool (the discovery seam is always present on
    /// top of these). The action tools (edit / write_file) register here when a
    /// task scope is active.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.active.push(tool);
    }

    /// Add a latent tool: discoverable by intent, not advertised up front.
    pub fn register_latent(&mut self, tool: Box<dyn Tool>) {
        self.latent.push(tool);
    }

    /// The advertised tool definitions for a request: the discovery seam plus
    /// the active tools, each with name, description, and argument schema. Latent
    /// tools are omitted (the model finds them via the discovery seam).
    pub fn advertised_defs(&self) -> Vec<ToolDef> {
        let mut defs = vec![ToolDef {
            name: DISCOVER.to_string(),
            description: "Search aden's tool catalog by intent; returns matching tool names."
                .to_string(),
            parameters: one_string_param("query", "what you want to do"),
        }];
        defs.extend(self.active.iter().map(|t| ToolDef {
            name: t.name().to_string(),
            description: t.intent().to_string(),
            parameters: t.parameters(),
        }));
        defs
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

    /// The file a call will write, if its tool declares one. Lets the pump
    /// snapshot-and-restore around the gate check.
    pub fn target_path(&self, call: &ToolCall) -> Option<PathBuf> {
        self.find(&call.name)
            .and_then(|t| t.target_path(&call.arguments))
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

/// A trivial tool that returns its arguments verbatim. Kept only to exercise the
/// dispatch path in tests; the live binary advertises no such tool (the real
/// tools are aden's, discovered on demand), so it is test-only.
#[cfg(test)]
pub struct EchoTool;

#[cfg(test)]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn run(&self, arguments: &str) -> ToolResult {
        Ok(arguments.to_string())
    }
}

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

    fn parameters(&self) -> serde_json::Value {
        one_string_param("anchor", "the aden anchor to assemble")
    }

    fn run(&self, arguments: &str) -> ToolResult {
        let anchor = arg(arguments, "anchor");
        if anchor.is_empty() {
            return Err("aden_asm needs an anchor argument".to_string());
        }
        crate::aden::pull(&self.dir, crate::aden::Pull::Asm(&anchor)).map_err(|e| e.to_string())
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

    fn parameters(&self) -> serde_json::Value {
        one_string_param("symbol", "the symbol name to understand")
    }

    fn run(&self, arguments: &str) -> ToolResult {
        let symbol = arg(arguments, "symbol");
        if symbol.is_empty() {
            return Err("aden_understand needs a symbol argument".to_string());
        }
        crate::aden::pull(&self.dir, crate::aden::Pull::Understand(&symbol))
            .map_err(|e| e.to_string())
    }
}

/// Resolve a tool's `path` argument under the project root. Returns `None` for an
/// empty path so the pump skips snapshotting.
fn rooted_path(dir: &Path, arguments: &str) -> Option<PathBuf> {
    let path = arg(arguments, "path");
    (!path.is_empty()).then(|| dir.join(path))
}

/// Action tool: replace an exact string in a file. Surgical and token-light --
/// the model sends only the slice that changes, which also keeps aden's
/// blast-radius diff tight. The pump gates the result via `impact-diff --scope`
/// and reverts the file if the edit escapes scope; the tool itself only applies.
pub struct EditTool {
    dir: PathBuf,
}

impl EditTool {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn intent(&self) -> &str {
        "replace an exact string in a file with another (the change is gated by aden)"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "file path relative to the project root" },
                "old_string": { "type": "string", "description": "exact text to replace; must occur exactly once" },
                "new_string": { "type": "string", "description": "replacement text" },
            },
            "required": ["path", "old_string", "new_string"],
        })
    }

    fn mutates(&self) -> bool {
        true
    }

    fn target_path(&self, arguments: &str) -> Option<PathBuf> {
        rooted_path(&self.dir, arguments)
    }

    fn run(&self, arguments: &str) -> ToolResult {
        let path = arg(arguments, "path");
        let old = arg(arguments, "old_string");
        let new = arg(arguments, "new_string");
        if path.is_empty() || old.is_empty() {
            return Err("edit needs path and old_string".to_string());
        }
        let full = self.dir.join(&path);
        let text =
            std::fs::read_to_string(&full).map_err(|e| format!("cannot read {path}: {e}"))?;
        match text.matches(&old).count() {
            0 => return Err(format!("old_string not found in {path}")),
            1 => {}
            n => {
                return Err(format!(
                    "old_string occurs {n} times in {path}; make it unique"
                ));
            }
        }
        let updated = text.replacen(&old, &new, 1);
        std::fs::write(&full, updated).map_err(|e| format!("cannot write {path}: {e}"))?;
        Ok(format!("edited {path}"))
    }
}

/// Action tool: create or overwrite a whole file. The pump gates the result and
/// reverts the file (or removes a newly created one) if the write escapes scope.
pub struct WriteTool {
    dir: PathBuf,
}

impl WriteTool {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn intent(&self) -> &str {
        "create or overwrite a file with the given content (the change is gated by aden)"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "file path relative to the project root" },
                "content": { "type": "string", "description": "the full new file content" },
            },
            "required": ["path", "content"],
        })
    }

    fn mutates(&self) -> bool {
        true
    }

    fn target_path(&self, arguments: &str) -> Option<PathBuf> {
        rooted_path(&self.dir, arguments)
    }

    fn run(&self, arguments: &str) -> ToolResult {
        let path = arg(arguments, "path");
        let content = arg(arguments, "content");
        if path.is_empty() {
            return Err("write_file needs a path".to_string());
        }
        let full = self.dir.join(&path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create directories for {path}: {e}"))?;
        }
        std::fs::write(&full, &content).map_err(|e| format!("cannot write {path}: {e}"))?;
        Ok(format!("wrote {} bytes to {path}", content.len()))
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

    /// The advertised tool names, for assertions.
    fn advertised_names(r: &ToolRegistry) -> Vec<String> {
        r.advertised_defs().into_iter().map(|d| d.name).collect()
    }

    #[test]
    fn defs_advertise_the_discovery_seam_and_active_tools() {
        // The discovery seam is always advertised; echo is active.
        assert_eq!(
            advertised_names(&registry()),
            vec![DISCOVER.to_string(), "echo".to_string()]
        );
        // Each advertised tool carries an argument schema.
        let defs = registry().advertised_defs();
        assert_eq!(defs[0].parameters["type"], "object");
    }

    #[test]
    fn latent_tools_are_discoverable_but_not_advertised() {
        let mut r = ToolRegistry::new();
        r.register_latent(Box::new(AsmTool::new(PathBuf::from("."))));
        r.register_latent(Box::new(UnderstandTool::new(PathBuf::from("."))));

        // Latent tools stay out of the advertised list (no bloat by default).
        assert_eq!(advertised_names(&r), vec![DISCOVER.to_string()]);

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
        // A JSON object missing the field is also empty -> error.
        assert!(asm.run("{}").is_err());
    }

    #[test]
    fn arg_reads_json_field_or_bare_string() {
        // Function-calling JSON args: pull the named field.
        assert_eq!(arg(r#"{"anchor":"foo"}"#, "anchor"), "foo");
        // Bare-string args (stub/tests): use the whole trimmed value.
        assert_eq!(arg("  foo  ", "anchor"), "foo");
        // Missing field -> empty.
        assert_eq!(arg(r#"{"other":"x"}"#, "anchor"), "");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("coxn-tools-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&d).expect("create temp dir");
        d
    }

    #[test]
    fn edit_replaces_a_unique_string() {
        let dir = temp_dir("edit-ok");
        std::fs::write(dir.join("f.txt"), "foo bar baz").unwrap();
        let tool = EditTool::new(dir.clone());
        assert!(tool.mutates());
        let args = r#"{"path":"f.txt","old_string":"bar","new_string":"BAR"}"#;
        assert_eq!(tool.run(args), Ok("edited f.txt".to_string()));
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "foo BAR baz"
        );
        // It declares the file it writes, so the pump can snapshot it.
        assert_eq!(tool.target_path(args), Some(dir.join("f.txt")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_rejects_missing_or_ambiguous_old_string() {
        let dir = temp_dir("edit-bad");
        std::fs::write(dir.join("f.txt"), "a a a").unwrap();
        let tool = EditTool::new(dir.clone());
        let missing = tool.run(r#"{"path":"f.txt","old_string":"z","new_string":"Z"}"#);
        assert!(missing.unwrap_err().contains("not found"));
        let ambiguous = tool.run(r#"{"path":"f.txt","old_string":"a","new_string":"b"}"#);
        assert!(ambiguous.unwrap_err().contains("occurs 3 times"));
        // The file is untouched when the edit is rejected.
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "a a a");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_file_creates_the_file_and_parent_dirs() {
        let dir = temp_dir("write");
        let tool = WriteTool::new(dir.clone());
        assert!(tool.mutates());
        let args = r#"{"path":"sub/new.txt","content":"hi there"}"#;
        assert_eq!(
            tool.run(args),
            Ok("wrote 8 bytes to sub/new.txt".to_string())
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sub/new.txt")).unwrap(),
            "hi there"
        );
        assert_eq!(tool.target_path(args), Some(dir.join("sub/new.txt")));
        std::fs::remove_dir_all(&dir).ok();
    }
}
