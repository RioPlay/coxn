//! Explicit provider auth helpers.
//!
//! These commands are user-initiated. coxn does not run cloud auth probes in the
//! background.

use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

use crate::codex_probe;
use crate::openai;
use crate::provider::{self, ProviderDriver};

pub fn run(dir: &Path, args: &[String]) -> i32 {
    let result = report(dir, args);
    print!("{}", result.output);
    result.code
}

pub struct AuthReport {
    pub code: i32,
    pub output: String,
}

pub fn report(dir: &Path, args: &[String]) -> AuthReport {
    match args.first().map(String::as_str) {
        Some("status") | None => status(dir),
        Some("setup") | Some("list") => setup(dir, args.get(1).map(String::as_str)),
        Some("login") => {
            let Some(id) = args.get(1) else {
                return AuthReport {
                    code: 2,
                    output: "usage: coxn auth login <id>\n".to_string(),
                };
            };
            login(dir, id)
        }
        Some("set-key") => {
            let Some(id) = args.get(1) else {
                return AuthReport {
                    code: 2,
                    output: "usage: coxn auth set-key <id> < key.txt\n".to_string(),
                };
            };
            set_key(id)
        }
        Some(other) => AuthReport {
            code: 2,
            output: format!(
                "coxn auth: unknown subcommand {other}\nusage: coxn auth status | list | setup [preset] | login <id> | set-key <id>\n"
            ),
        },
    }
}

fn setup(dir: &Path, preset_id: Option<&str>) -> AuthReport {
    let Some(id) = preset_id else {
        let mut out =
            String::from("provider setup wizard — pick one (TUI: /auth setup opens a picker):\n\n");
        for (category, group) in provider::presets_by_category() {
            out.push_str(&format!("{}\n", category.title()));
            for p in *group {
                let readiness = crate::discover::probe_preset(p);
                let star = if p.recommended { " ★" } else { "" };
                let key = if p.needs_key {
                    format!("needs {}", provider::secret_env_name(p.instance_id))
                } else {
                    "no key".to_string()
                };
                out.push_str(&format!(
                    "  {} {:<20}{star} {} — {key} ({})\n",
                    readiness.badge(),
                    p.id,
                    p.label,
                    readiness.hint()
                ));
            }
            out.push('\n');
        }
        out.push_str("example: coxn auth setup grok-cli\n");
        out.push_str("example: coxn auth setup ollama-native\n");
        return AuthReport {
            code: 0,
            output: out,
        };
    };
    match provider::apply_preset(dir, id) {
        Ok(msg) => AuthReport {
            code: 0,
            output: msg,
        },
        Err(e) => AuthReport {
            code: 1,
            output: format!("{e}\n"),
        },
    }
}

fn status(dir: &Path) -> AuthReport {
    let mut blocking = false;
    let mut output = String::new();

    let ready = crate::discover::list_ready_backends(dir);
    if !ready.is_empty() {
        output.push_str("ready backends (hot-swap via /model or Ctrl-Space):\n");
        let active = provider::load_config(dir).route("active");
        for b in &ready {
            let mark = if active
                .as_ref()
                .is_some_and(|a| a.instance_id == b.instance_id)
            {
                "✓"
            } else {
                " "
            };
            let tag = if b.text_only { " [text-only]" } else { "" };
            output.push_str(&format!("  {mark} {} · {}{tag}\n", b.display_name, b.model));
        }
        output.push('\n');
    }

    let cfg = provider::load_config(dir);
    if cfg.instances.is_empty() && ready.is_empty() {
        return AuthReport {
            code: 0,
            output: "no provider profiles configured — try: coxn auth setup grok-cli\n".to_string(),
        };
    }
    if !cfg.instances.is_empty() {
        output.push_str("configured providers:\n");
    }
    for instance in &cfg.instances {
        if !instance.enabled {
            output.push_str(&format!("○ {}: disabled\n", instance.id));
            continue;
        }
        match &instance.driver {
            ProviderDriver::OpenAiCompat => {
                let Some(base) = instance.base_url.as_deref() else {
                    blocking = true;
                    output.push_str(&format!("✗ {}: missing base_url\n", instance.id));
                    continue;
                };
                let key = provider::secret_for_instance(&instance.id);
                if provider::cloud_blocked(base, key.as_deref()) {
                    blocking = true;
                    output.push_str(&format!(
                        "✗ {}: cloud probe blocked; set {} or COXN_ALLOW_CLOUD=1",
                        instance.id,
                        provider::secret_env_name(&instance.id)
                    ));
                    output.push('\n');
                    continue;
                }
                match openai::list_models(base, key.as_deref()) {
                    Some(models) => {
                        output.push_str(&format!(
                            "✓ {}: reachable ({} model(s))\n",
                            instance.id,
                            models.len()
                        ));
                    }
                    None => {
                        blocking = true;
                        output.push_str(&format!(
                            "✗ {}: /models probe failed at {}\n",
                            instance.id, base
                        ));
                    }
                }
            }
            ProviderDriver::Stub => output.push_str(&format!("✓ {}: stub\n", instance.id)),
            ProviderDriver::Ollama => {
                // Ollama is local and keyless; probe reachability instead.
                let base = instance
                    .base_url
                    .as_deref()
                    .unwrap_or("http://localhost:11434");
                if crate::ollama::reachable(base) {
                    output.push_str(&format!(
                        "✓ {}: ollama native reachable at {base} (no key)\n",
                        instance.id
                    ));
                } else {
                    blocking = true;
                    output.push_str(&format!(
                        "✗ {}: ollama not reachable at {base}\n",
                        instance.id
                    ));
                }
            }
            ProviderDriver::Codex => {
                let bin = instance.binary.as_deref().unwrap_or("codex");
                let outcome = codex_probe::probe_instance(instance);
                let (is_blocking, line) =
                    codex_probe::format_status_line(&instance.id, bin, &outcome);
                if is_blocking {
                    blocking = true;
                }
                output.push_str(&format!("{line}\n"));
            }
            ProviderDriver::ClaudeCli => {
                let bin = instance.binary.as_deref().unwrap_or("claude");
                let home = instance.home_path.as_deref();
                if !binary_responds(bin) {
                    blocking = true;
                    output.push_str(&format!(
                        "✗ {}: {bin} not installed or not runnable\n",
                        instance.id
                    ));
                } else if crate::claude_cli::probe_logged_in(bin, home, &instance.env) {
                    output.push_str(&format!("✓ {}: {bin} authenticated\n", instance.id));
                } else {
                    blocking = true;
                    output.push_str(&format!(
                        "✗ {}: {bin} installed but not logged in (`{bin} login`)\n",
                        instance.id
                    ));
                }
            }
            ProviderDriver::GrokCli => {
                let bin = instance.binary.as_deref().unwrap_or("grok");
                let home = instance.home_path.as_deref();
                if !binary_responds(bin) {
                    blocking = true;
                    output.push_str(&format!(
                        "✗ {}: {bin} not installed or not runnable\n",
                        instance.id
                    ));
                } else if crate::grok_cli::probe_logged_in(bin, home, &instance.env) {
                    output.push_str(&format!("✓ {}: {bin} authenticated\n", instance.id));
                } else {
                    blocking = true;
                    output.push_str(&format!(
                        "✗ {}: {bin} installed but not logged in (`{bin} login`)\n",
                        instance.id
                    ));
                }
            }
            ProviderDriver::Unknown(driver) => {
                blocking = true;
                output.push_str(&format!("✗ {}: unknown driver '{}'\n", instance.id, driver));
            }
        }
    }

    AuthReport {
        code: if blocking { 1 } else { 0 },
        output,
    }
}

fn login(dir: &Path, id: &str) -> AuthReport {
    let cfg = provider::load_config(dir);
    let Some(instance) = cfg.instance(id) else {
        return AuthReport {
            code: 1,
            output: format!("provider instance '{id}' not found\n"),
        };
    };
    let output = match &instance.driver {
        ProviderDriver::OpenAiCompat => {
            format!(
                "set an API key with:\n  export {}=...\nor:\n  coxn auth set-key {} < key.txt",
                provider::secret_env_name(&instance.id),
                instance.id
            )
        }
        ProviderDriver::Codex => {
            let bin = instance.binary.as_deref().unwrap_or("codex");
            format!("run native login:\n  {bin} login")
        }
        ProviderDriver::ClaudeCli => {
            let bin = instance.binary.as_deref().unwrap_or("claude");
            format!("run native login:\n  {bin} login")
        }
        ProviderDriver::GrokCli => {
            let bin = instance.binary.as_deref().unwrap_or("grok");
            format!("run native login:\n  {bin} login")
        }
        ProviderDriver::Stub => format!("{} is offline stub; no auth needed", instance.id),
        ProviderDriver::Ollama => format!(
            "{} is native Ollama: local and keyless (no login needed)",
            instance.id
        ),
        ProviderDriver::Unknown(driver) => format!(
            "{} uses unknown driver '{}'; install a coxn build that supports it",
            instance.id, driver
        ),
    };
    AuthReport {
        code: 0,
        output: format!("{output}\n"),
    }
}

fn set_key(id: &str) -> AuthReport {
    let mut key = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut key) {
        return AuthReport {
            code: 1,
            output: format!("failed to read key from stdin: {e}\n"),
        };
    }
    match provider::write_secret(id, &key) {
        Ok(path) => AuthReport {
            code: 0,
            output: format!("wrote {path}\n"),
        },
        Err(e) => AuthReport {
            code: 1,
            output: format!("{e}\n"),
        },
    }
}

fn binary_responds(bin: &str) -> bool {
    let Ok(mut child) = std::process::Command::new(bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if start.elapsed() < Duration::from_secs(3) => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                return false;
            }
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_list_alias_matches_setup_listing() {
        let dir = Path::new(".");
        let list = report(dir, &["list".to_string()]);
        let setup = report(dir, &["setup".to_string()]);
        assert_eq!(list.code, 0);
        assert_eq!(setup.code, 0);
        // `list` and `setup` render the same wizard. Readiness badges (✓/▷) may
        // differ between back-to-back probes when a local daemon is flapping.
        for preset in provider::presets() {
            assert!(
                list.output.contains(preset.id),
                "list missing {}",
                preset.id
            );
            assert!(
                setup.output.contains(preset.id),
                "setup missing {}",
                preset.id
            );
        }
        assert_eq!(
            list.output.lines().count(),
            setup.output.lines().count(),
            "list and setup should have the same structure"
        );
        assert!(setup.output.contains("CLI piggyback"));
        assert!(setup.output.contains("grok-cli"));
    }
}
