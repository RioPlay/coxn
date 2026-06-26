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

fn gate_with(bin: &str, dir: &Path, manifest: &Path) -> GateOutcome {
    let mut cmd = Command::new(bin);
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
    cmd.arg("scope").arg(name);
    for s in seeds {
        cmd.arg("--seed").arg(s);
    }
    cmd.arg("--budget").arg(budget.to_string());
    cmd.arg("--json").arg(dir);
    run_text(cmd)
}

fn pull_with(bin: &str, dir: &Path, what: Pull) -> Result<String, AdenError> {
    let mut cmd = Command::new(bin);
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

/// A compact savings summary for the status line, from `aden status`. `None`
/// when aden cannot run, the command fails, or no savings are recorded.
pub fn savings(dir: &Path) -> Option<String> {
    let out = Command::new(aden_bin())
        .arg("status")
        .arg(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    extract_savings(&String::from_utf8_lossy(&out.stdout))
}

/// Pull the all-time savings detail out of `aden status` text. Pure for testing.
fn extract_savings(status: &str) -> Option<String> {
    let line = status
        .lines()
        .find(|l| l.trim_start().starts_with("All-time"))?;
    // "...All-time : N aden calls → est. ~X tool calls + ~Y tokens saved vs ..."
    let detail = line.split('→').nth(1).unwrap_or(line).trim();
    (!detail.is_empty()).then(|| format!("aden {detail}"))
}

/// Read a runtime preference from `.aden/config.toml` via `aden config get`.
/// `None` when the key is unset, aden cannot run, or the value is empty. Lets
/// coxn pin a provider/model without environment variables.
pub fn config_get(dir: &Path, key: &str) -> Option<String> {
    config_get_with(&aden_bin(), dir, key)
}

fn config_get_with(bin: &str, dir: &Path, key: &str) -> Option<String> {
    let out = Command::new(bin)
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
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
        let got = extract_savings(status).expect("savings line present");
        assert!(got.starts_with("aden est."), "{got}");
        assert!(got.contains("~90 tool calls"));
        assert!(got.contains("~30k tokens saved"));
        // No all-time line -> None.
        assert!(extract_savings("Aden Status: .\nno savings\n").is_none());
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
}
