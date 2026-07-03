//! TUI 3.0 structured shell — turn model, activity feed, region render.
//!
//! Opt-in via `COXN_TUI3=1`. Legacy single-pane render remains default.

pub mod render;
pub mod transcript;

pub use render::{render_activity, render_chrome, render_conversation};
pub use transcript::{ChromeState, LiveTurn, Ui3State};

/// True when structured TUI 3.0 layout is enabled.
pub fn enabled() -> bool {
    std::env::var("COXN_TUI3")
        .ok()
        .is_some_and(|v| matches!(v.as_str(), "1" | "on" | "true" | "yes"))
}
