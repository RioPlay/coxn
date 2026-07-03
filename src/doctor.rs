//! `coxn doctor`: one-screen health check for model, sandbox, aden, and gate preconditions.

use std::path::Path;

use crate::aden;
use crate::codex_probe;
use crate::openai;
use crate::provider;
use crate::sandbox;
use crate::trust;

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
        println!(
            "✓ model: {name} @ {base} (env){}",
            if key { "" } else { " — no COXN_MODEL_KEY" }
        );
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

    let provider_cfg = provider::load_config(dir);
    if !provider_cfg.instances.is_empty() {
        println!();
        println!("providers:");
        for instance in &provider_cfg.instances {
            if !instance.enabled {
                println!("○ {}: disabled", instance.id);
                continue;
            }
            match &instance.driver {
                provider::ProviderDriver::OpenAiCompat => {
                    let base = instance.base_url.as_deref().unwrap_or("(missing base_url)");
                    let key = provider::secret_for_instance(&instance.id);
                    if provider::cloud_blocked(base, key.as_deref()) {
                        blocking = true;
                        println!(
                            "✗ {}: {} requires COXN_KEY_{} or COXN_ALLOW_CLOUD=1",
                            instance.id,
                            base,
                            provider::secret_env_name(&instance.id).trim_start_matches("COXN_KEY_")
                        );
                    } else {
                        let auth = if key.is_some() { "key" } else { "no key" };
                        println!("✓ {}: {} ({auth})", instance.id, base);
                    }
                }
                provider::ProviderDriver::Stub => println!("✓ {}: stub", instance.id),
                provider::ProviderDriver::Ollama => {
                    let base = instance
                        .base_url
                        .as_deref()
                        .unwrap_or("http://localhost:11434");
                    if crate::ollama::reachable(base) {
                        println!("✓ {}: ollama native @ {base} (no key)", instance.id);
                    } else {
                        blocking = true;
                        println!("✗ {}: ollama not reachable @ {base}", instance.id);
                    }
                }
                provider::ProviderDriver::Codex => {
                    let bin = instance.binary.as_deref().unwrap_or("codex");
                    let outcome = codex_probe::probe_instance(instance);
                    let (is_blocking, line) =
                        codex_probe::format_status_line(&instance.id, bin, &outcome);
                    if is_blocking {
                        blocking = true;
                    }
                    println!("{line}");
                    if matches!(outcome, codex_probe::CodexProbeOutcome::Authenticated(_)) {
                        println!("  exec: codex CLI piggyback (text-only turns)");
                    }
                }
                provider::ProviderDriver::ClaudeCli => {
                    let bin = instance.binary.as_deref().unwrap_or("claude");
                    if codex_probe::binary_installed(bin) {
                        println!("○ {}: {bin} installed (auth probe deferred)", instance.id);
                    } else {
                        blocking = true;
                        println!("✗ {}: {bin} not installed or not runnable", instance.id);
                    }
                }
                provider::ProviderDriver::Unknown(driver) => {
                    blocking = true;
                    println!("✗ {}: unknown driver '{}'", instance.id, driver)
                }
            }
        }
    }

    if trust::auto_approve_enabled() {
        warnings = true;
        println!("⚠ auto-approve: COXN_AUTO_APPROVE=1 — human gate disabled");
        println!("  note: intended for `coxn once` headless runs only");
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
