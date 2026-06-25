//! The aden seam: coxn shells out to the `aden` binary.
//!
//! coxn carries no intelligence; aden directs and gates. This is the thin
//! boundary that runs aden subcommands and reads their exit codes and text.
//! aden is a subprocess, not a linked crate, so coxn keeps its three-dependency
//! budget (DESIGN allows either; the dep rule forces this). The gate's
//! exit-code contract and text output (see docs/contract.adoc) are shaped for
//! exactly this.

// The seam is wired into the pump in P2.2 (pull-context tools) and P2.3 (gate);
// allow ahead-of-wiring use until then.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

use crate::gate::GateVerdict;

/// The aden binary to invoke. `COXN_ADEN_BIN` overrides it (e.g. to point at a
/// dev build or the offline branch); otherwise `aden` on PATH.
fn aden_bin() -> String {
    std::env::var("COXN_ADEN_BIN").unwrap_or_else(|_| "aden".to_string())
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

/// The gate outcome: the verdict coxn obeys plus aden's surfaced message.
#[derive(Debug)]
pub struct GateOutcome {
    pub verdict: GateVerdict,
    pub message: String,
}

/// What to pull from the graph on the model's behalf.
pub enum Pull<'a> {
    /// Assemble the neighborhood for an anchor (`aden asm --from`).
    Asm(&'a str),
    /// Definition + callers + downstream impact for a symbol (`aden understand`).
    Understand(&'a str),
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
    }
    run_text(cmd)
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
