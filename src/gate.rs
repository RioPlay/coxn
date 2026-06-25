//! The gate: aden's blast-radius contract.
//!
//! Before the pump accepts an edit it consults aden's `impact-diff --scope`
//! verdict and obeys the exit code: in-scope (proceed), scope-escape or
//! blast-leak (block, surface the verdict). The contract types that coxn
//! consumes are defined here. See docs/contract.adoc.
//!
//! JSON deserialization of a manifest from real `aden scope` output is deferred
//! to Phase 2 (aden wiring); the wire format and any serde dependency land
//! there. These are the plain types and the exit-code logic coxn owns now.

// Contract types are defined ahead of the pump that consumes them: the gate
// verdict is wired in P1.6, the manifest in Phase 2. Lift once wired.
#![allow(dead_code)]

/// A scope manifest emitted by `aden scope`, consumed verbatim by coxn.
///
/// The deterministic, token-budgeted definition of what context and which files
/// a task may touch. coxn never widens it. See docs/contract.adoc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeManifest {
    /// Task name; labels the turn and the gate verdict.
    pub name: String,
    /// Seed anchors the task is about (resolved by aden from task text).
    pub seeds: Vec<String>,
    /// Expanded anchor set: community ∪ transitive dependents ∪ depth-1 deps.
    pub anchors: Vec<String>,
    /// File mandate: the disjoint list of files the agent may touch.
    pub files: Vec<String>,
    /// The `asm` parameters coxn uses to pre-assemble context under budget.
    pub context: Context,
    /// Risk score for the scope; classify with [`RiskClass::classify`].
    pub risk: u32,
}

/// The context assembly parameters carried by a manifest (`context` field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    /// The anchor set to assemble.
    pub anchors: Vec<String>,
    /// The token ceiling for assembly. coxn loads no more than this.
    pub budget: u32,
}

/// Risk class thresholds, per the aden contract: `0` none, `<=5` low,
/// `<=20` medium, else high.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskClass {
    None,
    Low,
    Medium,
    High,
}

impl RiskClass {
    /// Classify a numeric risk score into its band.
    pub fn classify(risk: u32) -> Self {
        match risk {
            0 => RiskClass::None,
            1..=5 => RiskClass::Low,
            6..=20 => RiskClass::Medium,
            _ => RiskClass::High,
        }
    }
}

/// The gate verdict coxn obeys, decoded from `aden impact-diff --scope`'s exit
/// code. coxn never proceeds on any nonzero exit: a gate that cannot run is a
/// closed gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// Touched symbols and blast set stay within the manifest. Proceed.
    InScope,
    /// An edit touched a symbol or file outside the mandate. Block.
    ScopeEscape,
    /// In-scope edits whose dependents reach a sibling scope. Block.
    BlastLeak,
    /// The gate could not run (unexpected exit code). Treated as a block.
    Error(i32),
}

impl GateVerdict {
    /// Decode the verdict from `impact-diff --scope`'s exit code. See the
    /// exit-code protocol in docs/contract.adoc (provisional mapping).
    pub fn from_exit_code(code: i32) -> Self {
        match code {
            0 => GateVerdict::InScope,
            1 => GateVerdict::ScopeEscape,
            2 => GateVerdict::BlastLeak,
            other => GateVerdict::Error(other),
        }
    }

    /// Whether coxn may accept the edit. Only `in-scope` proceeds.
    pub fn proceed(&self) -> bool {
        matches!(self, GateVerdict::InScope)
    }
}

/// A gate verdict plus the human-readable message aden surfaced with it.
#[derive(Debug, Clone)]
pub struct GateOutcome {
    pub verdict: GateVerdict,
    pub message: String,
}

impl GateOutcome {
    /// Whether the edit may proceed (delegates to the verdict).
    pub fn proceed(&self) -> bool {
        self.verdict.proceed()
    }
}

/// The blast-radius gate the pump consults before accepting an edit. The real
/// implementation runs `aden impact-diff --scope`; tests use a fake. Kept a
/// trait so the pump carries no aden specifics and stays unit-testable.
pub trait Gate {
    /// Check the current working-tree edit against the scope manifest.
    fn check(&self) -> GateOutcome;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_decodes_to_verdict() {
        assert_eq!(GateVerdict::from_exit_code(0), GateVerdict::InScope);
        assert_eq!(GateVerdict::from_exit_code(1), GateVerdict::ScopeEscape);
        assert_eq!(GateVerdict::from_exit_code(2), GateVerdict::BlastLeak);
        assert_eq!(GateVerdict::from_exit_code(7), GateVerdict::Error(7));
        assert_eq!(GateVerdict::from_exit_code(-1), GateVerdict::Error(-1));
    }

    #[test]
    fn only_in_scope_proceeds() {
        assert!(GateVerdict::InScope.proceed());
        assert!(!GateVerdict::ScopeEscape.proceed());
        assert!(!GateVerdict::BlastLeak.proceed());
        assert!(!GateVerdict::Error(3).proceed());
    }

    #[test]
    fn risk_class_bands() {
        assert_eq!(RiskClass::classify(0), RiskClass::None);
        assert_eq!(RiskClass::classify(1), RiskClass::Low);
        assert_eq!(RiskClass::classify(5), RiskClass::Low);
        assert_eq!(RiskClass::classify(6), RiskClass::Medium);
        assert_eq!(RiskClass::classify(20), RiskClass::Medium);
        assert_eq!(RiskClass::classify(21), RiskClass::High);
    }

    /// The contract example manifest constructs as the typed shape coxn expects.
    #[test]
    fn manifest_matches_contract_example() {
        let m = ScopeManifest {
            name: "lint-json".to_string(),
            seeds: vec!["cmd_lint".to_string()],
            anchors: vec![
                "cmd_lint".to_string(),
                "fmt_report".to_string(),
                "lint_rule".to_string(),
            ],
            files: vec!["src/lint.rs".to_string(), "src/fmt.rs".to_string()],
            context: Context {
                anchors: vec!["cmd_lint".to_string(), "fmt_report".to_string()],
                budget: 8192,
            },
            risk: 3,
        };
        assert_eq!(m.name, "lint-json");
        assert_eq!(RiskClass::classify(m.risk), RiskClass::Low);
        assert_eq!(m.context.budget, 8192);
    }
}
