//! Zero-config model discovery: logged-in CLIs and native Ollama before HTTP fallback.

use std::path::Path;

use crate::app::{ModelSel, claude_cli_model, codex_model, grok_cli_model, ollama_model};
use crate::claude_cli;
use crate::cli_ndjson;
use crate::codex_probe::{self, CodexProbeOutcome};
use crate::grok_cli;
use crate::model::AnyModel;
use crate::ollama;
use crate::openai;
use crate::provider::{self, ProviderDriver, ProviderInstance};

const OLLAMA_NATIVE_BASE: &str = "http://localhost:11434";

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

/// Probe installed, authenticated CLIs in priority order: grok → claude → codex.
pub fn detect_cli(dir: &Path) -> Option<(AnyModel, ModelSel)> {
    let cwd = std::env::current_dir().ok()?;
    try_grok_cli(&cwd)
        .or_else(|| try_claude_cli(&cwd))
        .or_else(|| try_codex_cli(dir, &cwd))
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

fn try_grok_cli(cwd: &Path) -> Option<(AnyModel, ModelSel)> {
    const BIN: &str = "grok";
    if !cli_ndjson::binary_installed(BIN) || !grok_cli::probe_logged_in(BIN, None, &[]) {
        return None;
    }
    let model = grok_cli::list_models(BIN, None, &[])
        .and_then(|models| models.into_iter().next())
        .unwrap_or_else(|| "grok-composer-2.5-fast".to_string());
    Some(grok_cli_model(
        "grok".to_string(),
        BIN.to_string(),
        model,
        None,
        Vec::new(),
        cwd.to_path_buf(),
        "auto-cli",
    ))
}

fn try_claude_cli(cwd: &Path) -> Option<(AnyModel, ModelSel)> {
    const BIN: &str = "claude";
    if !cli_ndjson::binary_installed(BIN) || !claude_cli::probe_logged_in(BIN, None, &[]) {
        return None;
    }
    let model = claude_cli::list_models(BIN, None, &[])
        .and_then(|models| models.into_iter().next())
        .unwrap_or_else(|| "claude-sonnet-4-6".to_string());
    Some(claude_cli_model(
        "claude".to_string(),
        BIN.to_string(),
        model,
        None,
        Vec::new(),
        cwd.to_path_buf(),
        "auto-cli",
    ))
}

fn try_codex_cli(dir: &Path, cwd: &Path) -> Option<(AnyModel, ModelSel)> {
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
    let codex_home = instance
        .shadow_home
        .clone()
        .or_else(|| instance.home_path.clone());
    let model = crate::codex_model::list_models(BIN, codex_home.as_deref(), &instance.env)
        .and_then(|models| models.into_iter().next())
        .unwrap_or_else(|| "gpt-5.4-mini".to_string());
    Some(codex_model(
        instance.id,
        BIN.to_string(),
        model,
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
