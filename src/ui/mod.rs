//! TUI 3.0 structured shell — turn model, activity feed, region render.
//!
//! Structured shell by default; set `COXN_TUI3=0` for legacy single-pane.

pub mod render;
pub mod transcript;

pub use render::{render_activity, render_chrome, render_conversation};
pub use transcript::{ChromeState, LiveTurn, Ui3State};

/// True when structured TUI 3.0 layout is enabled (default on; `COXN_TUI3=0` disables).
pub fn enabled() -> bool {
    match std::env::var("COXN_TUI3").ok().as_deref() {
        Some("0" | "off" | "false" | "no") => false,
        Some(_) | None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env;

    #[test]
    fn enabled_by_default() {
        let _guard = test_env::lock();
        unsafe { std::env::remove_var("COXN_TUI3") };
        assert!(enabled());
    }

    #[test]
    fn disabled_when_zero() {
        let _guard = test_env::lock();
        unsafe { std::env::set_var("COXN_TUI3", "0") };
        assert!(!enabled());
        unsafe { std::env::remove_var("COXN_TUI3") };
    }
}
