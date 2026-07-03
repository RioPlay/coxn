//! Short-lived `codex app-server` JSONL probe for auth status (no chat API).

use crate::codex_app_server::{self, CodexAccountWire, CodexAppServerConfig};
use crate::provider::ProviderInstance;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAccount {
    pub account_type: String,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub requires_openai_auth: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexProbeOutcome {
    Authenticated(CodexAccount),
    NotLoggedIn,
    NotInstalled,
    ProbeFailed(String),
}

/// Probe Codex account state via `initialize` → `initialized` → `account/read`.
pub fn probe_instance(instance: &ProviderInstance) -> CodexProbeOutcome {
    let bin = instance.binary.as_deref().unwrap_or("codex");
    if !codex_app_server::binary_installed(bin) {
        return CodexProbeOutcome::NotInstalled;
    }
    let config = CodexAppServerConfig::for_probe(
        bin.to_string(),
        codex_home(instance).map(str::to_string),
        instance.env.clone(),
    );
    match CodexAppServerSessionProbe::run(&config) {
        Ok(account) => {
            if account.email.is_some() {
                CodexProbeOutcome::Authenticated(account)
            } else {
                CodexProbeOutcome::NotLoggedIn
            }
        }
        Err(reason) => CodexProbeOutcome::ProbeFailed(reason),
    }
}

pub fn format_status_line(
    instance_id: &str,
    bin: &str,
    outcome: &CodexProbeOutcome,
) -> (bool, String) {
    match outcome {
        CodexProbeOutcome::Authenticated(account) => {
            let email = account.email.as_deref().unwrap_or("(unknown)");
            let plan = account
                .plan_type
                .as_deref()
                .map(|p| format!(", {p}"))
                .unwrap_or_default();
            (
                false,
                format!(
                    "✓ {instance_id}: {bin} authenticated ({account_type}, {email}{plan})",
                    account_type = account.account_type,
                    email = email,
                    plan = plan,
                ),
            )
        }
        CodexProbeOutcome::NotLoggedIn => (
            true,
            format!("✗ {instance_id}: {bin} installed but not logged in (`{bin} login`)"),
        ),
        CodexProbeOutcome::NotInstalled => (
            true,
            format!("✗ {instance_id}: {bin} not installed or not runnable"),
        ),
        CodexProbeOutcome::ProbeFailed(reason) => (
            true,
            format!("✗ {instance_id}: codex app-server probe failed ({reason})"),
        ),
    }
}

#[allow(dead_code)]
pub fn binary_installed(bin: &str) -> bool {
    codex_app_server::binary_installed(bin)
}

fn codex_home(instance: &ProviderInstance) -> Option<&str> {
    instance
        .shadow_home
        .as_deref()
        .or(instance.home_path.as_deref())
}

struct CodexAppServerSessionProbe;

impl CodexAppServerSessionProbe {
    fn run(config: &CodexAppServerConfig) -> Result<CodexAccount, String> {
        let mut session = codex_app_server::CodexAppServerSession::spawn(config)?;
        let wire = session.account_read()?;
        Ok(from_wire(wire))
    }
}

fn from_wire(wire: CodexAccountWire) -> CodexAccount {
    CodexAccount {
        account_type: wire.account_type,
        email: wire.email,
        plan_type: wire.plan_type,
        requires_openai_auth: wire.requires_openai_auth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_app_server::test_support::{FakeCodexMode, write_fake_codex};

    #[test]
    fn probe_parses_account_read_from_fake_binary() {
        let _guard = crate::cli_ndjson::test_support::fake_cli_test_lock();
        let dir = crate::cli_ndjson::test_support::unique_temp_dir("coxn-codex-probe");
        let fake = write_fake_codex(&dir, FakeCodexMode::AuthOnly);
        let config =
            CodexAppServerConfig::for_probe(fake.to_string_lossy().to_string(), None, vec![]);
        let account = CodexAppServerSessionProbe::run(&config).expect("probe should succeed");
        assert_eq!(account.account_type, "chatgpt");
        assert_eq!(account.email.as_deref(), Some("user@example.com"));
        assert_eq!(account.plan_type.as_deref(), Some("plus"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_status_line_reports_authenticated_account() {
        let outcome = CodexProbeOutcome::Authenticated(CodexAccount {
            account_type: "chatgpt".to_string(),
            email: Some("user@example.com".to_string()),
            plan_type: Some("plus".to_string()),
            requires_openai_auth: false,
        });
        let (blocking, line) = format_status_line("codex-main", "codex", &outcome);
        assert!(!blocking);
        assert!(line.contains("authenticated"));
        assert!(line.contains("user@example.com"));
    }
}
