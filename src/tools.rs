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

/// Read a boolean argument from a tool-call payload (a JSON object's `key`
/// field), defaulting to `false` when absent or not a bool.
fn arg_bool(arguments: &str, key: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|v| v.get(key).and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
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
    /// Whether a gate-blocked mutation can be undone by restoring a single-file
    /// snapshot. True for file edits (they declare a [`Tool::target_path`]);
    /// false for a command, whose arbitrary effects coxn cannot snapshot -- for
    /// it the gate degrades to detecting and reporting scope impact rather than
    /// reverting. Only consulted for mutating tools.
    fn revertible(&self) -> bool {
        true
    }
    /// The file this call will write, for a mutating tool, so the pump can
    /// snapshot it before applying and restore it if the gate blocks the edit.
    /// `None` for tools that touch no single file (the default).
    fn target_path(&self, _arguments: &str) -> Option<PathBuf> {
        None
    }
    /// The sandbox parameters for a shell-command tool: `(root_dir, use_bwrap)`.
    /// Only `RunTool` overrides this; all other tools return `None`.
    fn sandbox_params(&self) -> Option<(&Path, bool)> {
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
    ///
    /// Used by the discovery seam test suite to confirm that latent registration
    /// works. In production, aden tools are either active (when aden is present)
    /// or absent entirely; latent registration is not used in the live binary path.
    #[allow(dead_code)]
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

    /// Whether the named tool's mutation can be reverted by a single-file
    /// snapshot (file edits) or not (a command). Unknown tools default to
    /// revertible; dispatch handles the error path.
    pub fn revertible(&self, name: &str) -> bool {
        self.find(name).map(|t| t.revertible()).unwrap_or(true)
    }

    /// The file a call will write, if its tool declares one. Lets the pump
    /// snapshot-and-restore around the gate check.
    pub fn target_path(&self, call: &ToolCall) -> Option<PathBuf> {
        self.find(&call.name)
            .and_then(|t| t.target_path(&call.arguments))
    }

    /// The sandbox parameters for the active `run_command` tool, if present.
    /// Returns `(root_dir, use_bwrap)` so the streaming path can spawn the child
    /// without borrowing the registry across an await point.
    pub fn run_command_params(&self) -> Option<(&Path, bool)> {
        self.find("run_command").and_then(|t| t.sandbox_params())
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

/// Which read-only aden query an [`AdenTool`] runs. aden is the context layer:
/// the model reaches code through these dense, structure-aware queries rather
/// than raw file reads.
#[derive(Clone, Copy)]
enum AdenQuery {
    Asm,
    Understand,
    Grep,
    Ask,
    Locate,
}

/// Pull-context tool over one aden read query. The model calls these to pull
/// blast-radius, search, and comprehension on demand (pull, not push); aden is
/// the bloat arbiter and coxn only relays.
pub struct AdenTool {
    dir: PathBuf,
    query: AdenQuery,
}

impl AdenTool {
    pub fn asm(dir: PathBuf) -> Self {
        Self {
            dir,
            query: AdenQuery::Asm,
        }
    }
    pub fn understand(dir: PathBuf) -> Self {
        Self {
            dir,
            query: AdenQuery::Understand,
        }
    }
    pub fn grep(dir: PathBuf) -> Self {
        Self {
            dir,
            query: AdenQuery::Grep,
        }
    }
    pub fn ask(dir: PathBuf) -> Self {
        Self {
            dir,
            query: AdenQuery::Ask,
        }
    }
    pub fn locate(dir: PathBuf) -> Self {
        Self {
            dir,
            query: AdenQuery::Locate,
        }
    }

    /// The argument key this query reads from the call payload.
    fn arg_key(&self) -> &'static str {
        match self.query {
            AdenQuery::Asm => "anchor",
            AdenQuery::Understand | AdenQuery::Locate => "symbol",
            AdenQuery::Grep => "pattern",
            AdenQuery::Ask => "question",
        }
    }
}

impl Tool for AdenTool {
    fn name(&self) -> &str {
        match self.query {
            AdenQuery::Asm => "aden_asm",
            AdenQuery::Understand => "aden_understand",
            AdenQuery::Grep => "aden_grep",
            AdenQuery::Ask => "aden_ask",
            AdenQuery::Locate => "aden_locate",
        }
    }

    fn intent(&self) -> &str {
        match self.query {
            AdenQuery::Asm => "assemble an anchor's graph neighborhood (blast radius and context)",
            AdenQuery::Understand => "a symbol's definition, callers, and downstream impact",
            AdenQuery::Grep => "structure-aware code search; each hit tagged with its symbol",
            AdenQuery::Ask => "ask a natural-language question about the codebase",
            AdenQuery::Locate => "find a symbol's definition and its call sites",
        }
    }

    fn parameters(&self) -> serde_json::Value {
        let key = self.arg_key();
        let desc = match self.query {
            AdenQuery::Asm => "the aden anchor to assemble",
            AdenQuery::Understand => "the symbol name to understand",
            AdenQuery::Grep => "the search pattern",
            AdenQuery::Ask => "the question to answer",
            AdenQuery::Locate => "the symbol name to locate",
        };
        one_string_param(key, desc)
    }

    fn run(&self, arguments: &str) -> ToolResult {
        let value = arg(arguments, self.arg_key());
        if value.is_empty() {
            return Err(format!(
                "{} needs a {} argument",
                self.name(),
                self.arg_key()
            ));
        }
        let pull = match self.query {
            AdenQuery::Asm => crate::aden::Pull::Asm(&value),
            AdenQuery::Understand => crate::aden::Pull::Understand(&value),
            AdenQuery::Grep => crate::aden::Pull::Grep(&value),
            AdenQuery::Ask => crate::aden::Pull::Ask(&value),
            AdenQuery::Locate => crate::aden::Pull::Locate(&value),
        };
        crate::aden::pull(&self.dir, pull).map_err(|e| e.to_string())
    }
}

/// The most a `read_file` returns inline before truncating, so a huge file can
/// not blow up the context. The model can fall back to `aden_grep` for scale.
const READ_FILE_CAP: usize = 50_000;

/// Read a file's exact contents under the project root. The companion to `edit`:
/// the model reads verbatim text here to get the precise `old_string` to replace
/// (aden's context is dense/summarized, not byte-exact). Read-only, so the pump
/// does not gate it; confined to the project root like the action tools.
pub struct ReadFileTool {
    dir: PathBuf,
}

impl ReadFileTool {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn intent(&self) -> &str {
        "read a file's exact contents (use before edit to get the text to replace)"
    }

    fn parameters(&self) -> serde_json::Value {
        one_string_param("path", "file path relative to the project root")
    }

    fn run(&self, arguments: &str) -> ToolResult {
        let path = arg(arguments, "path");
        let full = project_path(&self.dir, &path)?;
        let text =
            std::fs::read_to_string(&full).map_err(|e| format!("cannot read {path}: {e}"))?;
        if text.len() > READ_FILE_CAP {
            let head: String = text.chars().take(READ_FILE_CAP).collect();
            Ok(format!(
                "{head}\n[truncated: {path} is {} bytes; use run_command with grep, or aden_grep when available]",
                text.len()
            ))
        } else {
            Ok(text)
        }
    }
}

/// Resolve a `path` argument under the project root, rejecting anything that
/// could escape it. An action tool writes to the working tree; the gate only
/// judges the tree's git diff, so a write outside the tree (an absolute path or
/// `..`) would land ungated. Only a relative path of normal components is
/// allowed -- no root, no prefix, no `..`.
///
/// Symlink safety: after validating components, the function canonicalizes the
/// deepest existing ancestor of the joined path and confirms it is inside the
/// canonical project root. A symlink that resolves outside the root is rejected.
/// A broken symlink in the chain causes canonicalize to fail, which is also
/// rejected (safe default). The original joined path (not the canonical one) is
/// returned so that not-yet-created suffix components remain plain Normal
/// segments -- WriteTool's create_dir_all runs after this returns.
fn project_path(dir: &Path, path: &str) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("a relative path is required".to_string());
    }
    for component in Path::new(path).components() {
        match component {
            std::path::Component::Normal(_) | std::path::Component::CurDir => {}
            _ => {
                return Err(format!(
                    "path must be relative to the project root with no '..' or absolute segments: {path}"
                ));
            }
        }
    }
    let joined = dir.join(path);
    let canonical_root =
        std::fs::canonicalize(dir).map_err(|e| format!("cannot resolve project root: {e}"))?;
    // Walk to the deepest existing ancestor of `joined` so we can canonicalize
    // it. Use `symlink_metadata` (lstat, does not follow): a dangling symlink
    // component is an existing entry, so we stop on it and canonicalize -- which
    // fails for a broken link and rejects the path. `try_exists` would follow
    // the link, see the missing target, skip the component, and let a write
    // escape through it.
    let mut ancestor: &Path = &joined;
    loop {
        if ancestor.symlink_metadata().is_ok() {
            break;
        }
        match ancestor.parent() {
            Some(p) => ancestor = p,
            None => {
                // Exhausted the path; fall back to the project root itself.
                ancestor = dir;
                break;
            }
        }
    }
    let canonical = std::fs::canonicalize(ancestor)
        .map_err(|e| format!("path cannot be verified within the project root: {e}"))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(format!("path escapes the project root: {path}"));
    }
    Ok(joined)
}

/// The file a `path`-taking tool will write, for the pump to snapshot. `None`
/// when the path is missing or would escape the project root (the run will
/// reject it too).
fn rooted_path(dir: &Path, arguments: &str) -> Option<PathBuf> {
    project_path(dir, &arg(arguments, "path")).ok()
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
        if old.is_empty() {
            return Err("edit needs old_string".to_string());
        }
        let full = project_path(&self.dir, &path)?;
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
        let full = project_path(&self.dir, &path)?;
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create directories for {path}: {e}"))?;
        }
        std::fs::write(&full, &content).map_err(|e| format!("cannot write {path}: {e}"))?;
        Ok(format!("wrote {} bytes to {path}", content.len()))
    }
}

/// Action tool: run a shell command, confined by the sandbox. The riskiest
/// capability coxn exposes -- arbitrary execution -- so it is always approval
/// gated (`mutates` is true) and, when bwrap is present, namespace-confined to
/// the project root with no network by default (see [`crate::sandbox`]). A
/// command's effects are not a single-file edit, so it is not revertible: the
/// aden gate degrades to detection-and-report for it (see the pump).
pub struct RunTool {
    dir: PathBuf,
    /// Whether bwrap was found at startup; selects sandbox vs direct-exec.
    bwrap: bool,
}

impl RunTool {
    pub fn new(dir: PathBuf, bwrap: bool) -> Self {
        Self { dir, bwrap }
    }
}

impl Tool for RunTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn intent(&self) -> &str {
        "run a shell command in a sandbox (project root writable, no network unless requested) -- use for builds, tests, git, listing files"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "the shell command to run (sh -c) in the project root" },
                "network": { "type": "boolean", "description": "set true ONLY if the command needs network access (e.g. fetching dependencies); off by default" },
            },
            "required": ["command"],
        })
    }

    fn mutates(&self) -> bool {
        // Not necessarily a mutation, but the riskiest call there is: route it
        // through the pump's approval gate like the editors.
        true
    }

    fn revertible(&self) -> bool {
        // A command's arbitrary effects cannot be undone by a file snapshot.
        false
    }

    fn sandbox_params(&self) -> Option<(&Path, bool)> {
        Some((&self.dir, self.bwrap))
    }

    fn run(&self, arguments: &str) -> ToolResult {
        let command = arg(arguments, "command");
        if command.trim().is_empty() {
            return Err("run_command needs a command argument".to_string());
        }
        let network = arg_bool(arguments, "network");
        Ok(format_run(&crate::sandbox::run(
            &self.dir, &command, network, self.bwrap,
        )))
    }
}

/// Render a command's outcome as the text fed back to the model. The first line
/// starts with "cmd:" so the transcript can identify it as a command result and
/// render it with a distinct style. Format:
///
/// - Header line: `cmd: sandboxed` or `cmd: NO SANDBOX (approval was the only gate)`
/// - Exit line: `ok: exit 0` / `err: exit N` / `err: timed out` / `err: killed by signal`
/// - Then the output (or `(no output)`)
pub(crate) fn format_run(outcome: &crate::sandbox::RunOutcome) -> String {
    let mut s = String::new();
    // Header line (always starts with "cmd:").
    if outcome.confinement == crate::sandbox::Confinement::Unsandboxed {
        s.push_str("cmd: NO SANDBOX (approval was the only gate)\n");
    } else {
        s.push_str("cmd: sandboxed\n");
    }
    // Exit / status line.
    if outcome.timed_out {
        s.push_str("err: timed out\n");
    } else {
        match outcome.exit_code {
            Some(0) => s.push_str("ok: exit 0\n"),
            Some(code) => s.push_str(&format!("err: exit {code}\n")),
            None => s.push_str("err: killed by signal\n"),
        }
    }
    if outcome.output.is_empty() {
        s.push_str("(no output)");
    } else {
        s.push_str(&outcome.output);
    }
    s
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
        r.register_latent(Box::new(AdenTool::asm(PathBuf::from("."))));
        r.register_latent(Box::new(AdenTool::understand(PathBuf::from("."))));

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
        r.register_latent(Box::new(AdenTool::asm(PathBuf::from("."))));
        // Dispatchable even though latent; empty arg proves the tool actually ran.
        assert!(r.dispatch(&call("aden_asm", "")).is_err());
        // Still unknown if never registered.
        assert!(r.dispatch(&call("ghost", "")).is_err());
    }

    #[test]
    fn aden_tools_reject_empty_arguments() {
        let asm = AdenTool::asm(PathBuf::from("."));
        let understand = AdenTool::understand(PathBuf::from("."));
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
    fn aden_tools_cover_the_read_and_search_surface() {
        let d = PathBuf::from(".");
        assert_eq!(AdenTool::asm(d.clone()).name(), "aden_asm");
        assert_eq!(AdenTool::understand(d.clone()).name(), "aden_understand");
        assert_eq!(AdenTool::grep(d.clone()).name(), "aden_grep");
        assert_eq!(AdenTool::ask(d.clone()).name(), "aden_ask");
        assert_eq!(AdenTool::locate(d.clone()).name(), "aden_locate");
        // Each is read-only (never gated) and rejects an empty arg before shelling out.
        let grep = AdenTool::grep(d.clone());
        assert!(!grep.mutates());
        assert!(grep.run("{}").is_err());
        assert!(AdenTool::ask(d).run(r#"{"other":"x"}"#).is_err());
    }

    #[test]
    fn read_file_returns_exact_contents_and_confines_to_root() {
        let dir = temp_dir("read");
        std::fs::write(dir.join("a.txt"), "exact\ncontents").unwrap();
        let tool = ReadFileTool::new(dir.clone());
        assert!(!tool.mutates());
        assert_eq!(
            tool.run(r#"{"path":"a.txt"}"#),
            Ok("exact\ncontents".to_string())
        );
        // Reads are confined to the project root, like the action tools.
        assert!(
            tool.run(r#"{"path":"/etc/passwd"}"#)
                .unwrap_err()
                .contains("relative")
        );
        assert!(
            tool.run(r#"{"path":"../../x"}"#)
                .unwrap_err()
                .contains("relative")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_file_truncates_a_large_file() {
        let dir = temp_dir("read-big");
        std::fs::write(dir.join("big.txt"), "x".repeat(READ_FILE_CAP + 100)).unwrap();
        let out = ReadFileTool::new(dir.clone())
            .run(r#"{"path":"big.txt"}"#)
            .unwrap();
        assert!(out.contains("[truncated"), "{out:.80}");
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
    fn action_tools_reject_paths_that_escape_the_project_root() {
        let dir = temp_dir("escape");
        let edit = EditTool::new(dir.clone());
        let write = WriteTool::new(dir.clone());
        // Absolute and parent-traversal paths are refused before any write, and
        // expose no target to snapshot.
        for bad in [
            r#"{"path":"/etc/passwd","old_string":"x","new_string":"y"}"#,
            r#"{"path":"../../escape.txt","old_string":"x","new_string":"y"}"#,
            r#"{"path":"src/../../up.txt","old_string":"x","new_string":"y"}"#,
        ] {
            assert!(edit.run(bad).unwrap_err().contains("relative"), "{bad}");
            assert!(edit.target_path(bad).is_none(), "{bad}");
        }
        let bad_write = r#"{"path":"/tmp/coxn-escape-probe","content":"x"}"#;
        assert!(write.run(bad_write).unwrap_err().contains("relative"));
        assert!(!std::path::Path::new("/tmp/coxn-escape-probe").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn arg_bool_reads_json_bool_or_defaults_false() {
        assert!(arg_bool(r#"{"network":true}"#, "network"));
        assert!(!arg_bool(r#"{"network":false}"#, "network"));
        assert!(!arg_bool(r#"{"other":true}"#, "network"));
        assert!(!arg_bool("not json", "network"));
    }

    #[test]
    fn run_tool_is_gated_and_not_revertible() {
        let tool = RunTool::new(PathBuf::from("."), false);
        assert_eq!(tool.name(), "run_command");
        // Always approval-gated, never snapshot-revertible, no single target.
        assert!(tool.mutates());
        assert!(!tool.revertible());
        assert!(tool.target_path(r#"{"command":"ls"}"#).is_none());
        // An empty command is rejected before launching anything.
        assert!(tool.run(r#"{"command":"  "}"#).is_err());
    }

    #[test]
    fn run_tool_executes_via_fallback_and_reports_exit() {
        // bwrap=false exercises the portable direct-exec path (no sandbox dep).
        let tool = RunTool::new(std::env::temp_dir(), false);
        let out = tool.run(r#"{"command":"printf hello"}"#).expect("runs");
        // "ok: exit 0" contains "exit 0".
        assert!(out.contains("exit 0"), "{out}");
        assert!(out.contains("hello"), "{out}");
        // New header for the unsandboxed path.
        assert!(out.contains("NO SANDBOX"), "fallback warns: {out}");
        let bad = tool.run(r#"{"command":"exit 7"}"#).expect("runs");
        // "err: exit 7" contains "exit 7".
        assert!(bad.contains("exit 7"), "{bad}");
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

    #[test]
    fn symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir("symlink-escape");
        // A symlink inside the project root that points outside (to /tmp).
        symlink("/tmp", dir.join("escape")).expect("create symlink");
        // Attempting to access a path through the escaping symlink must fail.
        let edit = EditTool::new(dir.clone());
        let err = edit
            .run(r#"{"path":"escape/foo","old_string":"x","new_string":"y"}"#)
            .unwrap_err();
        assert!(err.contains("escape"), "expected 'escape' in: {err}");
        let read = ReadFileTool::new(dir.clone());
        let err = read.run(r#"{"path":"escape/foo"}"#).unwrap_err();
        assert!(err.contains("escape"), "expected 'escape' in: {err}");
        let write = WriteTool::new(dir.clone());
        let err = write
            .run(r#"{"path":"escape/foo","content":"x"}"#)
            .unwrap_err();
        assert!(err.contains("escape"), "expected 'escape' in: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dangling_symlink_component_is_rejected() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir("symlink-dangling");
        // A symlink to a path that does NOT exist yet, outside the root. The
        // ancestor walk must stop on the symlink entry (not follow it to the
        // missing target), canonicalize it, and reject -- otherwise a write
        // would follow the link and create the file outside the project.
        let outside = std::env::temp_dir().join(format!("coxn-evil-{}", std::process::id()));
        std::fs::remove_dir_all(&outside).ok();
        symlink(&outside, dir.join("evil")).expect("create dangling symlink");
        let write = WriteTool::new(dir.clone());
        let err = write
            .run(r#"{"path":"evil/foo","content":"x"}"#)
            .unwrap_err();
        assert!(
            err.contains("escape") || err.contains("verified"),
            "expected rejection, got: {err}"
        );
        // Nothing was created outside the root.
        assert!(!outside.exists(), "write escaped to {}", outside.display());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn normal_in_root_file_still_reads_and_edits() {
        let dir = temp_dir("symlink-safe-read");
        std::fs::write(dir.join("ok.txt"), "hello world").unwrap();
        // Read succeeds for a plain in-root file.
        let read = ReadFileTool::new(dir.clone());
        assert_eq!(
            read.run(r#"{"path":"ok.txt"}"#),
            Ok("hello world".to_string())
        );
        // Edit succeeds for a plain in-root file.
        let edit = EditTool::new(dir.clone());
        assert_eq!(
            edit.run(r#"{"path":"ok.txt","old_string":"hello","new_string":"goodbye"}"#),
            Ok("edited ok.txt".to_string())
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("ok.txt")).unwrap(),
            "goodbye world"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_to_new_nested_dir_succeeds() {
        let dir = temp_dir("symlink-safe-write");
        let write = WriteTool::new(dir.clone());
        // Writing to a path whose intermediate directories do not yet exist must
        // succeed: the deepest-existing-ancestor walk falls back to an existing
        // ancestor, confirms it is inside the root, then returns the original
        // joined path so WriteTool's create_dir_all can make the new dirs.
        assert_eq!(
            write.run(r#"{"path":"sub/dir/new.txt","content":"data"}"#),
            Ok("wrote 4 bytes to sub/dir/new.txt".to_string())
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sub/dir/new.txt")).unwrap(),
            "data"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
