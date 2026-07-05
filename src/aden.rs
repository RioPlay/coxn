//! The aden seam: coxn shells out to the `aden` binary.
//!
//! coxn carries no intelligence; aden directs and gates. This is the thin
//! boundary that runs aden subcommands and reads their exit codes and text.
//! aden is a subprocess, not a linked crate, so coxn keeps its three-dependency
//! budget (DESIGN allows either; the dep rule forces this). The gate's
//! exit-code contract and text output (see docs/contract.adoc) are shaped for
//! exactly this.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::gate::{Gate, GateOutcome, GateVerdict};

/// Env flag for read-only aden subprocesses. Suppresses `ensure_fresh` silent
/// `gen` so coxn (or MCP) can own explicit indexing without store lock fights.
const ADEN_SKIP_AUTO_GEN: &str = "ADEN_SKIP_AUTO_GEN";

fn read_only_aden_env(cmd: &mut Command) {
    cmd.env(ADEN_SKIP_AUTO_GEN, "1");
}

/// The aden binary to invoke. `COXN_ADEN_BIN` overrides it (e.g. to point at a
/// dev build or the offline branch); otherwise `aden` on PATH.
fn aden_bin() -> String {
    std::env::var("COXN_ADEN_BIN").unwrap_or_else(|_| "aden".to_string())
}

/// What aden can do in this session, probed once at startup.
///
/// When `available` is false all other fields are `None` and every aden
/// operation degrades gracefully. Callers must never shell out to aden again
/// when `available` is false; they read from this cached result instead.
pub struct AdenCaps {
    pub available: bool,
    pub model_base_url: Option<String>,
    pub model_name: Option<String>,
}

/// Probe once at boot: can `aden` run from this environment?
///
/// Shells `<aden_bin> --version` with stdout/stderr discarded. If the process
/// cannot spawn (binary not on PATH, not executable, etc.) or exits non-zero,
/// returns `AdenCaps { available: false, .. }`. If it runs, reads
/// `model.base_url` and `model.name` from `.aden/config.toml` and returns them
/// alongside `available: true`. Reading config happens here so the rest of
/// startup can use `caps` without shelling out again.
pub fn probe(dir: &Path) -> AdenCaps {
    let bin = aden_bin();
    let ok = Command::new(&bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return AdenCaps {
            available: false,
            model_base_url: None,
            model_name: None,
        };
    }
    AdenCaps {
        available: true,
        model_base_url: config_get(dir, "model.base_url"),
        model_name: config_get(dir, "model.name"),
    }
}

/// An aden invocation that could not run, or ran but failed.
#[derive(Debug)]
pub enum AdenError {
    /// The process could not be spawned (aden not found, not executable, ...).
    Spawn(String),
    /// aden ran but exited non-zero (for commands where that means failure).
    Failed { code: Option<i32>, stderr: String },
}

impl std::fmt::Display for AdenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdenError::Spawn(msg) => write!(f, "could not run aden: {msg}"),
            AdenError::Failed { code, stderr } => {
                write!(f, "aden failed (exit {code:?}): {stderr}")
            }
        }
    }
}

impl std::error::Error for AdenError {}

/// What to pull from the graph on the model's behalf. Each maps to a read-only
/// aden subcommand; the model reaches code through these dense, structure-aware
/// queries rather than raw file reads (aden is the context layer).
pub enum Pull<'a> {
    /// Assemble the neighborhood for an anchor (`aden asm --from`).
    Asm(&'a str),
    /// Definition + callers + downstream impact for a symbol (`aden understand`).
    Understand(&'a str),
    /// Structure-aware content search, each hit tagged with its symbol (`aden grep`).
    Grep(&'a str),
    /// Natural-language question resolved to a subgraph (`aden ask`).
    Ask(&'a str),
    /// A symbol's definition and call sites (`aden locate`).
    Locate(&'a str),
    /// Blast radius / downstream impact (`aden query --impact`).
    Impact(&'a str),
}

/// Run the blast-radius gate for `manifest` against the working tree at `dir`.
/// The verdict is decoded from aden's exit code; a spawn failure is a closed
/// gate ([`GateVerdict::Error`]) because a gate that cannot run must not let an
/// edit through.
pub fn gate(dir: &Path, manifest: &Path) -> GateOutcome {
    gate_with(&aden_bin(), dir, manifest)
}

/// Emit the scope manifest JSON for a task (`aden scope <name> --seed ... --json`).
pub fn scope(dir: &Path, name: &str, seeds: &[String], budget: u64) -> Result<String, AdenError> {
    scope_with(&aden_bin(), dir, name, seeds, budget)
}

/// Emit a task partition (`aden scope --agents <name> --seed ...`): per-sub-scope
/// manifests under `.aden/agents/` plus the index on stdout (returned here).
pub fn scope_agents(
    dir: &Path,
    name: &str,
    seeds: &[String],
    budget: u64,
) -> Result<String, AdenError> {
    let mut cmd = Command::new(aden_bin());
    read_only_aden_env(&mut cmd);
    cmd.arg("scope").arg("--agents").arg(name);
    for s in seeds {
        cmd.arg("--seed").arg(s);
    }
    cmd.arg("--budget").arg(budget.to_string()).arg(dir);
    run_text(cmd)
}

/// Pull context from aden (asm / understand), returning aden's text output.
pub fn pull(dir: &Path, what: Pull) -> Result<String, AdenError> {
    pull_with(&aden_bin(), dir, what)
}

/// One explicit write: incremental `aden gen --quiet`. coxn calls this at boot
/// (when aden is available) so every later read can set [`ADEN_SKIP_AUTO_GEN`]
/// without fighting the store lock.
pub fn ensure_indexed(dir: &Path) -> Result<(), AdenError> {
    let mut cmd = Command::new(aden_bin());
    cmd.arg("gen").arg("--quiet").arg(dir);
    run_text(cmd).map(|_| ())
}

fn gate_with(bin: &str, dir: &Path, manifest: &Path) -> GateOutcome {
    let mut cmd = Command::new(bin);
    read_only_aden_env(&mut cmd);
    cmd.arg("impact-diff").arg("--scope").arg(manifest).arg(dir);
    match cmd.output() {
        Ok(out) => GateOutcome {
            verdict: GateVerdict::from_exit_code(out.status.code().unwrap_or(-1)),
            message: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        },
        Err(e) => GateOutcome {
            verdict: GateVerdict::Error(-1),
            message: format!("aden gate could not run: {e}"),
        },
    }
}

fn scope_with(
    bin: &str,
    dir: &Path,
    name: &str,
    seeds: &[String],
    budget: u64,
) -> Result<String, AdenError> {
    let mut cmd = Command::new(bin);
    read_only_aden_env(&mut cmd);
    cmd.arg("scope").arg(name);
    for s in seeds {
        cmd.arg("--seed").arg(s);
    }
    cmd.arg("--budget").arg(budget.to_string());
    cmd.arg("--json").arg(dir);
    run_text(cmd)
}

/// Extract the seeds array from a scope manifest JSON (written by `aden scope`
/// or `aden scope --agents`). Used by the sub-agent runner so each sub-scope
/// can assemble its own aden-provided context via the same pull-Asms pattern
/// as top-level load_task, without requiring coxn to understand the full
/// manifest shape beyond the seeds list.
#[allow(dead_code)] // Phase 5 sub-agent runner substrate (used by execute + tests)
pub fn seeds_from_manifest(path: &Path) -> Result<Vec<String>, AdenError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| AdenError::Spawn(format!("reading manifest {}: {}", path.display(), e)))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| AdenError::Failed {
        code: None,
        stderr: format!("invalid json in {}: {}", path.display(), e),
    })?;
    let seeds = v
        .get("seeds")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    Ok(seeds)
}

/// Extract `context.budget` from a scope manifest JSON.
///
/// Missing or non-numeric budgets return `Ok(None)` so callers can fall back to
/// the task-level budget without treating older manifests as errors.
/// Extract the manifest's `files` mandate (repo-relative paths the scope may
/// touch). Used by the parallel scheduler to verify two scopes' working-tree
/// mandates are disjoint before running them concurrently. Missing or
/// non-array `files` returns `Ok(vec![])` so a manifest without a mandate is
/// treated as no-known-files (never parallelizable, always serialized).
pub fn files_from_manifest(path: &Path) -> Result<Vec<String>, AdenError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| AdenError::Spawn(format!("reading manifest {}: {}", path.display(), e)))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| AdenError::Failed {
        code: None,
        stderr: format!("invalid json in {}: {}", path.display(), e),
    })?;
    Ok(v.get("files")
        .and_then(|f| f.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default())
}

pub fn budget_from_manifest(path: &Path) -> Result<Option<u64>, AdenError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| AdenError::Spawn(format!("reading manifest {}: {}", path.display(), e)))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| AdenError::Failed {
        code: None,
        stderr: format!("invalid json in {}: {}", path.display(), e),
    })?;
    Ok(v.get("context")
        .and_then(|c| c.get("budget"))
        .and_then(|b| b.as_u64()))
}

fn pull_with(bin: &str, dir: &Path, what: Pull) -> Result<String, AdenError> {
    let mut cmd = Command::new(bin);
    read_only_aden_env(&mut cmd);
    match what {
        Pull::Asm(anchor) => {
            cmd.arg("asm").arg("--from").arg(anchor).arg(dir);
        }
        Pull::Understand(symbol) => {
            cmd.arg("understand").arg(symbol).arg(dir);
        }
        Pull::Grep(pattern) => {
            cmd.arg("grep").arg(pattern).arg(dir);
        }
        Pull::Ask(question) => {
            cmd.arg("ask").arg(question).arg(dir);
        }
        Pull::Locate(symbol) => {
            cmd.arg("locate").arg(symbol).arg(dir);
        }
        Pull::Impact(symbol) => {
            cmd.arg("query")
                .arg("--impact")
                .arg(symbol)
                .arg("--format")
                .arg("table")
                .arg(dir);
        }
    }
    run_text(cmd)
}

/// The real blast-radius gate: runs `aden impact-diff --scope` for a fixed
/// manifest against a working tree. Implements [`Gate`] so the pump can consult
/// it before accepting an edit without knowing aden exists.
pub struct AdenGate {
    dir: PathBuf,
    manifest: PathBuf,
}

impl AdenGate {
    pub fn new(dir: PathBuf, manifest: PathBuf) -> Self {
        Self { dir, manifest }
    }
}

impl Gate for AdenGate {
    fn check(&self) -> GateOutcome {
        gate(&self.dir, &self.manifest)
    }
}

/// Raw savings detail from the All-time line (text after `→`). Pure parse helper.
pub fn savings_detail_from_status(status: &str) -> Option<String> {
    let line = status
        .lines()
        .find(|l| l.trim_start().starts_with("All-time"))?;
    // "...All-time : N aden calls → est. ~X tool calls + ~Y tokens saved vs ..."
    let detail = line.split('→').nth(1).unwrap_or(line).trim();
    (!detail.is_empty()).then(|| detail.to_string())
}

/// Read savings from `.aden/savings.json` (no subprocess). Preferred on the TUI hot path.
pub fn savings_detail_from_file(dir: &Path) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Ledger {
        queries: u64,
        saved_tokens: i64,
        tool_calls_saved: u64,
    }
    #[derive(serde::Deserialize)]
    struct SavingsFile {
        schema: u32,
        all_time: Ledger,
    }
    let path = dir.join(".aden/savings.json");
    let json = std::fs::read_to_string(path).ok()?;
    let file: SavingsFile = serde_json::from_str(&json).ok()?;
    if file.schema != 2 || file.all_time.queries == 0 {
        return None;
    }
    let at = &file.all_time;
    if at.saved_tokens <= 0 && at.tool_calls_saved == 0 {
        return None;
    }
    let tokens = if at.saved_tokens.abs() >= 1000 {
        format!("~{}k tokens saved", at.saved_tokens.abs() / 1000)
    } else {
        format!("~{} tokens saved", at.saved_tokens.abs())
    };
    Some(format!(
        "~{} tool calls + {tokens} vs grep-and-read",
        at.tool_calls_saved
    ))
}

/// The raw All-time savings detail from `aden status` (no `aden` prefix).
/// Spawns a subprocess — avoid on the idle TUI path; use [`savings_detail_from_file`].
#[allow(dead_code)] // doctor/CLI fallback seam
pub fn savings_detail(dir: &Path) -> Option<String> {
    savings_detail_from_file(dir).or_else(|| {
        let mut cmd = Command::new(aden_bin());
        read_only_aden_env(&mut cmd);
        let out = cmd.arg("status").arg(dir).output().ok()?;
        if !out.status.success() {
            return None;
        }
        savings_detail_from_status(&String::from_utf8_lossy(&out.stdout))
    })
}

/// Status-line scope text for a savings detail (always tagged `[est.]`).
pub fn format_savings_status(detail: &str) -> String {
    let d = detail.trim();
    let body = d
        .strip_prefix("[est.]")
        .or_else(|| d.strip_prefix("est."))
        .map(str::trim)
        .unwrap_or(d);
    format!("aden [est.] {body}")
}

/// Chrome-bar scope text: shorter, still tagged `[est.]`.
pub fn format_savings_chrome(detail: &str) -> String {
    let d = detail.trim();
    let d = d
        .strip_prefix("[est.]")
        .or_else(|| d.strip_prefix("est."))
        .map(str::trim)
        .unwrap_or(d);
    if let Some((tools, tokens)) = parse_savings_detail_parts(d) {
        format!("aden [est.] {tokens} · ~{tools} tools")
    } else {
        format!("aden [est.] {d}")
    }
}

/// Parse `~N tool calls + ~Xk tokens saved vs grep-and-read`.
fn parse_savings_detail_parts(detail: &str) -> Option<(u64, String)> {
    let mut tools: Option<u64> = None;
    let mut tokens: Option<String> = None;
    for part in detail.split('+') {
        let p = part.trim();
        if let Some(rest) = p.strip_prefix("~") {
            if p.contains("tool calls") {
                tools = rest.split_whitespace().next()?.parse().ok();
            } else if p.contains("tokens saved") {
                let tok = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or(rest)
                    .trim_end_matches("tokens")
                    .trim_end_matches("saved")
                    .trim();
                tokens = Some(tok.to_string());
            }
        }
    }
    match (tools, tokens) {
        (Some(t), Some(tok)) => Some((t, tok)),
        _ => None,
    }
}

/// Read a runtime preference from `.aden/config.toml` via `aden config get`.
/// `None` when the key is unset, aden cannot run, or the value is empty. Lets
/// coxn pin a provider/model without environment variables.
pub fn config_get(dir: &Path, key: &str) -> Option<String> {
    config_get_with(&aden_bin(), dir, key)
}

fn config_get_with(bin: &str, dir: &Path, key: &str) -> Option<String> {
    let mut cmd = Command::new(bin);
    read_only_aden_env(&mut cmd);
    let out = cmd
        .arg("config")
        .arg("get")
        .arg(key)
        .arg(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Run a command expecting text on stdout; non-zero exit is an error.
fn run_text(mut cmd: Command) -> Result<String, AdenError> {
    let out = cmd.output().map_err(|e| AdenError::Spawn(e.to_string()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(AdenError::Failed {
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

/// Launch `aden view [anchor]` in the background (browser UI).
/// Non-blocking; returns immediately after spawn. The viewer is the canonical
/// rich graph surface; coxn does not duplicate it.
pub fn launch_view(dir: &Path, anchor: Option<&str>) -> std::io::Result<()> {
    let bin = aden_bin();
    let mut cmd = Command::new(&bin);
    read_only_aden_env(&mut cmd);
    cmd.arg("view");
    if let Some(a) = anchor {
        cmd.arg(a);
    }
    cmd.arg(dir);
    // Fire and forget; --no-open is user choice via aden flags/env if wanted.
    let _ = cmd.spawn()?;
    Ok(())
}

/// Export a diagram (default Mermaid) for an anchor or whole relevant slice.
/// `aden viz` supports Mermaid/DOT/JSON via flags; default to mermaid text.
pub fn diagram(dir: &Path, anchor: Option<&str>) -> Result<String, AdenError> {
    let mut cmd = Command::new(aden_bin());
    read_only_aden_env(&mut cmd);
    cmd.arg("viz");
    if let Some(a) = anchor {
        cmd.arg(a);
    }
    // Prefer mermaid when supported; fall back to default output.
    // Many installs accept --format mermaid; if not, the text still contains ```mermaid
    cmd.arg("--format").arg("mermaid");
    cmd.arg(dir);
    run_text(cmd)
}

/// Run `aden doctor` and return its diagnostic text (environment + repo health).
pub fn doctor(dir: &Path) -> Result<String, AdenError> {
    let mut cmd = Command::new(aden_bin());
    read_only_aden_env(&mut cmd);
    cmd.arg("doctor").arg(dir);
    run_text(cmd)
}

/// Run `aden communities` for functional code clusters.
pub fn communities(dir: &Path) -> Result<String, AdenError> {
    let mut cmd = Command::new(aden_bin());
    read_only_aden_env(&mut cmd);
    cmd.arg("communities").arg(dir);
    run_text(cmd)
}

/// Run `aden audit` (OWASP-aligned).
pub fn audit(dir: &Path) -> Result<String, AdenError> {
    let mut cmd = Command::new(aden_bin());
    read_only_aden_env(&mut cmd);
    cmd.arg("audit").arg(dir);
    run_text(cmd)
}

/// List graph anchors/symbols (for palette, search). Uses `aden list --json`.
/// Returns newline separated anchors (filtered if provided).
#[allow(dead_code)] // on-demand aden seam; not wired in hot-path menus
pub fn list_symbols(dir: &Path, filter: Option<&str>) -> Result<String, AdenError> {
    let mut cmd = Command::new(aden_bin());
    read_only_aden_env(&mut cmd);
    cmd.arg("list").arg("--json");
    if let Some(f) = filter {
        cmd.arg("--filter").arg(f);
    }
    cmd.arg(dir);
    let json = run_text(cmd)?;
    // Parse json to extract anchors array, join with \n for lines().
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json)
        && let Some(arr) = val.get("anchors").and_then(|a| a.as_array())
    {
        let lines: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        return Ok(lines.join("\n"));
    }
    Ok(json)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Default, serde::Deserialize, PartialEq, Eq)]
    struct SavingsLedgerJson {
        queries: u64,
        returned_tokens: u64,
        baseline_tokens: u64,
        saved_tokens: i64,
        #[serde(default)]
        tool_calls_saved: u64,
    }

    fn validate_savings_ledger(ledger: &SavingsLedgerJson) -> Result<(), String> {
        let expected = ledger.baseline_tokens as i64 - ledger.returned_tokens as i64;
        if ledger.saved_tokens != expected {
            return Err(format!(
                "saved_tokens {} != baseline_tokens - returned_tokens ({expected})",
                ledger.saved_tokens
            ));
        }
        Ok(())
    }

    #[derive(Debug, Default, serde::Deserialize)]
    struct SavingsSessionJson {
        #[serde(default, flatten)]
        ledger: SavingsLedgerJson,
    }

    #[derive(Debug, serde::Deserialize)]
    struct SavingsFileJson {
        schema: u32,
        all_time: SavingsLedgerJson,
        #[serde(default)]
        session: SavingsSessionJson,
    }

    fn validate_savings_file_json(json: &str) -> Result<(), String> {
        let file: SavingsFileJson = serde_json::from_str(json).map_err(|e| e.to_string())?;
        if file.schema != 2 {
            return Err(format!("unsupported savings schema {}", file.schema));
        }
        validate_savings_ledger(&file.all_time)?;
        validate_savings_ledger(&file.session.ledger)?;
        Ok(())
    }
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    /// Serializes the tests that exec a freshly-written script. Writing then
    /// exec'ing a file while another thread forks can transiently fail with
    /// ETXTBSY ("text file busy"); holding this for each exec'ing test keeps the
    /// suite deterministic.
    static EXEC_LOCK: Mutex<()> = Mutex::new(());

    /// Write a throwaway executable standing in for `aden`: echo `stdout`, exit
    /// `code`. Lets us test the seam's exit-code and output handling hermetically.
    fn fake_aden(tag: &str, code: i32, stdout: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("coxn-fake-aden-{}-{tag}.sh", std::process::id()));
        std::fs::write(
            &path,
            format!("#!/bin/sh\nprintf '%s' '{stdout}'\nexit {code}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn gate_decodes_exit_code_and_captures_message() {
        let _serial = EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir();
        let manifest = dir.join("m.json");

        let leak = fake_aden("leak", 2, "gate: BLAST-LEAK");
        let out = gate_with(leak.to_str().unwrap(), &dir, &manifest);
        assert_eq!(out.verdict, GateVerdict::BlastLeak);
        assert_eq!(out.message, "gate: BLAST-LEAK");
        assert!(!out.verdict.proceed());

        let ok = fake_aden("ok", 0, "gate: in-scope");
        let out = gate_with(ok.to_str().unwrap(), &dir, &manifest);
        assert_eq!(out.verdict, GateVerdict::InScope);
        assert!(out.verdict.proceed());
    }

    #[test]
    fn extract_savings_pulls_the_all_time_detail() {
        let status = "Aden Status: .\n\
            Savings estimate (vs grep-and-read) [est.]:\n\
            \x20 This session: 3 aden calls → est. ~8 tool calls + ~2k tokens saved vs grep-and-read\n\
            \x20 All-time    : 40 aden calls → est. ~90 tool calls + ~30k tokens saved vs grep-and-read\n";
        let got = savings_detail_from_status(status)
            .map(|d| format_savings_status(&d))
            .expect("savings line present");
        assert!(got.starts_with("aden [est.]"), "{got}");
        assert!(got.contains("~90 tool calls"));
        assert!(got.contains("~30k tokens saved"));
        // No all-time line -> None.
        assert!(savings_detail_from_status("Aden Status: .\nno savings\n").is_none());
    }

    #[test]
    fn format_savings_chrome_shortens_detail() {
        let detail = "est. ~58 tool calls + ~481k tokens saved vs grep-and-read";
        let chrome = format_savings_chrome(detail);
        assert!(chrome.contains("aden [est.]"), "{chrome}");
        assert!(chrome.contains("481k"), "{chrome}");
        assert!(chrome.contains("~58 tools"), "{chrome}");
    }

    #[test]
    fn validate_savings_file_json_checks_ledger_math() {
        let ok = r#"{
            "schema": 2,
            "all_time": {
                "queries": 25,
                "returned_tokens": 99602,
                "baseline_tokens": 540690,
                "saved_tokens": 441088,
                "tool_calls_saved": 53
            },
            "session": {
                "started_unix": 1,
                "last_unix": 1,
                "queries": 0,
                "returned_tokens": 0,
                "baseline_tokens": 0,
                "saved_tokens": 0,
                "tool_calls_saved": 0
            }
        }"#;
        validate_savings_file_json(ok).expect("valid ledger");

        let bad = r#"{
            "schema": 2,
            "all_time": {
                "queries": 1,
                "returned_tokens": 100,
                "baseline_tokens": 500,
                "saved_tokens": 999,
                "tool_calls_saved": 1
            },
            "session": {}
        }"#;
        assert!(validate_savings_file_json(bad).is_err());
    }

    #[test]
    fn savings_detail_from_file_reads_repo_ledger() {
        let dir = Path::new(".");
        let detail = savings_detail_from_file(dir);
        if Path::new(".aden/savings.json").exists() {
            assert!(detail.is_some(), "savings.json present should yield detail");
            assert!(detail.unwrap().contains("tool calls"));
        }
    }

    fn validate_repo_savings_json_if_present() {
        let path = Path::new(".aden/savings.json");
        if !path.exists() {
            return;
        }
        let json = std::fs::read_to_string(path).expect("read savings.json");
        validate_savings_file_json(&json).expect("repo savings.json must be internally consistent");
    }

    #[test]
    fn gate_that_cannot_run_is_closed() {
        let _serial = EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let out = gate_with(
            "/nonexistent/coxn/aden",
            &std::env::temp_dir(),
            Path::new("m.json"),
        );
        assert!(matches!(out.verdict, GateVerdict::Error(_)));
        assert!(!out.verdict.proceed());
    }

    #[test]
    fn config_get_returns_value_or_none() {
        let _serial = EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir();

        let found = fake_aden("cfg-ok", 0, "ollama-model");
        assert_eq!(
            config_get_with(found.to_str().unwrap(), &dir, "model.name"),
            Some("ollama-model".to_string())
        );

        // Missing key: aden exits non-zero -> None.
        let missing = fake_aden("cfg-miss", 1, "");
        assert_eq!(
            config_get_with(missing.to_str().unwrap(), &dir, "model.name"),
            None
        );
    }

    #[test]
    fn text_commands_return_stdout_or_error() {
        let _serial = EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir();

        let good = fake_aden("scope-ok", 0, "{\"name\":\"t\"}");
        let json = scope_with(good.to_str().unwrap(), &dir, "t", &["seed".into()], 8192).unwrap();
        assert_eq!(json, "{\"name\":\"t\"}");

        let bad = fake_aden("scope-bad", 1, "");
        let err = pull_with(bad.to_str().unwrap(), &dir, Pull::Understand("x"));
        assert!(matches!(err, Err(AdenError::Failed { code: Some(1), .. })));
    }

    #[test]
    fn seeds_from_manifest_parses_seeds_array() {
        let _serial = EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir();
        let man = dir.join(format!("coxn-test-manifest-{}.json", std::process::id()));
        std::fs::write(&man, r#"{"name":"t","seeds":["foo","bar"]}"#).unwrap();
        let seeds = seeds_from_manifest(&man).expect("parses");
        assert_eq!(seeds, vec!["foo".to_string(), "bar".to_string()]);
        let _ = std::fs::remove_file(&man);
    }

    #[test]
    fn budget_from_manifest_reads_context_budget() {
        let _serial = EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir();
        let man = dir.join(format!(
            "coxn-test-budget-manifest-{}.json",
            std::process::id()
        ));
        std::fs::write(&man, r#"{"name":"t","context":{"budget":4096}}"#).unwrap();
        assert_eq!(budget_from_manifest(&man).expect("parses"), Some(4096));

        std::fs::write(&man, r#"{"name":"t","context":{}}"#).unwrap();
        assert_eq!(budget_from_manifest(&man).expect("parses"), None);
        let _ = std::fs::remove_file(&man);
    }

    #[test]
    fn files_from_manifest_reads_files_mandate() {
        let _serial = EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir();
        let man = dir.join(format!(
            "coxn-test-files-manifest-{}.json",
            std::process::id()
        ));
        std::fs::write(&man, r#"{"name":"t","files":["src/a.rs","src/b.rs"]}"#).unwrap();
        assert_eq!(
            files_from_manifest(&man).expect("parses"),
            vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
        );

        // No `files` key => empty mandate (never parallelizable).
        std::fs::write(&man, r#"{"name":"t"}"#).unwrap();
        assert_eq!(
            files_from_manifest(&man).expect("parses"),
            Vec::<String>::new()
        );
        let _ = std::fs::remove_file(&man);
    }
}
