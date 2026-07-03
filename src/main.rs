//! coxn: a lean directional harness for aden.
//!
//! coxn is a "dumb pump": it steers and sets pace, and carries no intelligence.
//! aden directs and gates; the LLM acts; coxn steers. See DESIGN.adoc.

mod aden;
mod agents;
mod app;
mod auth;
mod codex_app_server;
mod codex_model;
mod codex_probe;
mod commands;
mod doctor;
mod drive;
mod execute;
mod gate;
mod layout;
mod model;
mod mouse;
mod ollama;
mod openai;
mod provider;
mod pump;
mod run_ledger;
mod sandbox;
mod session;
mod tools;
mod trust;
mod tui;
mod vim;

use std::io;

use drive::{run_once, run_tui};

fn print_cli_help() {
    println!(
        "\
coxn — lean gated terminal harness for coding LLMs

USAGE:
    coxn                 Interactive TUI (default)
    coxn doctor          Health check (model, sandbox, aden, task)
    coxn auth status     Check configured provider auth
    coxn auth setup      List provider presets; setup <id> writes config
    coxn --version       Print version
    coxn --help          This help

ENVIRONMENT:
    COXN_MODEL_BASE_URL  OpenAI-compatible endpoint
    COXN_MODEL_NAME      Model id (default: local)
    COXN_MODEL_KEY       API key (never written to config)
    COXN_KEY_<INSTANCE>  API key for a named provider instance
    COXN_BARE=1          Empty system prompt (zero-default-context purists)
    COXN_AUTO_APPROVE=1  Required for `coxn once` (auto-approves tool calls)
    COXN_VIM=1           Opt into vim-style Normal/Visual/Command modes (default: insert-only)
    COXN_TASK_NAME       Task scope (aden blast-radius gate)
    COXN_TASK_SEEDS      Comma-separated seed symbols
    COXN_ADEN_BIN        Path to aden binary

See README.md for tools, approval gate, and slash commands."
    );
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(arg) = args.first() {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("coxn {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                print_cli_help();
                return Ok(());
            }
            "doctor" => std::process::exit(doctor::run(&dir)),
            "auth" => std::process::exit(auth::run(&dir, &args[1..])),
            "once" => return run_once(&dir, &args[1..]).await,
            s if s.starts_with('-') => {
                eprintln!("coxn: unknown flag {s} (try --help)");
                std::process::exit(2);
            }
            _ => {}
        }
    }

    run_tui(&dir).await
}
