//! The TUI chrome: ratatui + crossterm, minimal.
//!
//! A streaming output pane, a status line, and a confirm modal. Immediate-mode
//! render loop: append to a buffer, redraw next frame. No graph rendering;
//! the inspector stays browser-native (`aden view`).
