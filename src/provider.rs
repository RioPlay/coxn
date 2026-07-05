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
    GrokCli,
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
            "grok" | "grok_cli" | "grok-cli" | "grok_build" | "grok-build" => Self::GrokCli,
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
    /// Minimal instance for auth/discovery probes (binary + driver only).
    pub fn for_probe(
        id: impl Into<String>,
        driver: ProviderDriver,
        binary: impl Into<String>,
    ) -> Self {
        let mut instance = Self::new(id.into());
        instance.driver = driver;
        instance.binary = Some(binary.into());
        instance
    }

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

pub fn config_path(dir: &Path) -> std::path::PathBuf {
    dir.join(".aden/config.toml")
}

pub fn load_config(dir: &Path) -> ProviderConfig {
    let Ok(text) = std::fs::read_to_string(config_path(dir)) else {
        return ProviderConfig::default();
    };
    parse_config(&text)
}

/// How a preset is grouped in the setup picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetCategory {
    /// Local daemons (Ollama, LM Studio).
    Local,
    /// Cloud HTTP APIs (OpenAI, OpenRouter).
    Cloud,
    /// Installed CLI piggybacks (grok, claude, codex).
    Cli,
}

impl PresetCategory {
    pub fn title(self) -> &'static str {
        match self {
            Self::Local => "local — free, runs on your machine",
            Self::Cloud => "cloud API — needs an API key",
            Self::Cli => "CLI piggyback — uses your terminal login",
        }
    }
}

/// A built-in provider profile users can apply with `coxn auth setup <id>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub instance_id: &'static str,
    pub driver: &'static str,
    pub base_url: &'static str,
    pub model: &'static str,
    pub needs_key: bool,
    pub category: PresetCategory,
    /// Surfaced first in guided pickers.
    pub recommended: bool,
}

/// Presets grouped for menus and CLI listings.
pub fn presets_by_category() -> &'static [(PresetCategory, &'static [ProviderPreset])] {
    &[
        (PresetCategory::Cli, CLI_PRESETS),
        (PresetCategory::Local, LOCAL_PRESETS),
        (PresetCategory::Cloud, CLOUD_PRESETS),
    ]
}

const LOCAL_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "ollama-native",
        label: "Ollama (native /api/chat)",
        instance_id: "local",
        driver: "ollama",
        base_url: "http://localhost:11434",
        model: "qwen2.5-coder",
        needs_key: false,
        category: PresetCategory::Local,
        recommended: true,
    },
    ProviderPreset {
        id: "local-ollama",
        label: "Ollama (OpenAI-compat /v1)",
        instance_id: "local",
        driver: "openai_compat",
        base_url: "http://localhost:11434/v1",
        model: "qwen2.5-coder",
        needs_key: false,
        category: PresetCategory::Local,
        recommended: false,
    },
    ProviderPreset {
        id: "lmstudio",
        label: "LM Studio (:1234)",
        instance_id: "local",
        driver: "openai_compat",
        base_url: "http://localhost:1234/v1",
        model: "local",
        needs_key: false,
        category: PresetCategory::Local,
        recommended: false,
    },
];

const CLOUD_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "openrouter-claude",
        label: "Claude via OpenRouter",
        instance_id: "openrouter",
        driver: "openai_compat",
        base_url: "https://openrouter.ai/api/v1",
        model: "anthropic/claude-sonnet-4",
        needs_key: true,
        category: PresetCategory::Cloud,
        recommended: true,
    },
    ProviderPreset {
        id: "openrouter-gpt",
        label: "GPT via OpenRouter",
        instance_id: "openrouter",
        driver: "openai_compat",
        base_url: "https://openrouter.ai/api/v1",
        model: "openai/gpt-4o",
        needs_key: true,
        category: PresetCategory::Cloud,
        recommended: false,
    },
    ProviderPreset {
        id: "openrouter-gemini",
        label: "Gemini via OpenRouter",
        instance_id: "openrouter",
        driver: "openai_compat",
        base_url: "https://openrouter.ai/api/v1",
        model: "google/gemini-2.0-flash-001",
        needs_key: true,
        category: PresetCategory::Cloud,
        recommended: false,
    },
    ProviderPreset {
        id: "openrouter-grok",
        label: "Grok via OpenRouter",
        instance_id: "openrouter",
        driver: "openai_compat",
        base_url: "https://openrouter.ai/api/v1",
        model: "x-ai/grok-2-1212",
        needs_key: true,
        category: PresetCategory::Cloud,
        recommended: false,
    },
    ProviderPreset {
        id: "openai",
        label: "OpenAI API",
        instance_id: "openai",
        driver: "openai_compat",
        base_url: "https://api.openai.com/v1",
        model: "gpt-4o",
        needs_key: true,
        category: PresetCategory::Cloud,
        recommended: false,
    },
];

const CLI_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "grok-cli",
        label: "Grok Build CLI (piggyback)",
        instance_id: "grok",
        driver: "grok_cli",
        base_url: "",
        model: "grok-composer-2.5-fast",
        needs_key: false,
        category: PresetCategory::Cli,
        recommended: true,
    },
    ProviderPreset {
        id: "claude-cli",
        label: "Claude Code CLI (piggyback)",
        instance_id: "claude",
        driver: "claude_cli",
        base_url: "",
        model: "claude-sonnet-4-6",
        needs_key: false,
        category: PresetCategory::Cli,
        recommended: false,
    },
    ProviderPreset {
        id: "codex",
        label: "Codex CLI (app-server piggyback)",
        instance_id: "codex",
        driver: "codex",
        base_url: "",
        model: "gpt-5.4-mini",
        needs_key: false,
        category: PresetCategory::Cli,
        recommended: false,
    },
];

/// Named presets for common local and cloud backends (OpenAI-compat unless noted).
pub fn presets() -> &'static [ProviderPreset] {
    static ALL: std::sync::OnceLock<Vec<ProviderPreset>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        presets_by_category()
            .iter()
            .flat_map(|(_, group)| group.iter().copied())
            .collect()
    })
    .as_slice()
}

pub fn preset_by_id(id: &str) -> Option<&'static ProviderPreset> {
    presets().iter().find(|p| p.id == id)
}

/// Merge a preset into `.aden/config.toml` (creates the file if missing).
pub fn apply_preset(dir: &Path, preset_id: &str) -> Result<String, String> {
    let preset = preset_by_id(preset_id)
        .ok_or_else(|| format!("unknown preset '{preset_id}' (run: coxn auth setup)"))?;
    let path = config_path(dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let merged = merge_preset_into_config(&existing, preset);
    write_config_atomic(&path, &merged)?;
    let active = format!("{}:{}", preset.instance_id, preset.model);
    let mut notes = format!("wrote {}\nactive route: {active}\n", path.display());
    if preset.needs_key {
        notes.push_str(&format!(
            "next: export {}=your-api-key\n      or: coxn auth set-key {} < key.txt\n",
            secret_env_name(preset.instance_id),
            preset.instance_id
        ));
    } else {
        notes.push_str("no API key needed — run: coxn auth status\n");
    }
    Ok(notes)
}

fn write_config_atomic(path: &std::path::Path, content: &str) -> Result<(), String> {
    let temp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    std::fs::write(&temp, content).map_err(|e| format!("write {}: {e}", temp.display()))?;
    std::fs::rename(&temp, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("persist {}: {e}", path.display())
    })
}

fn merge_preset_into_config(existing: &str, preset: &ProviderPreset) -> String {
    let section = format!("provider.{}", preset.instance_id);
    let mut body = remove_section(existing, &section);
    body = set_route_active(&body, &format!("{}:{}", preset.instance_id, preset.model));
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    if !body.is_empty() && !body.ends_with("\n\n") {
        body.push('\n');
    }
    body.push_str(&format!(
        "[provider.{}]\n\
         driver = \"{}\"\n\
         base_url = \"{}\"\n\
         enabled = true\n",
        preset.instance_id, preset.driver, preset.base_url
    ));
    if let Some(name) = preset.label.split('(').next().map(str::trim) {
        if !name.is_empty() {
            body.push_str(&format!("display_name = \"{name}\"\n"));
        }
    }
    body
}

fn remove_section(text: &str, section_name: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let name = trimmed[1..trimmed.len() - 1].trim();
            skipping = name == section_name;
        }
        if !skipping && (!out.is_empty() || !line.is_empty()) {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

/// Persist `route.active` so hot-swaps survive restart.
pub fn set_active_route(dir: &Path, selection: &str) -> Result<(), String> {
    set_route_entry(dir, "active", selection)
}

/// Persist any `[route]` key (`active`, per-instance memory, role routes).
pub fn set_route_entry(dir: &Path, key: &str, selection: &str) -> Result<(), String> {
    if split_selection(selection).is_none() {
        return Err(format!("invalid route selection '{selection}'"));
    }
    if key.trim().is_empty() {
        return Err("empty route key".to_string());
    }
    let path = config_path(dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let merged = merge_route_key(&existing, key, selection);
    write_config_atomic(&path, &merged)
}

fn merge_route_key(text: &str, key: &str, selection: &str) -> String {
    let route_line = format!("{key} = \"{selection}\"");
    if text.contains("[route]") {
        let mut out = String::new();
        let mut in_route = false;
        let mut wrote_key = false;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed == "[route]" {
                in_route = true;
                out.push_str(line);
                out.push('\n');
                continue;
            }
            if in_route && trimmed.starts_with('[') {
                if !wrote_key {
                    out.push_str(&route_line);
                    out.push('\n');
                    wrote_key = true;
                }
                in_route = false;
            }
            if in_route && trimmed.starts_with(key) {
                out.push_str(&route_line);
                out.push('\n');
                wrote_key = true;
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        if in_route && !wrote_key {
            out.push_str(&route_line);
            out.push('\n');
        }
        out.trim_end().to_string()
    } else {
        let mut out = text.trim_end().to_string();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("[route]\n");
        out.push_str(&route_line);
        out.push('\n');
        out
    }
}

fn set_route_active(text: &str, selection: &str) -> String {
    merge_route_key(text, "active", selection)
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

/// Persist an API key to `~/.config/coxn/secrets/<id>.key` (mode 0600).
pub fn write_secret(id: &str, key: &str) -> Result<String, String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("empty key refused".to_string());
    }
    let path = secret_file_path(id).ok_or_else(|| "HOME is not set".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        set_dir_permissions(parent);
    }
    let temp_path = path.with_extension(format!("key.{}.tmp", std::process::id()));
    std::fs::write(&temp_path, format!("{key}\n"))
        .map_err(|e| format!("write {}: {e}", temp_path.display()))?;
    set_secret_permissions(&temp_path);
    std::fs::rename(&temp_path, &path).map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        format!("persist {}: {e}", path.display())
    })?;
    set_secret_permissions(&path);
    Ok(path.display().to_string())
}

#[cfg(unix)]
fn set_secret_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_secret_permissions(_path: &std::path::Path) {}

#[cfg(unix)]
fn set_dir_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn set_dir_permissions(_path: &std::path::Path) {}

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
    fn preset_apply_merges_provider_and_route() {
        let preset = preset_by_id("openrouter-claude").unwrap();
        let merged = merge_preset_into_config(
            "[provider.local]\ndriver = \"openai_compat\"\nbase_url = \"http://localhost:11434/v1\"\n",
            preset,
        );
        assert!(merged.contains("[provider.openrouter]"));
        assert!(merged.contains("anthropic/claude-sonnet-4"));
        assert!(merged.contains("[provider.local]"));
        assert!(merged.contains("active = \"openrouter:anthropic/claude-sonnet-4\""));
    }

    #[test]
    fn preset_apply_replaces_existing_provider_section() {
        let preset = preset_by_id("openai").unwrap();
        let merged = merge_preset_into_config(
            "[provider.openai]\ndriver = \"openai_compat\"\nbase_url = \"https://old.example/v1\"\nenabled = false\n",
            preset,
        );
        assert!(merged.contains("https://api.openai.com/v1"));
        assert!(!merged.contains("old.example"));
        assert_eq!(merged.matches("[provider.openai]").count(), 1);
    }

    #[test]
    fn apply_preset_writes_config_file() {
        let dir = std::env::temp_dir().join(format!("coxn-preset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        apply_preset(&dir, "lmstudio").expect("apply");
        let text = std::fs::read_to_string(config_path(&dir)).unwrap();
        assert!(text.contains("http://localhost:1234/v1"));
        assert!(text.contains("active = \"local:local\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_active_route_writes_route_section() {
        let dir = std::env::temp_dir().join(format!("coxn-route-active-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".aden")).unwrap();
        set_active_route(&dir, "grok:grok-model").expect("set route");
        let text = std::fs::read_to_string(config_path(&dir)).unwrap();
        assert!(text.contains("active = \"grok:grok-model\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn presets_include_local_and_cloud() {
        assert!(preset_by_id("local-ollama").is_some());
        assert!(preset_by_id("openrouter-gemini").is_some());
        assert!(preset_by_id("nope").is_none());
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
