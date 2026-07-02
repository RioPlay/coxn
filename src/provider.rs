//! Provider instance configuration and routing.
//!
//! coxn keeps provider selection as data: named instances plus a model id. This
//! module deliberately stops at config/secrets/routing; model execution still
//! goes through the single provider-neutral seam in `model.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderDriver {
    OpenAiCompat,
    Ollama,
    Stub,
    Codex,
    ClaudeCli,
    Unknown(String),
}

impl ProviderDriver {
    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "openai_compat" | "openai-compatible" | "openai" => Self::OpenAiCompat,
            "ollama" | "ollama_native" => Self::Ollama,
            "stub" => Self::Stub,
            "codex" => Self::Codex,
            "claude" | "claude_cli" | "claude-cli" => Self::ClaudeCli,
            _ => Self::Unknown(value.trim().to_string()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderInstance {
    pub id: String,
    pub driver: ProviderDriver,
    pub display_name: Option<String>,
    pub enabled: bool,
    pub base_url: Option<String>,
    pub binary: Option<String>,
    pub home_path: Option<String>,
    pub shadow_home: Option<String>,
    pub env: Vec<(String, String)>,
    pub secret_env_keys: Vec<String>,
}

impl ProviderInstance {
    fn new(id: String) -> Self {
        Self {
            id,
            driver: ProviderDriver::OpenAiCompat,
            display_name: None,
            enabled: true,
            base_url: None,
            binary: None,
            home_path: None,
            shadow_home: None,
            env: Vec::new(),
            secret_env_keys: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSelection {
    pub instance_id: String,
    pub model: String,
}

#[derive(Clone, Debug, Default)]
pub struct ProviderConfig {
    pub instances: Vec<ProviderInstance>,
    pub routes: HashMap<String, ModelSelection>,
}

impl ProviderConfig {
    pub fn instance(&self, id: &str) -> Option<&ProviderInstance> {
        self.instances.iter().find(|p| p.id == id)
    }

    pub fn route(&self, key: &str) -> Option<ModelSelection> {
        self.routes.get(key).cloned()
    }
}

pub fn load_config(dir: &Path) -> ProviderConfig {
    let path = dir.join(".aden/config.toml");
    let Ok(text) = std::fs::read_to_string(path) else {
        return ProviderConfig::default();
    };
    parse_config(&text)
}

pub fn split_selection(value: &str) -> Option<ModelSelection> {
    let (instance_id, model) = value.split_once(':')?;
    let instance_id = instance_id.trim();
    let model = model.trim();
    if !valid_provider_slug(instance_id) || model.is_empty() {
        return None;
    }
    Some(ModelSelection {
        instance_id: instance_id.to_string(),
        model: model.to_string(),
    })
}

pub fn secret_for_instance(id: &str) -> Option<String> {
    std::env::var(secret_env_name(id))
        .ok()
        .or_else(|| read_secret_file(id))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn secret_env_name(id: &str) -> String {
    format!(
        "COXN_KEY_{}",
        id.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
    )
}

pub fn secret_file_path(id: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("coxn")
            .join("secrets")
            .join(format!("{id}.key")),
    )
}

pub fn cloud_allowed() -> bool {
    std::env::var("COXN_ALLOW_CLOUD")
        .ok()
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
}

pub fn is_local_base_url(base_url: &str) -> bool {
    let url = base_url.trim().to_ascii_lowercase();
    url.contains("localhost")
        || url.contains("127.0.0.1")
        || url.contains("[::1]")
        || url.contains("://0.0.0.0")
}

pub fn cloud_blocked(base_url: &str, key: Option<&str>) -> bool {
    !is_local_base_url(base_url) && key.is_none() && !cloud_allowed()
}

fn read_secret_file(id: &str) -> Option<String> {
    std::fs::read_to_string(secret_file_path(id)?).ok()
}

fn parse_config(text: &str) -> ProviderConfig {
    let mut cfg = ProviderConfig::default();
    let mut section = Section::Other;

    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = parse_section(name.trim(), &mut cfg);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = trim_value(value.trim());
        match &section {
            Section::Provider(id) => {
                if let Some(instance) = cfg.instances.iter_mut().find(|p| p.id == *id) {
                    apply_provider_key(instance, key, value);
                }
            }
            Section::Route => {
                if let Some(selection) = split_selection(value) {
                    cfg.routes.insert(key.to_string(), selection);
                }
            }
            Section::Other => {}
        }
    }

    cfg
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Section {
    Provider(String),
    Route,
    Other,
}

fn parse_section(name: &str, cfg: &mut ProviderConfig) -> Section {
    if name == "route" {
        return Section::Route;
    }
    if let Some(id) = name.strip_prefix("provider.") {
        let id = id.trim().to_string();
        if valid_provider_slug(&id) && cfg.instance(&id).is_none() {
            cfg.instances.push(ProviderInstance::new(id.clone()));
        }
        return Section::Provider(id);
    }
    Section::Other
}

fn apply_provider_key(instance: &mut ProviderInstance, key: &str, value: &str) {
    match key {
        "driver" => instance.driver = ProviderDriver::parse(value),
        "display_name" | "name" => instance.display_name = Some(value.to_string()),
        "enabled" => instance.enabled = parse_bool(value),
        "base_url" => instance.base_url = Some(value.to_string()),
        "binary" => instance.binary = Some(value.to_string()),
        "home_path" | "codex_home" | "claude_home" => instance.home_path = Some(value.to_string()),
        "shadow_home" => instance.shadow_home = Some(value.to_string()),
        "secret_env_key" => instance.secret_env_keys.push(value.to_string()),
        _ if key.starts_with("env.") => {
            let env_key = key.trim_start_matches("env.").to_string();
            if valid_env_var_name(&env_key) {
                instance.env.push((env_key, value.to_string()));
            }
        }
        _ => {}
    }
}

fn trim_value(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(value)
}

fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

fn valid_provider_slug(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    value.len() <= 64
        && first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn valid_env_var_name(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    value.len() <= 128
        && (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_instances_and_routes() {
        let cfg = parse_config(
            r#"
            [provider.local]
            driver = "openai_compat"
            base_url = "http://localhost:11434/v1"

            [provider.openrouter]
            driver = "openai_compat"
            base_url = "https://openrouter.ai/api/v1"
            enabled = false

            [route]
            active = "local:qwen2.5-coder"
            synth = "openrouter:anthropic/claude-sonnet-4"
            "#,
        );

        assert_eq!(cfg.instances.len(), 2);
        assert!(cfg.instance("local").unwrap().enabled);
        assert!(!cfg.instance("openrouter").unwrap().enabled);
        assert_eq!(
            cfg.route("synth"),
            Some(ModelSelection {
                instance_id: "openrouter".to_string(),
                model: "anthropic/claude-sonnet-4".to_string()
            })
        );
    }

    #[test]
    fn splits_instance_model_selection() {
        assert_eq!(
            split_selection("local:qwen2.5-coder"),
            Some(ModelSelection {
                instance_id: "local".to_string(),
                model: "qwen2.5-coder".to_string()
            })
        );
        assert_eq!(split_selection("missing-colon"), None);
        assert_eq!(split_selection(":model"), None);
        assert_eq!(split_selection("local:"), None);
        assert_eq!(split_selection("1bad:model"), None);
    }

    #[test]
    fn detects_cloud_gate() {
        assert!(!cloud_blocked("http://localhost:11434/v1", None));
        assert!(cloud_blocked("https://openrouter.ai/api/v1", None));
        assert!(!cloud_blocked("https://openrouter.ai/api/v1", Some("sk")));
    }

    #[test]
    fn preserves_unknown_drivers_as_unavailable() {
        let cfg = parse_config(
            r#"
            [provider.future]
            driver = "future_driver"
            base_url = "https://example.com/v1"
            "#,
        );

        assert_eq!(
            cfg.instance("future").map(|p| &p.driver),
            Some(&ProviderDriver::Unknown("future_driver".to_string()))
        );
    }

    #[test]
    fn ignores_invalid_provider_ids_and_env_names() {
        let cfg = parse_config(
            r#"
            [provider.1bad]
            driver = "openai_compat"

            [provider.good]
            env.GOOD_KEY = "yes"
            env.1BAD = "no"
            "#,
        );

        assert!(cfg.instance("1bad").is_none());
        assert_eq!(
            cfg.instance("good").map(|p| &p.env),
            Some(&vec![("GOOD_KEY".to_string(), "yes".to_string())])
        );
    }
}
