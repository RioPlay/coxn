//! Zero-config model discovery: logged-in CLIs and native Ollama before HTTP fallback.

use std::collections::HashSet;
use std::path::Path;

use crate::app::{
    ModelSel, claude_cli_model, codex_model, grok_cli_model, ollama_model, openai_model,
    resolve_instance_from_config,
};
use crate::claude_cli;
use crate::cli_ndjson;
use crate::codex_probe::{self, CodexProbeOutcome};
use crate::grok_cli;
use crate::model::AnyModel;
use crate::ollama;
use crate::openai;
use crate::provider::{self, ModelSelection, ProviderDriver, ProviderInstance};

const OLLAMA_NATIVE_BASE: &str = "http://localhost:11434";

/// A signed-in or reachable backend the user can hot-swap to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadyBackend {
    pub instance_id: String,
    pub display_name: String,
    pub model: String,
    pub text_only: bool,
}

/// True when the active backend is a CLI piggyback (no tool calling in coxn).
pub fn is_text_only_piggyback(sel: &ModelSel) -> bool {
    let Some(endpoint) = sel.endpoint.as_ref() else {
        return false;
    };
    endpoint
        .base_url
        .starts_with(crate::codex_model::CODEX_ENDPOINT_SCHEME)
        || endpoint.base_url.starts_with(claude_cli::CLAUDE_CLI_SCHEME)
        || endpoint.base_url.starts_with(grok_cli::GROK_CLI_SCHEME)
}

/// Append a chrome/status suffix when tools are disabled on this backend.
pub fn model_display_label(sel: &ModelSel, usage: Option<crate::model::Usage>) -> String {
    let mut label = sel.label();
    if let Some(u) = usage.filter(|u| u.prompt_tokens > 0) {
        let ctx = if u.prompt_tokens >= 1000 {
            format!("~{:.1}k ctx", u.prompt_tokens as f64 / 1000.0)
        } else {
            format!("~{} ctx", u.prompt_tokens)
        };
        label = format!("{label} {ctx}");
    }
    if is_text_only_piggyback(sel) {
        label.push_str(" [text-only]");
    }
    label
}

/// Every backend that is authenticated or reachable right now (config + auto-detect).
pub fn list_ready_backends(dir: &Path) -> Vec<ReadyBackend> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let cfg = provider::load_config(dir);

    for instance in &cfg.instances {
        if !instance.enabled {
            continue;
        }
        if let Some(backend) = ready_from_instance(instance) {
            seen.insert(backend.instance_id.clone());
            out.push(backend);
        }
    }

    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return out,
    };

    for (id, build) in [
        ("grok", try_grok_backend(&cwd, None)),
        ("claude", try_claude_backend(&cwd, None)),
        ("codex", try_codex_backend(dir, &cwd, None)),
    ] {
        if !seen.contains(id) {
            if let Some(backend) = build {
                seen.insert(id.to_string());
                out.push(backend);
            }
        }
    }

    if !seen.contains("local") && ollama::reachable(OLLAMA_NATIVE_BASE) {
        let model = openai::list_models(&format!("{OLLAMA_NATIVE_BASE}/v1"), None)
            .and_then(|models| models.into_iter().next())
            .unwrap_or_else(|| "qwen2.5-coder".to_string());
        out.push(ReadyBackend {
            instance_id: "local".to_string(),
            display_name: "ollama (native)".to_string(),
            model,
            text_only: false,
        });
    }

    if let Some((base, model)) = openai::detect() {
        let id = if base.contains(":11434") {
            "local"
        } else if base.contains(":1234") {
            "lmstudio"
        } else {
            "auto-http"
        };
        if !seen.contains(id) {
            out.push(ReadyBackend {
                instance_id: id.to_string(),
                display_name: format!("auto-detect ({base})"),
                model,
                text_only: false,
            });
        }
    }

    out
}

/// Resolve `instance:model` from config or auto-detected CLIs/Ollama.
pub fn resolve_backend_selection(
    dir: &Path,
    selection: ModelSelection,
) -> Option<(AnyModel, ModelSel)> {
    let cfg = provider::load_config(dir);
    if let Some(resolved) = resolve_instance_from_config(&cfg, selection.clone(), "switch") {
        return Some(resolved);
    }
    let cwd = std::env::current_dir().ok()?;
    match selection.instance_id.as_str() {
        "grok" => try_grok_cli(&cwd, Some(&selection.model)),
        "claude" => try_claude_cli(&cwd, Some(&selection.model)),
        "codex" => try_codex_cli(dir, &cwd, Some(&selection.model)),
        "local" if ollama::reachable(OLLAMA_NATIVE_BASE) => Some(ollama_model(
            "local".to_string(),
            OLLAMA_NATIVE_BASE.to_string(),
            selection.model,
            None,
            "switch",
        )),
        "lmstudio" | "auto-http" => {
            let (base, _) = openai::detect()?;
            Some(openai_model(
                selection.instance_id,
                base,
                selection.model,
                None,
                "switch",
            ))
        }
        _ => None,
    }
}

/// Probe installed, authenticated CLIs in priority order: grok → claude → codex.
pub fn detect_cli(dir: &Path) -> Option<(AnyModel, ModelSel)> {
    let cwd = std::env::current_dir().ok()?;
    try_grok_cli(&cwd, None)
        .or_else(|| try_claude_cli(&cwd, None))
        .or_else(|| try_codex_cli(dir, &cwd, None))
}

/// Prefer native Ollama (`/api/chat`) when the daemon is up — full streaming + tools.
pub fn detect_ollama_native() -> Option<(AnyModel, ModelSel)> {
    if !ollama::reachable(OLLAMA_NATIVE_BASE) {
        return None;
    }
    let model = openai::list_models(&format!("{OLLAMA_NATIVE_BASE}/v1"), None)
        .and_then(|models| models.into_iter().next())
        .unwrap_or_else(|| "qwen2.5-coder".to_string());
    Some(ollama_model(
        "local".to_string(),
        OLLAMA_NATIVE_BASE.to_string(),
        model,
        None,
        "auto-ollama",
    ))
}

/// True when a provider instance uses a text-only CLI piggyback driver.
pub fn instance_is_text_only(driver: &ProviderDriver) -> bool {
    matches!(
        driver,
        ProviderDriver::Codex | ProviderDriver::ClaudeCli | ProviderDriver::GrokCli
    )
}

/// True when a configured `instance:model` selection resolves to a text-only backend.
pub fn selection_is_text_only(
    cfg: &provider::ProviderConfig,
    selection: &provider::ModelSelection,
) -> bool {
    cfg.instance(&selection.instance_id)
        .is_some_and(|i| i.enabled && instance_is_text_only(&i.driver))
}

/// Whether a configured CLI instance is installed and authenticated.
pub fn cli_instance_ready(instance: &ProviderInstance) -> bool {
    match instance.driver {
        ProviderDriver::GrokCli => {
            let bin = instance.binary.as_deref().unwrap_or("grok");
            cli_ndjson::binary_installed(bin)
                && grok_cli::probe_logged_in(bin, instance.home_path.as_deref(), &instance.env)
        }
        ProviderDriver::ClaudeCli => {
            let bin = instance.binary.as_deref().unwrap_or("claude");
            cli_ndjson::binary_installed(bin)
                && claude_cli::probe_logged_in(bin, instance.home_path.as_deref(), &instance.env)
        }
        ProviderDriver::Codex => {
            let bin = instance.binary.as_deref().unwrap_or("codex");
            cli_ndjson::binary_installed(bin)
                && matches!(
                    codex_probe::probe_instance(instance),
                    CodexProbeOutcome::Authenticated(_)
                )
        }
        _ => true,
    }
}

fn ready_from_instance(instance: &ProviderInstance) -> Option<ReadyBackend> {
    let display_name = instance
        .display_name
        .clone()
        .unwrap_or_else(|| instance.id.clone());
    match instance.driver {
        ProviderDriver::OpenAiCompat => {
            let base = instance.base_url.as_deref()?;
            let key = provider::secret_for_instance(&instance.id);
            if provider::cloud_blocked(base, key.as_deref()) {
                return None;
            }
            let models = openai::list_models(base, key.as_deref())?;
            let model = models.into_iter().next()?;
            Some(ReadyBackend {
                instance_id: instance.id.clone(),
                display_name,
                model,
                text_only: false,
            })
        }
        ProviderDriver::Ollama => {
            let base = instance.base_url.as_deref().unwrap_or(OLLAMA_NATIVE_BASE);
            if !ollama::reachable(base) {
                return None;
            }
            let model = openai::list_models(&format!("{base}/v1"), None)
                .and_then(|models| models.into_iter().next())
                .unwrap_or_else(|| "qwen2.5-coder".to_string());
            Some(ReadyBackend {
                instance_id: instance.id.clone(),
                display_name,
                model,
                text_only: false,
            })
        }
        ProviderDriver::Stub => Some(ReadyBackend {
            instance_id: instance.id.clone(),
            display_name,
            model: "stub".to_string(),
            text_only: false,
        }),
        ProviderDriver::Codex | ProviderDriver::ClaudeCli | ProviderDriver::GrokCli => {
            if !cli_instance_ready(instance) {
                return None;
            }
            let model = default_cli_model(instance)?;
            Some(ReadyBackend {
                instance_id: instance.id.clone(),
                display_name,
                model,
                text_only: true,
            })
        }
        ProviderDriver::Unknown(_) => None,
    }
}

fn default_cli_model(instance: &ProviderInstance) -> Option<String> {
    match instance.driver {
        ProviderDriver::GrokCli => {
            let bin = instance.binary.as_deref().unwrap_or("grok");
            grok_cli::list_models(bin, instance.home_path.as_deref(), &instance.env)
                .and_then(|models| models.into_iter().next())
                .or(Some("grok-composer-2.5-fast".to_string()))
        }
        ProviderDriver::ClaudeCli => {
            let bin = instance.binary.as_deref().unwrap_or("claude");
            claude_cli::list_models(bin, instance.home_path.as_deref(), &instance.env)
                .and_then(|models| models.into_iter().next())
                .or(Some("claude-sonnet-4-6".to_string()))
        }
        ProviderDriver::Codex => {
            let bin = instance.binary.as_deref().unwrap_or("codex");
            let home = instance
                .shadow_home
                .as_deref()
                .or(instance.home_path.as_deref());
            crate::codex_model::list_models(bin, home, &instance.env)
                .and_then(|models| models.into_iter().next())
                .or(Some("gpt-5.4-mini".to_string()))
        }
        _ => None,
    }
}

fn try_grok_backend(_cwd: &Path, model: Option<&str>) -> Option<ReadyBackend> {
    const BIN: &str = "grok";
    if !cli_ndjson::binary_installed(BIN) || !grok_cli::probe_logged_in(BIN, None, &[]) {
        return None;
    }
    let model = model
        .map(str::to_string)
        .or_else(|| {
            grok_cli::list_models(BIN, None, &[]).and_then(|models| models.into_iter().next())
        })
        .unwrap_or_else(|| "grok-composer-2.5-fast".to_string());
    Some(ReadyBackend {
        instance_id: "grok".to_string(),
        display_name: "grok CLI".to_string(),
        model,
        text_only: true,
    })
}

fn try_claude_backend(cwd: &Path, model: Option<&str>) -> Option<ReadyBackend> {
    let _ = cwd;
    const BIN: &str = "claude";
    if !cli_ndjson::binary_installed(BIN) || !claude_cli::probe_logged_in(BIN, None, &[]) {
        return None;
    }
    let model = model
        .map(str::to_string)
        .or_else(|| {
            claude_cli::list_models(BIN, None, &[]).and_then(|models| models.into_iter().next())
        })
        .unwrap_or_else(|| "claude-sonnet-4-6".to_string());
    Some(ReadyBackend {
        instance_id: "claude".to_string(),
        display_name: "claude CLI".to_string(),
        model,
        text_only: true,
    })
}

fn try_codex_backend(dir: &Path, cwd: &Path, model: Option<&str>) -> Option<ReadyBackend> {
    let _ = cwd;
    const BIN: &str = "codex";
    if !cli_ndjson::binary_installed(BIN) {
        return None;
    }
    let cfg = provider::load_config(dir);
    let instance = cfg
        .instance("codex")
        .cloned()
        .unwrap_or_else(|| ProviderInstance::for_probe("codex", ProviderDriver::Codex, BIN));
    if !matches!(
        codex_probe::probe_instance(&instance),
        CodexProbeOutcome::Authenticated(_)
    ) {
        return None;
    }
    let model = model
        .map(str::to_string)
        .or_else(|| default_cli_model(&instance))
        .unwrap_or_else(|| "gpt-5.4-mini".to_string());
    Some(ReadyBackend {
        instance_id: instance.id,
        display_name: "codex CLI".to_string(),
        model,
        text_only: true,
    })
}

fn try_grok_cli(cwd: &Path, model: Option<&str>) -> Option<(AnyModel, ModelSel)> {
    let backend = try_grok_backend(cwd, model)?;
    Some(grok_cli_model(
        backend.instance_id,
        "grok".to_string(),
        backend.model,
        None,
        Vec::new(),
        cwd.to_path_buf(),
        "auto-cli",
    ))
}

fn try_claude_cli(cwd: &Path, model: Option<&str>) -> Option<(AnyModel, ModelSel)> {
    let backend = try_claude_backend(cwd, model)?;
    Some(claude_cli_model(
        backend.instance_id,
        "claude".to_string(),
        backend.model,
        None,
        Vec::new(),
        cwd.to_path_buf(),
        "auto-cli",
    ))
}

fn try_codex_cli(dir: &Path, cwd: &Path, model: Option<&str>) -> Option<(AnyModel, ModelSel)> {
    let backend = try_codex_backend(dir, cwd, model)?;
    let cfg = provider::load_config(dir);
    let instance = cfg
        .instance(&backend.instance_id)
        .cloned()
        .unwrap_or_else(|| ProviderInstance::for_probe("codex", ProviderDriver::Codex, "codex"));
    let codex_home = instance
        .shadow_home
        .clone()
        .or_else(|| instance.home_path.clone());
    Some(codex_model(
        backend.instance_id,
        instance.binary.unwrap_or_else(|| "codex".to_string()),
        backend.model,
        codex_home,
        instance.env,
        cwd.to_path_buf(),
        "auto-cli",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Endpoint;

    #[test]
    fn text_only_piggyback_detects_cli_schemes() {
        let sel = ModelSel {
            name: "m".to_string(),
            endpoint: Some(Endpoint {
                instance_id: "grok".to_string(),
                base_url: format!("{}grok", grok_cli::GROK_CLI_SCHEME),
                key: None,
                source: "test".to_string(),
            }),
        };
        assert!(is_text_only_piggyback(&sel));
        let openai = ModelSel {
            name: "m".to_string(),
            endpoint: Some(Endpoint {
                instance_id: "local".to_string(),
                base_url: "http://localhost:11434/v1".to_string(),
                key: None,
                source: "test".to_string(),
            }),
        };
        assert!(!is_text_only_piggyback(&openai));
    }

    #[test]
    fn list_ready_backends_empty_without_config_or_clis() {
        let dir = std::env::temp_dir().join(format!("coxn-discover-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Without config or live CLIs this may be empty or contain auto-detect only.
        let _backends = list_ready_backends(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selection_is_text_only_for_cli_drivers() {
        let mut cfg = provider::ProviderConfig::default();
        cfg.instances.push(ProviderInstance::for_probe(
            "grok",
            ProviderDriver::GrokCli,
            "grok",
        ));
        let sel = provider::ModelSelection {
            instance_id: "grok".into(),
            model: "m".into(),
        };
        assert!(selection_is_text_only(&cfg, &sel));
    }
}
