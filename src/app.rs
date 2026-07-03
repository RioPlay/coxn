//! App startup and high-level session wiring.
//!
//! The model-selection core (provider resolution, the `ModelSel`/`Endpoint`
//! types, the OpenAI-compat and native Ollama constructors, role routing, the
//! task-env config, and the agent preambles) lives here so `main.rs` can stay
//! focused on CLI routing; the TUI drive loop lives in `drive.rs`. `execute.rs`
//! imports these
//! directly from this module; `main.rs` re-exports them so its existing call
//! sites are unchanged.
//!
//! Behaviour-wise this is a pure structural extract -- nothing here changed when
//! it moved out of `main.rs`.

use std::path::Path;

use std::path::PathBuf;

use crate::claude_cli::{self, CLAUDE_CLI_SCHEME, ClaudeCliPiggybackModel};
use crate::codex_model::{self, CODEX_ENDPOINT_SCHEME, CodexPiggybackModel};
use crate::grok_cli::{self, GROK_CLI_SCHEME, GrokCliPiggybackModel};
use crate::model::{AnyModel, StubModel};
use crate::{aden, ollama, openai, provider};

/// The live provider connection, kept so `/model` can enumerate and switch
/// models at runtime without re-resolving. The stub has no endpoint.
pub(crate) struct Endpoint {
    pub(crate) instance_id: String,
    pub(crate) base_url: String,
    pub(crate) key: Option<String>,
    pub(crate) source: String,
}

/// The active model selection: which model, and (for a real provider) where it
/// lives. Selection is data, so switching is just rebuilding the backend.
pub(crate) struct ModelSel {
    pub(crate) name: String,
    pub(crate) endpoint: Option<Endpoint>,
}

impl ModelSel {
    /// True when no real provider is configured (offline stub backend).
    pub(crate) fn is_offline_stub(&self) -> bool {
        self.endpoint.is_none()
    }

    /// The status-line label tagging the model and how it was resolved.
    pub(crate) fn label(&self) -> String {
        match &self.endpoint {
            Some(e) => format!(
                "{} @ {} ({}/{})",
                self.name, e.base_url, e.instance_id, e.source
            ),
            None => {
                "stub (no model; start Ollama/LM Studio or set COXN_MODEL_BASE_URL)".to_string()
            }
        }
    }
}

/// Build an OpenAI-compatible model paired with its selection state.
pub(crate) fn openai_model(
    instance_id: String,
    base_url: String,
    model: String,
    key: Option<String>,
    source: impl Into<String>,
) -> (AnyModel, ModelSel) {
    let backend = AnyModel::OpenAiCompat(openai::OpenAiCompatModel::new(
        base_url.clone(),
        model.clone(),
        key.clone(),
    ));
    (
        backend,
        ModelSel {
            name: model,
            endpoint: Some(Endpoint {
                instance_id,
                base_url,
                key,
                source: source.into(),
            }),
        },
    )
}

/// Build a Claude Code CLI piggyback model paired with its selection state.
pub(crate) fn claude_cli_model(
    instance_id: String,
    binary: String,
    model: String,
    home_path: Option<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
    source: impl Into<String>,
) -> (AnyModel, ModelSel) {
    let endpoint = format!("{CLAUDE_CLI_SCHEME}{binary}");
    let backend = AnyModel::ClaudeCliPiggyback(ClaudeCliPiggybackModel::new(
        binary.clone(),
        model.clone(),
        home_path,
        env,
        cwd,
    ));
    (
        backend,
        ModelSel {
            name: model,
            endpoint: Some(Endpoint {
                instance_id,
                base_url: endpoint,
                key: None,
                source: source.into(),
            }),
        },
    )
}

/// Build a Grok Build CLI piggyback model paired with its selection state.
pub(crate) fn grok_cli_model(
    instance_id: String,
    binary: String,
    model: String,
    home_path: Option<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
    source: impl Into<String>,
) -> (AnyModel, ModelSel) {
    let endpoint = format!("{GROK_CLI_SCHEME}{binary}");
    let backend = AnyModel::GrokCliPiggyback(GrokCliPiggybackModel::new(
        binary.clone(),
        model.clone(),
        home_path,
        env,
        cwd,
    ));
    (
        backend,
        ModelSel {
            name: model,
            endpoint: Some(Endpoint {
                instance_id,
                base_url: endpoint,
                key: None,
                source: source.into(),
            }),
        },
    )
}

/// Build a Codex CLI piggyback model paired with its selection state.
pub(crate) fn codex_model(
    instance_id: String,
    binary: String,
    model: String,
    codex_home: Option<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
    source: impl Into<String>,
) -> (AnyModel, ModelSel) {
    let endpoint = format!("{CODEX_ENDPOINT_SCHEME}{binary}");
    let backend = AnyModel::CodexPiggyback(CodexPiggybackModel::new(
        binary.clone(),
        model.clone(),
        codex_home,
        env,
        cwd,
    ));
    (
        backend,
        ModelSel {
            name: model,
            endpoint: Some(Endpoint {
                instance_id,
                base_url: endpoint,
                key: None,
                source: source.into(),
            }),
        },
    )
}

/// Build a native Ollama (`/api/chat`, NDJSON streaming) model paired with its
/// selection state. Ollama uses no API key (it is local); `key` is captured for
/// the `Endpoint` but never sent. The base URL is typically
/// `http://localhost:11434` (no `/v1`).
pub(crate) fn ollama_model(
    instance_id: String,
    base_url: String,
    model: String,
    key: Option<String>,
    source: impl Into<String>,
) -> (AnyModel, ModelSel) {
    let backend = AnyModel::Ollama(ollama::OllamaModel::new(base_url.clone(), model.clone()));
    (
        backend,
        ModelSel {
            name: model,
            endpoint: Some(Endpoint {
                instance_id,
                base_url,
                key,
                source: source.into(),
            }),
        },
    )
}

/// Resolve a provider instance `selection` (`instance:model`) into a concrete
/// backend + selection. `None` when the instance is disabled, unknown, or
/// blocked by the cloud wall without a key. The driver dispatch is the single
/// place that knows how each `[provider.*]` driver turns into an `AnyModel`.
pub(crate) fn resolve_instance_from_config(
    cfg: &provider::ProviderConfig,
    selection: provider::ModelSelection,
    source: &str,
) -> Option<(AnyModel, ModelSel)> {
    let instance = cfg.instance(&selection.instance_id)?;
    if !instance.enabled {
        return None;
    }
    match instance.driver {
        provider::ProviderDriver::OpenAiCompat => {
            let base_url = instance.base_url.clone()?;
            let key = provider::secret_for_instance(&instance.id);
            if provider::cloud_blocked(&base_url, key.as_deref()) {
                return None;
            }
            Some(openai_model(
                instance.id.clone(),
                base_url,
                selection.model,
                key,
                source,
            ))
        }
        provider::ProviderDriver::Stub => Some((
            AnyModel::Stub(StubModel),
            ModelSel {
                name: selection.model,
                endpoint: None,
            },
        )),
        provider::ProviderDriver::Ollama => {
            // Ollama is local-first; a missing base_url falls back to the
            // default Ollama port. Cloud-gating does not apply (Ollama is
            // local-only in practice), but we still refuse a cloud-ish URL
            // carrying no key for consistency with the cloud wall.
            let base_url = instance
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            let key = provider::secret_for_instance(&instance.id);
            if provider::cloud_blocked(&base_url, key.as_deref()) {
                return None;
            }
            Some(ollama_model(
                instance.id.clone(),
                base_url,
                selection.model,
                key,
                source,
            ))
        }
        provider::ProviderDriver::Codex => {
            let binary = instance
                .binary
                .clone()
                .unwrap_or_else(|| "codex".to_string());
            if !crate::discover::cli_instance_ready(instance) {
                return None;
            }
            let cwd = std::env::current_dir().ok()?;
            let codex_home = instance
                .shadow_home
                .clone()
                .or_else(|| instance.home_path.clone());
            Some(codex_model(
                instance.id.clone(),
                binary,
                selection.model,
                codex_home,
                instance.env.clone(),
                cwd,
                source,
            ))
        }
        provider::ProviderDriver::ClaudeCli => {
            let binary = instance
                .binary
                .clone()
                .unwrap_or_else(|| "claude".to_string());
            if !crate::discover::cli_instance_ready(instance) {
                return None;
            }
            let cwd = std::env::current_dir().ok()?;
            Some(claude_cli_model(
                instance.id.clone(),
                binary,
                selection.model,
                instance.home_path.clone(),
                instance.env.clone(),
                cwd,
                source,
            ))
        }
        provider::ProviderDriver::GrokCli => {
            let binary = instance
                .binary
                .clone()
                .unwrap_or_else(|| "grok".to_string());
            if !crate::discover::cli_instance_ready(instance) {
                return None;
            }
            let cwd = std::env::current_dir().ok()?;
            Some(grok_cli_model(
                instance.id.clone(),
                binary,
                selection.model,
                instance.home_path.clone(),
                instance.env.clone(),
                cwd,
                source,
            ))
        }
        provider::ProviderDriver::Unknown(_) => None,
    }
}

/// Rebuild the active Codex backend after `/model` switches model name.
pub(crate) fn rebuild_claude_cli_model(
    dir: &Path,
    sel: &ModelSel,
    model_name: String,
) -> Option<AnyModel> {
    let endpoint = sel.endpoint.as_ref()?;
    let binary = claude_cli::binary_from_endpoint(&endpoint.base_url)?;
    let cfg = provider::load_config(dir);
    let instance = cfg.instance(&endpoint.instance_id)?;
    if !matches!(instance.driver, provider::ProviderDriver::ClaudeCli) {
        return None;
    }
    Some(AnyModel::ClaudeCliPiggyback(ClaudeCliPiggybackModel::new(
        binary.to_string(),
        model_name,
        instance.home_path.clone(),
        instance.env.clone(),
        dir,
    )))
}

pub(crate) fn rebuild_grok_cli_model(
    dir: &Path,
    sel: &ModelSel,
    model_name: String,
) -> Option<AnyModel> {
    let endpoint = sel.endpoint.as_ref()?;
    let binary = grok_cli::binary_from_endpoint(&endpoint.base_url)?;
    let cfg = provider::load_config(dir);
    let instance = cfg.instance(&endpoint.instance_id)?;
    if !matches!(instance.driver, provider::ProviderDriver::GrokCli) {
        return None;
    }
    Some(AnyModel::GrokCliPiggyback(GrokCliPiggybackModel::new(
        binary.to_string(),
        model_name,
        instance.home_path.clone(),
        instance.env.clone(),
        dir,
    )))
}

pub(crate) fn rebuild_codex_model(
    dir: &Path,
    sel: &ModelSel,
    model_name: String,
) -> Option<AnyModel> {
    let endpoint = sel.endpoint.as_ref()?;
    let binary = codex_model::codex_binary_from_endpoint(&endpoint.base_url)?;
    let cfg = provider::load_config(dir);
    let instance = cfg.instance(&endpoint.instance_id)?;
    if !matches!(instance.driver, provider::ProviderDriver::Codex) {
        return None;
    }
    let codex_home = instance
        .shadow_home
        .clone()
        .or_else(|| instance.home_path.clone());
    Some(AnyModel::CodexPiggyback(CodexPiggybackModel::new(
        binary.to_string(),
        model_name,
        codex_home,
        instance.env.clone(),
        dir,
    )))
}

/// Resolve a role to an `instance:model` selection via `[route]`.
pub(crate) fn resolve_role(
    dir: &Path,
    caps: &aden::AdenCaps,
    role: &str,
) -> Option<provider::ModelSelection> {
    let cfg = provider::load_config(dir);
    if let Some(sel) = cfg.route(role).or_else(|| cfg.route("active")) {
        return Some(sel);
    }
    if caps.available {
        aden::config_get(dir, &format!("route.{role}"))
            .and_then(|value| provider::split_selection(&value))
    } else {
        None
    }
}

/// Read the task env (`COXN_TASK_NAME` + `COXN_TASK_SEEDS` + `COXN_TASK_BUDGET`).
/// `None` (no active task) when the name is unset/empty.
pub(crate) fn task_config() -> Option<(String, Vec<String>, u64)> {
    let name = std::env::var("COXN_TASK_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let seeds = std::env::var("COXN_TASK_SEEDS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let budget = std::env::var("COXN_TASK_BUDGET")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8192);
    Some((name, seeds, budget))
}

/// The base agent preamble layered under every system prompt that aden scopes.
/// coxn never injects provider-specific nudges here -- this is the neutral
/// "how to act through tools" floor.
pub(crate) const AGENT_PREAMBLE_BASE: &str = "\
You are coxn, a coding agent. To change code, call `read_file` to get the exact \
current text, then `edit` (replace an exact unique string) or `write_file` (whole \
file) -- do not print a patch for the user to apply. To build, test, run, or use \
git, call `run_command`: it runs in a sandbox confined to the project, with no \
network unless you set network:true. Verify your changes by running the tests.\n\n";

/// The aden-specific suffix appended when aden is present and the scope gated.
///
/// Appended after [`AGENT_PREAMBLE_BASE`] when aden produced a scope manifest,
/// so every edit is governed. Followed immediately by the per-seed asm context.
pub(crate) const AGENT_PREAMBLE_ADEN: &str = "\
Edits are gated by aden against the task scope and reverted if they escape, so \
keep changes minimal and in scope. To search or understand code, use the aden \
tools: aden_grep, aden_locate, aden_asm, aden_understand, aden_ask.\n\n\
=== task scope context ===\n\n";

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn resolve_role_reads_config_routes_without_aden() {
        let dir = std::env::temp_dir().join(format!("coxn-route-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".aden")).unwrap();
        fs::write(
            dir.join(".aden/config.toml"),
            r#"
[provider.local]
driver = "openai_compat"
base_url = "http://localhost:11434/v1"
enabled = true

[route]
active = "local:model-a"
scout = "local:model-b"
synth = "local:model-c"
"#,
        )
        .unwrap();
        let caps = aden::AdenCaps {
            available: false,
            model_base_url: None,
            model_name: None,
        };
        let scout = resolve_role(&dir, &caps, "scout").expect("scout route");
        assert_eq!(scout.instance_id, "local");
        assert_eq!(scout.model, "model-b");
        let synth = resolve_role(&dir, &caps, "synth").expect("synth route");
        assert_eq!(synth.model, "model-c");
        let unknown = resolve_role(&dir, &caps, "missing");
        assert_eq!(unknown.as_ref().map(|s| s.model.as_str()), Some("model-a"));
        let _ = fs::remove_dir_all(&dir);
    }
}
