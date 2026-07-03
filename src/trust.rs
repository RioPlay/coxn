//! Per-tool approval trust tiers. Permission presets, not inference.

/// True when `COXN_AUTO_APPROVE` bypasses the human approval gate (`coxn once`).
pub fn auto_approve_enabled() -> bool {
    std::env::var("COXN_AUTO_APPROVE")
        .ok()
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
}

/// How a tool class is approved by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLevel {
    /// Prompt every call (mutating tools).
    Prompt,
    /// Auto-approve for the session after first allow or at boot.
    Session,
}

/// Trust policy for the three tool classes coxn exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trust {
    pub read: TrustLevel,
}

impl Default for Trust {
    fn default() -> Self {
        Self {
            read: TrustLevel::Session,
        }
    }
}

impl Trust {
    /// Trust ladder chip: supervised human gate, optional scope gate, read tier.
    pub fn ladder_tag(&self, task_gated: bool) -> &'static str {
        if auto_approve_enabled() {
            return "trust: AUTO-APPROVE";
        }
        if task_gated {
            return "trust: supervised+scope";
        }
        match self.read {
            TrustLevel::Session => "trust: supervised",
            TrustLevel::Prompt => "trust: supervised",
        }
    }

    /// Seed session approvals for tools that are session-trusted at boot.
    pub fn seed_approvals(&self, approvals: &mut std::collections::HashSet<String>) {
        if self.read == TrustLevel::Session {
            approvals.insert("read_file".to_string());
        }
    }

    /// Toggle read trust; returns a user-facing note.
    pub fn toggle_read(&mut self) -> String {
        self.read = match self.read {
            TrustLevel::Session => TrustLevel::Prompt,
            TrustLevel::Prompt => TrustLevel::Session,
        };
        format!("read_file trust → {}", self.read_label())
    }

    fn read_label(&self) -> &'static str {
        match self.read {
            TrustLevel::Session => "session-auto",
            TrustLevel::Prompt => "prompt",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_shows_auto_approve_when_env_set() {
        // SAFETY: single-threaded test; cleared on drop.
        unsafe { std::env::set_var("COXN_AUTO_APPROVE", "1") };
        assert_eq!(Trust::default().ladder_tag(false), "trust: AUTO-APPROVE");
        unsafe { std::env::remove_var("COXN_AUTO_APPROVE") };
    }

    #[test]
    fn ladder_shows_scope_when_task_gated() {
        assert_eq!(Trust::default().ladder_tag(true), "trust: supervised+scope");
    }
}
