//! Explicit provider auth helpers.
//!
//! These commands are user-initiated. coxn does not run cloud auth probes in the
//! background.

use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

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
        Some("setup") => setup(dir, args.get(1).map(String::as_str)),
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
                "coxn auth: unknown subcommand {other}\nusage: coxn auth status | setup [preset] | login <id> | set-key <id>\n"
            ),
        },
    }
}

fn setup(dir: &Path, preset_id: Option<&str>) -> AuthReport {
    let Some(id) = preset_id else {
        let mut out =
            String::from("provider presets (coxn auth setup <id> or /auth setup <id>):\n\n");
        for p in provider::presets() {
            let key = if p.needs_key {
                format!("needs {}", provider::secret_env_name(p.instance_id))
            } else {
                "no key".to_string()
            };
            out.push_str(&format!("  {:<18} {} — {key}\n", p.id, p.label));
        }
        out.push_str("\nexample: coxn auth setup openrouter-claude\n");
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
    let cfg = provider::load_config(dir);
    if cfg.instances.is_empty() {
        return AuthReport {
            code: 0,
            output: "no provider profiles configured\n".to_string(),
        };
    }

    let mut blocking = false;
    let mut output = String::new();
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
                if binary_responds(bin) {
                    output.push_str(&format!("✓ {}: {bin} installed\n", instance.id));
                } else {
                    blocking = true;
                    output.push_str(&format!(
                        "✗ {}: {bin} not installed or not runnable\n",
                        instance.id
                    ));
                }
            }
            ProviderDriver::ClaudeCli => {
                let bin = instance.binary.as_deref().unwrap_or("claude");
                if binary_responds(bin) {
                    output.push_str(&format!("✓ {}: {bin} installed\n", instance.id));
                } else {
                    blocking = true;
                    output.push_str(&format!(
                        "✗ {}: {bin} not installed or not runnable\n",
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
    let key = key.trim();
    if key.is_empty() {
        return AuthReport {
            code: 1,
            output: "empty key refused\n".to_string(),
        };
    }
    let Some(path) = provider::secret_file_path(id) else {
        return AuthReport {
            code: 1,
            output: "HOME is not set; cannot choose secret path\n".to_string(),
        };
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return AuthReport {
            code: 1,
            output: format!("failed to create {}: {e}\n", parent.display()),
        };
    }
    if let Some(parent) = path.parent() {
        set_dir_permissions(parent);
    }
    let temp_path = path.with_extension(format!("key.{}.tmp", std::process::id()));
    if let Err(e) = std::fs::write(&temp_path, format!("{key}\n")) {
        return AuthReport {
            code: 1,
            output: format!("failed to write {}: {e}\n", temp_path.display()),
        };
    }
    set_secret_permissions(&temp_path);
    if let Err(e) = std::fs::rename(&temp_path, &path) {
        let _ = std::fs::remove_file(&temp_path);
        return AuthReport {
            code: 1,
            output: format!("failed to persist {}: {e}\n", path.display()),
        };
    }
    set_secret_permissions(&path);
    AuthReport {
        code: 0,
        output: format!("wrote {}\n", path.display()),
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

#[cfg(unix)]
fn set_secret_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_secret_permissions(_path: &Path) {}

#[cfg(unix)]
fn set_dir_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn set_dir_permissions(_path: &Path) {}
