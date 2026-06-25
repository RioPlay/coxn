//! Confine a command coxn runs to the project root, via bwrap when present.
//!
//! Running a shell command is the riskiest capability coxn exposes: arbitrary
//! execution. The boundary is layered. The human approves the exact command
//! (the pump's approval gate); underneath that, this module confines it. With
//! `bwrap` present the command runs in fresh namespaces -- the project root is
//! read-write, the rest of the filesystem read-only, there is no network unless
//! it is asked for, and the environment is cleared so none of the parent's
//! secrets (model API keys and the like) leak in. With `bwrap` absent the
//! command runs directly with that same cleared-and-whitelisted environment and
//! the cwd pinned to the project root; the approval prompt is then the only
//! isolation. Either way the output is capped and a wall-clock timeout bounds
//! it. No new crate: `bwrap` and `timeout` are shelled out, the same pattern as
//! aden.

use std::path::Path;
use std::process::{Command, Stdio};

/// Whether the command was confined by bwrap or fell back to direct execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confinement {
    /// Ran inside a bwrap sandbox (filesystem, network, and pid confined).
    Sandboxed,
    /// bwrap was unavailable; ran directly with a cleared environment and the
    /// working directory pinned to the project root. The approval prompt was
    /// the only isolation.
    Unsandboxed,
}

/// The outcome of running a command.
pub struct RunOutcome {
    pub confinement: Confinement,
    /// The process exit code, or `None` if it was killed by a signal.
    pub exit_code: Option<i32>,
    /// Set when the wall-clock timeout fired (the process was killed).
    pub timed_out: bool,
    /// Combined stdout+stderr, already capped to a bounded size.
    pub output: String,
}

/// Default wall-clock budget for a command; `COXN_RUN_TIMEOUT_SECS` overrides.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Seconds between SIGTERM (at the deadline) and SIGKILL.
const KILL_GRACE_SECS: u64 = 5;
/// `timeout`'s exit code when it kills a command at the deadline.
const TIMEOUT_EXIT_CODE: i32 = 124;
/// Output line caps: the head and tail are kept and the middle elided. Errors
/// live at the tail of build/test output, context at the head.
const HEAD_LINES: usize = 120;
const TAIL_LINES: usize = 120;
/// A hard character backstop, in case the output is few but enormous lines.
const OUTPUT_CHAR_CAP: usize = 60_000;

/// Probe whether `bwrap` is usable. Cheap; coxn calls it once at startup and
/// passes the result down, so a missing sandbox is known before the first run.
pub fn bwrap_available() -> bool {
    probe("bwrap")
}

/// Whether a helper binary runs (gates bwrap and timeout).
fn probe(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The wall-clock budget in seconds (env override, zero or junk ignored).
fn timeout_secs() -> u64 {
    std::env::var("COXN_RUN_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
}

/// Resolve an env var, falling back to `default` when unset or empty.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// The safe environment passed to a command: a fixed whitelist, never the
/// parent's full environment (which may hold model API keys). Returned as
/// `(key, value)` pairs; applied directly in the fallback path and mirrored
/// into bwrap's `--setenv` flags in the sandboxed path. `home` is the value of
/// `HOME` to expose (a tmpfs path inside the sandbox; the real home in the
/// fallback), kept separate from the real toolchain dirs so cargo/rustup still
/// resolve.
fn safe_env(home: &str, cargo_home: &str, rustup_home: &str) -> Vec<(String, String)> {
    let mut env = vec![
        (
            "PATH".to_string(),
            format!("{cargo_home}/bin:/usr/local/bin:/usr/bin:/bin"),
        ),
        ("HOME".to_string(), home.to_string()),
        ("USER".to_string(), env_or("USER", "user")),
        ("TERM".to_string(), env_or("TERM", "xterm-256color")),
        ("LANG".to_string(), env_or("LANG", "C.UTF-8")),
        ("CARGO_HOME".to_string(), cargo_home.to_string()),
        ("RUSTUP_HOME".to_string(), rustup_home.to_string()),
    ];
    // Pass a few optional toolchain knobs through only when actually set.
    for opt in ["RUSTC_WRAPPER", "RUSTUP_TOOLCHAIN", "CARGO_TARGET_DIR"] {
        if let Ok(v) = std::env::var(opt)
            && !v.is_empty()
        {
            env.push((opt.to_string(), v));
        }
    }
    env
}

/// Build the bwrap flag list: project root read-write, the rest of the
/// filesystem read-only, fresh namespaces, no network unless `network`, a
/// cleared-and-whitelisted environment, and `sh -c <command>` as the payload.
fn bwrap_args(
    root: &Path,
    network: bool,
    env: &[(String, String)],
    cargo_home: &str,
    rustup_home: &str,
    command: &str,
) -> Vec<String> {
    let root = root.display().to_string();
    let mut a: Vec<String> = vec!["--unshare-all".to_string()];
    // Network is off by default (a fresh, empty net namespace); --share-net
    // re-shares the host network only when the caller opted in.
    if network {
        a.push("--share-net".to_string());
    }
    // Merged-/usr base, read-only.
    a.extend(["--ro-bind", "/usr", "/usr"].map(String::from));
    a.extend(["--symlink", "usr/bin", "/bin"].map(String::from));
    a.extend(["--symlink", "usr/sbin", "/sbin"].map(String::from));
    a.extend(["--symlink", "usr/lib", "/lib"].map(String::from));
    a.extend(["--symlink", "usr/lib", "/lib64"].map(String::from));
    // Resolver, certificates, and clock, best-effort (a missing one is fine).
    for p in [
        "/etc/passwd",
        "/etc/group",
        "/etc/nsswitch.conf",
        "/etc/resolv.conf",
        "/etc/ssl",
        "/etc/ca-certificates",
        "/etc/pki",
        "/etc/localtime",
    ] {
        a.push("--ro-bind-try".to_string());
        a.push(p.to_string());
        a.push(p.to_string());
    }
    // Virtual filesystems. /tmp and /run are fresh tmpfs (so HOME=/tmp is
    // writable and empty).
    a.extend(["--proc", "/proc"].map(String::from));
    a.extend(["--dev", "/dev"].map(String::from));
    a.extend(["--tmpfs", "/tmp"].map(String::from));
    a.extend(["--tmpfs", "/run"].map(String::from));
    // Toolchain caches, read-only and best-effort, so cargo/rustc work offline
    // without letting a command tamper with the host cache or its config.
    a.push("--ro-bind-try".to_string());
    a.push(cargo_home.to_string());
    a.push(cargo_home.to_string());
    a.push("--ro-bind-try".to_string());
    a.push(rustup_home.to_string());
    a.push(rustup_home.to_string());
    // The project: the one read-write path on the host.
    a.push("--bind".to_string());
    a.push(root.clone());
    a.push(root.clone());
    a.push("--chdir".to_string());
    a.push(root);
    // Clear the environment, then set exactly the whitelist.
    a.push("--clearenv".to_string());
    for (k, v) in env {
        a.push("--setenv".to_string());
        a.push(k.clone());
        a.push(v.clone());
    }
    // New session (closes the TIOCSTI terminal-injection vector) and tear the
    // sandbox down if coxn dies.
    a.push("--new-session".to_string());
    a.push("--die-with-parent".to_string());
    a.push("--".to_string());
    a.push("sh".to_string());
    a.push("-c".to_string());
    a.push(command.to_string());
    a
}

/// Run `command` confined to `root`. `network` opts the sandbox into the host
/// network; `use_bwrap` selects the sandbox or the direct-exec fallback (coxn
/// probes bwrap once at startup and passes the answer here). Blocking: the
/// caller runs it on the pump's synchronous tool-dispatch path.
//
// FIXME(async): this blocks until the command exits, freezing the TUI for the
// duration (bounded by the timeout). The non-blocking upgrade is a tokio
// child + line channel drained by the draw loop; it needs an async Tool::run,
// so it is a separate change. The approval prompt warns the user a command is
// about to run, so the freeze is expected, not a surprise.
pub fn run(root: &Path, command: &str, network: bool, use_bwrap: bool) -> RunOutcome {
    let confinement = if use_bwrap {
        Confinement::Sandboxed
    } else {
        Confinement::Unsandboxed
    };
    let command = command.trim();
    if command.is_empty() {
        return RunOutcome {
            confinement,
            exit_code: None,
            timed_out: false,
            output: "no command given".to_string(),
        };
    }

    let home = env_or("HOME", "/tmp");
    let cargo_home = env_or("CARGO_HOME", &format!("{home}/.cargo"));
    let rustup_home = env_or("RUSTUP_HOME", &format!("{home}/.rustup"));

    // Wall-clock guard via coreutils `timeout` when present (SIGTERM at the
    // deadline, SIGKILL after a grace window). --die-with-parent then reaps the
    // sandbox children.
    let mut argv: Vec<String> = Vec::new();
    if probe("timeout") {
        argv.push("timeout".to_string());
        argv.push(format!("--kill-after={KILL_GRACE_SECS}s"));
        argv.push(format!("{}s", timeout_secs()));
    }

    let mut cmd;
    if use_bwrap {
        // HOME points at the in-sandbox tmpfs so host dotfiles are unreachable;
        // CARGO_HOME/RUSTUP_HOME stay the real (read-only bound) paths.
        let env = safe_env("/tmp", &cargo_home, &rustup_home);
        argv.push("bwrap".to_string());
        argv.extend(bwrap_args(
            root,
            network,
            &env,
            &cargo_home,
            &rustup_home,
            command,
        ));
        let (prog, rest) = argv.split_first().expect("argv is non-empty");
        cmd = Command::new(prog);
        cmd.args(rest);
        // bwrap isolates the child env via --clearenv, so the outer env is moot.
    } else {
        // No filesystem or network isolation is possible without bwrap. Still
        // clear the environment so secrets do not leak, pin the cwd to the
        // project root, and run through sh -c.
        argv.push("sh".to_string());
        argv.push("-c".to_string());
        argv.push(command.to_string());
        let (prog, rest) = argv.split_first().expect("argv is non-empty");
        cmd = Command::new(prog);
        cmd.args(rest);
        cmd.current_dir(root);
        cmd.env_clear();
        for (k, v) in safe_env(&home, &cargo_home, &rustup_home) {
            cmd.env(k, v);
        }
    }
    cmd.stdin(Stdio::null());

    match cmd.output() {
        Ok(out) => {
            let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
            if !out.stderr.is_empty() {
                if !combined.is_empty() && !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&String::from_utf8_lossy(&out.stderr));
            }
            let exit_code = out.status.code();
            RunOutcome {
                confinement,
                exit_code,
                timed_out: exit_code == Some(TIMEOUT_EXIT_CODE),
                output: cap_output(&combined),
            }
        }
        Err(e) => RunOutcome {
            confinement,
            exit_code: None,
            timed_out: false,
            output: format!("failed to launch command: {e}"),
        },
    }
}

/// Bound a command's output: keep a head and a tail, elide the middle, then
/// enforce a hard character cap as a backstop. Keeps the model's context and
/// the transcript from being flooded by a noisy build.
fn cap_output(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let mut capped = if lines.len() > HEAD_LINES + TAIL_LINES {
        let omitted = lines.len() - HEAD_LINES - TAIL_LINES;
        let mut out = lines[..HEAD_LINES].join("\n");
        out.push_str(&format!("\n... ({omitted} lines omitted) ...\n"));
        out.push_str(&lines[lines.len() - TAIL_LINES..].join("\n"));
        out
    } else {
        s.to_string()
    };
    if capped.chars().count() > OUTPUT_CHAR_CAP {
        capped = capped.chars().take(OUTPUT_CHAR_CAP).collect::<String>();
        capped.push_str("\n... (output truncated) ...");
    }
    capped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_env_is_a_fixed_whitelist_with_no_secret_keys() {
        // safe_env builds a fixed whitelist; it never reads arbitrary parent
        // vars, so a key like COXN_MODEL_KEY can never appear by construction.
        let env = safe_env("/tmp", "/home/u/.cargo", "/home/u/.rustup");
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"PATH"));
        assert!(keys.contains(&"HOME"));
        assert!(keys.contains(&"CARGO_HOME"));
        // No whitelisted key looks like a credential.
        for (k, _) in &env {
            let upper = k.to_uppercase();
            assert!(
                !upper.contains("KEY")
                    && !upper.contains("TOKEN")
                    && !upper.contains("SECRET")
                    && !upper.contains("PASSWORD"),
                "credential-shaped key in whitelist: {k}"
            );
        }
        // HOME is the value we asked for; toolchain dirs are the real ones.
        assert_eq!(env.iter().find(|(k, _)| k == "HOME").unwrap().1, "/tmp");
        assert_eq!(
            env.iter().find(|(k, _)| k == "CARGO_HOME").unwrap().1,
            "/home/u/.cargo"
        );
    }

    #[test]
    fn bwrap_args_confine_root_and_clear_env() {
        let env = safe_env("/tmp", "/c", "/r");
        let args = bwrap_args(Path::new("/proj"), false, &env, "/c", "/r", "echo hi");
        let joined = args.join(" ");
        // Fresh namespaces, no network by default, cleared env, new session.
        assert!(args.contains(&"--unshare-all".to_string()));
        assert!(!args.contains(&"--share-net".to_string()));
        assert!(args.contains(&"--clearenv".to_string()));
        assert!(args.contains(&"--new-session".to_string()));
        assert!(args.contains(&"--die-with-parent".to_string()));
        // The project root is the read-write bind, and the cwd.
        assert!(joined.contains("--bind /proj /proj"));
        assert!(joined.contains("--chdir /proj"));
        // The payload runs via sh -c and is the final argument.
        assert_eq!(args.last().unwrap(), "echo hi");
        let dashdash = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(&args[dashdash + 1..dashdash + 3], &["sh", "-c"]);
    }

    #[test]
    fn bwrap_args_share_net_only_when_requested() {
        let env = safe_env("/tmp", "/c", "/r");
        let on = bwrap_args(Path::new("/p"), true, &env, "/c", "/r", "x");
        assert!(on.contains(&"--share-net".to_string()));
        let off = bwrap_args(Path::new("/p"), false, &env, "/c", "/r", "x");
        assert!(!off.contains(&"--share-net".to_string()));
    }

    #[test]
    fn cap_output_keeps_short_output_and_elides_long() {
        assert_eq!(cap_output("a\nb\nc"), "a\nb\nc");
        let many = (0..1000)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let capped = cap_output(&many);
        assert!(capped.contains("lines omitted"));
        assert!(capped.starts_with("0\n1\n"));
        assert!(capped.trim_end().ends_with("999"));
        // The omitted count is total minus head minus tail.
        let omitted = 1000 - HEAD_LINES - TAIL_LINES;
        assert!(capped.contains(&format!("({omitted} lines omitted)")));
    }

    #[test]
    fn fallback_run_clears_the_parent_environment() {
        // The fallback path needs no bwrap, so it is portable. Prove env_clear
        // by picking a real parent var that is NOT whitelisted and that `sh`
        // will not re-add, then confirming the command cannot see it. No
        // set_var: mutating process env races other tests under cargo's runner.
        let whitelist = [
            "PATH",
            "HOME",
            "USER",
            "TERM",
            "LANG",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "RUSTC_WRAPPER",
            "RUSTUP_TOOLCHAIN",
            "CARGO_TARGET_DIR",
        ];
        let sh_readds = ["PWD", "SHLVL", "_", "OLDPWD", "IFS", "PS1", "PS2", "PS4"];
        let probe = std::env::vars().map(|(k, _)| k).find(|k| {
            !whitelist.contains(&k.as_str())
                && !sh_readds.contains(&k.as_str())
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !k.is_empty()
        });
        let Some(var) = probe else {
            return; // nothing non-whitelisted present to prove against
        };
        let dir = std::env::temp_dir();
        let out = run(
            &dir,
            &format!("printf '%s' \"${{{var}:-cleared}}\""),
            false,
            false,
        );
        assert_eq!(out.confinement, Confinement::Unsandboxed);
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(
            out.output, "cleared",
            "parent var {var} leaked into the command"
        );
    }

    #[test]
    fn fallback_run_reports_nonzero_exit() {
        let dir = std::env::temp_dir();
        let out = run(&dir, "exit 3", false, false);
        assert_eq!(out.exit_code, Some(3));
        assert!(!out.timed_out);
    }

    #[test]
    fn sandboxed_run_confines_to_project_root_when_bwrap_present() {
        // Only meaningful where bwrap actually runs; skip otherwise.
        if !bwrap_available() {
            return;
        }
        // A real project root: a unique subdir bound read-write into the sandbox.
        let dir = std::env::temp_dir().join(format!("coxn-sbx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create root");

        // A read-only bind (/usr) cannot be written, and the host is untouched.
        let ro = run(
            &dir,
            "echo x > /usr/coxn_probe 2>/dev/null && echo WROTE || echo BLOCKED",
            false,
            true,
        );
        assert_eq!(ro.confinement, Confinement::Sandboxed);
        assert!(
            ro.output.contains("BLOCKED"),
            "ro-bind not enforced: {}",
            ro.output
        );
        assert!(!std::path::Path::new("/usr/coxn_probe").exists());

        // The project root IS writable and the write persists to the host.
        let probe = dir.join("rw_probe");
        let _ = std::fs::remove_file(&probe);
        let rw = run(&dir, "echo ok > rw_probe", false, true);
        assert_eq!(rw.exit_code, Some(0), "{}", rw.output);
        assert!(probe.exists(), "project root not writable in sandbox");

        // Network is off by default inside the sandbox.
        let net = run(
            &dir,
            "getent hosts example.com >/dev/null 2>&1 && echo UP || echo DOWN",
            false,
            true,
        );
        assert!(
            net.output.contains("DOWN"),
            "network not blocked: {}",
            net.output
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
