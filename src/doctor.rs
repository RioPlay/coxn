//! `coxn doctor`: one-screen health check for model, sandbox, aden, and gate preconditions.

use std::path::Path;

use crate::aden;
use crate::openai;
use crate::sandbox;

/// Run all checks, print a human-readable report to stdout, return exit code
/// (0 = ready to code, 1 = blocking issue, 2 = warnings only).
pub fn run(dir: &Path) -> i32 {
    let mut blocking = false;
    let mut warnings = false;

    println!("coxn doctor");
    println!("project: {}", dir.display());
    println!();

    // --- Model ---
    let has_env = std::env::var("COXN_MODEL_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .is_some();
    let detected = openai::detect();

    if has_env {
        let name = std::env::var("COXN_MODEL_NAME").unwrap_or_else(|_| "local".into());
        let base = std::env::var("COXN_MODEL_BASE_URL").unwrap_or_default();
        let key = std::env::var("COXN_MODEL_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .is_some();
        println!("✓ model: {name} @ {base} (env){}", if key { "" } else { " — no COXN_MODEL_KEY" });
        if !key && !base.contains("localhost") && !base.contains("127.0.0.1") {
            warnings = true;
            println!("  warn: cloud endpoint without COXN_MODEL_KEY may 401");
        }
    } else if let Some((base, model)) = detected {
        println!("✓ model: {model} @ {base} (auto-detect)");
    } else {
        blocking = true;
        println!("✗ model: OFFLINE STUB — no endpoint reachable");
        println!("  fix: start Ollama/LM Studio, or set COXN_MODEL_BASE_URL");
    }

    // --- Sandbox ---
    if sandbox::bwrap_available() {
        println!("✓ sandbox: bwrap (namespaced confinement)");
    } else {
        warnings = true;
        println!("⚠ sandbox: NO SANDBOX — approval gate only");
        println!("  fix: install bubblewrap (bwrap) for FS/network isolation");
    }

    // --- aden (optional) ---
    let caps = aden::probe(dir);
    if caps.available {
        println!("✓ aden: on PATH");
        if let (Some(url), Some(name)) = (&caps.model_base_url, &caps.model_name) {
            println!("  config: {name} @ {url}");
        }
    } else {
        println!("○ aden: not on PATH (optional — standalone mode)");
    }

    // --- Task scope / dirty tree ---
    let task = std::env::var("COXN_TASK_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty());
    if let Some(name) = task {
        println!("○ task: {name} (blast-radius gate active when aden + seeds set)");
        if git_dirty(dir) {
            warnings = true;
            println!("  warn: dirty git tree — impact-diff judges whole diff vs HEAD");
            println!("  fix: commit or stash before scoped edits");
        }
    } else {
        println!("○ task: none (ungated — human approval only)");
    }

    // --- Sessions dir ---
    let sessions = session_dir();
    println!("○ sessions: {}", sessions.display());

    println!();
    if blocking {
        println!("status: NOT READY (blocking)");
        1
    } else if warnings {
        println!("status: READY WITH WARNINGS");
        2
    } else {
        println!("status: READY");
        0
    }
}

pub(crate) fn git_dirty(dir: &Path) -> bool {
    std::process::Command::new("git")
        .args(["-C", &dir.display().to_string(), "status", "--porcelain"])
        .output()
        .ok()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

pub(crate) fn session_dir() -> std::path::PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".local/share"))
                .unwrap_or_else(|_| std::path::PathBuf::from(".local/share"))
        })
        .join("coxn/sessions")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_runs_without_panic() {
        let dir = std::env::current_dir().expect("cwd");
        // Exit code depends on environment; just ensure it returns 0/1/2.
        let code = run(&dir);
        assert!(code <= 2);
    }
}