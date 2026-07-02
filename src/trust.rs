//! Per-tool approval trust tiers. Permission presets, not inference.

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
    /// Short status fragment for the status line.
    pub fn status_tag(&self) -> &'static str {
        match self.read {
            TrustLevel::Session => "trust: read-auto",
            TrustLevel::Prompt => "trust: read-gated",
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
