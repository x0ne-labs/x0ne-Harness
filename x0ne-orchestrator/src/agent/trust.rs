//! The human gate, dispensed inside directories the operator has named.
//!
//! Designed a confirmation-free mode and set its rules: visible
//! while active, not inherited by sub-agents, dispensing what stays inside the
//! operator's declared frontier and never the host, the network or the safety
//! net. This is that mode, **scoped to directories** — the shape the owner
//! asked for on 2026-08-17 (« free-running, but only in this directory »).
//!
//! What it decides: whether an action that `requires_approval` would gate may
//! run without asking. Nothing else. The runtime's own policy — allowlists,
//! path confinement, `deny_path_patterns` — is not consulted here and not
//! widened by this: an action dispensed here that the runtime refuses is still
//! refused.
//!
//! SECURITY: every test in this file that says « dispensed » has a twin that
//! says « not dispensed » one step outside the root, because a trust root that
//! leaks by one `..` is the host.

use serde_json::Value;
use std::path::{Component, Path, PathBuf};

use crate::config::TrustConfig;

/// Whether `action` may skip the human gate under `trust`.
///
/// `false` whenever in doubt: no roots, an unknown action type, a path that
/// does not resolve, a composite with one child that is not dispensable.
pub fn dispensed(action: &Value, trust: &TrustConfig) -> bool {
    if trust.roots.is_empty() {
        return false;
    }
    let roots: Vec<PathBuf> = trust.roots.iter().filter_map(|r| resolve_root(r)).collect();
    if roots.is_empty() {
        return false;
    }
    let gate = Gate {
        roots: &roots,
        workdir_programs: &trust.workdir_programs,
    };
    dispensed_under(action, &gate, 0)
}

/// What a trust root grants: its directories, and the extra programs the
/// operator named in `[trust] workdir_programs`.
struct Gate<'a> {
    roots: &'a [PathBuf],
    workdir_programs: &'a [String],
}

/// Composites recurse into their children (invariant 4: a composite is worth
/// what it contains); `depth` caps the recursion the way the runtime does.
fn dispensed_under(action: &Value, gate: &Gate, depth: usize) -> bool {
    let roots = gate.roots;
    if depth > 3 {
        return false;
    }
    let Some(action_type) = action.get("type").and_then(|v| v.as_str()) else {
        return false;
    };
    let s = |key: &str| action.get(key).and_then(|v| v.as_str()).unwrap_or("");
    match action_type {
        // ── Filesystem: every path under a root ──────────────────────────
        "ReadFile" | "WriteFile" | "EditFile" | "ApplyPatch" | "MoveToTrash" | "FileInfo"
        | "StatFiles" | "Diff" | "Checksum" | "SearchCode" | "SearchAndRead" | "ReadLogs" => {
            let mut paths: Vec<&str> = Vec::new();
            for key in ["path", "root", "dir", "directory", "a", "b"] {
                let p = s(key);
                if !p.is_empty() {
                    paths.push(p);
                }
            }
            if let Some(arr) = action.get("paths").and_then(|v| v.as_array()) {
                paths.extend(arr.iter().filter_map(|v| v.as_str()));
            }
            !paths.is_empty() && paths.iter().all(|p| under_any(p, roots))
        }
        "ReadFiles" => match action.get("paths").and_then(|v| v.as_array()) {
            Some(arr) if !arr.is_empty() => arr
                .iter()
                .all(|v| v.as_str().map(|p| under_any(p, roots)).unwrap_or(false)),
            _ => false,
        },
        "PatchSet" => match action.get("patches").and_then(|v| v.as_array()) {
            Some(arr) if !arr.is_empty() => arr.iter().all(|p| {
                p.get("path")
                    .and_then(|v| v.as_str())
                    .map(|p| under_any(p, roots))
                    .unwrap_or(false)
            }),
            _ => false,
        },
        // ── A process: its working directory under a root, and no argument
        // that names a place outside it. `env` that chooses the binary is
        // refused by the runtime before this matters (`env_keys`); still, an
        // action with any `env` is not dispensed — the gate is where an
        // operator learns a program is being reached for with a changed
        // environment.
        "RunCommand" | "SafeTest" => {
            if action
                .get("env")
                .and_then(|v| v.as_object())
                .map(|o| !o.is_empty())
                .unwrap_or(false)
            {
                return false;
            }
            let cwd = s("cwd");
            let cwd_ok = if cwd.is_empty() {
                std::env::current_dir()
                    .map(|d| under_any_path(&d, roots))
                    .unwrap_or(false)
            } else {
                under_any(cwd, roots)
            };
            if !cwd_ok {
                return false;
            }
            let program = s("command");
            let args: Vec<&str> = action
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            let listed = match builtin_verdict(program, &args) {
                Some(true) => false,
                Some(false) => return false,
                None if workdir_program_listed(program, gate.workdir_programs) => true,
                None => return false,
            };
            let args_ok = args.iter().all(|arg| arg_stays_inside(arg, roots))
                && (!listed || args.iter().all(|arg| words_stay_inside(arg, roots)));
            let path_ok = s("path").is_empty() || under_any(s("path"), roots);
            args_ok && path_ok
        }
        // ── Project memory: the one file it appends to, under a root ──
        "ProjectNote" => crate::project::project_note_path()
            .to_str()
            .map(|p| under_any(p, roots))
            .unwrap_or(false),
        // ── A live process: it was gated when it was started ─────────────
        "SendInput" | "StopHandle" | "CheckHandle" => !s("handle").is_empty(),
        // ── Composites: worth what they contain ──────────────────────────
        "Pipeline" | "Plan" | "Try" | "SafeChange" => {
            let children = action
                .get("steps")
                .or_else(|| action.get("actions"))
                .and_then(|v| v.as_array());
            match children {
                Some(c) if !c.is_empty() => c.iter().all(|a| dispensed_under(a, gate, depth + 1)),
                _ => false,
            }
        }
        "If" => ["then", "else", "then_action", "else_action"]
            .iter()
            .filter_map(|k| action.get(*k))
            .filter(|v| !v.is_null())
            .all(|a| dispensed_under(a, gate, depth + 1)),
        // ── Everything else: network, desktop, host, Incus, snapshots,
        // artifacts, budget, memory, unknown — the gate stays. Deliberately
        // not a catch-all: a new action type is asked about until someone
        // decides it may not be.
        _ => false,
    }
}

/// Programs a trust root dispenses, and the flags that turn them into
/// something else.
///
/// SECURITY: a closed list, on purpose. Trust means « stays inside the
/// directory », and a program is only as inside as its arguments let it be:
/// `python3 -c "open('/etc/x','w')"` writes wherever the code says, `curl -d
/// @secret http://…` leaves the machine, `git push` too, `find -exec rm` runs
/// a second program. So: only these programs, and for each the flags that
/// smuggle a program or a destination are refused (review of 2026-08-17:
/// « argv flag smuggling in confirmation-free mode »). A shell is
/// never here — a granted shell is a granted everything. Anything
/// not listed keeps the gate: the runtime allowlist decides whether it may
/// run at all, this only decides whether a person is asked.
///
/// `cargo test`, `pytest` and `python3 script.py` DO execute code that could touch the
/// host; that is what the operator accepts by naming a trust root, and it is
/// the same acceptance the « allow once » would have been. The confinement of
/// running code is the sandbox's job (Incus), not the gate's.
///
/// `Some(verdict)` for a program the list knows, `None` for one it does not.
/// A known program is never re-decided by `workdir_programs`: listing
/// `python3` there must not lift the `-c` refusal.
fn builtin_verdict(program: &str, args: &[&str]) -> Option<bool> {
    // The program itself: a bare name (resolved by PATH), an absolute path, or
    // exactly `./check.sh`. A relative path with directories (`./sub/cargo`,
    // `../x/python3`) is a local file wearing a trusted name — gate.
    if program.contains('/') && !program.starts_with('/') && program != "./check.sh" {
        return Some(false);
    }
    let name = program.rsplit('/').next().unwrap_or(program);
    let has = |flags: &[&str]| args.iter().any(|a| flags.contains(a));
    let starts = |prefixes: &[&str]| {
        args.iter()
            .any(|a| prefixes.iter().any(|p| a.starts_with(p)))
    };
    // A short-flag cluster carrying one of `letters`: `-Sc`, `-pe`, `-mfoo`.
    // Interpreters accept combined flags, so `-c` alone is not the check.
    let short_cluster_has = |letters: &[char]| {
        args.iter().any(|a| {
            a.len() > 1
                && a.starts_with('-')
                && !a.starts_with("--")
                && a[1..].chars().any(|ch| letters.contains(&ch))
        })
    };
    let verdict = match name {
        // Build & test tools: no alternate manifests / directories / config,
        // no crates.io side. `-Z` unstable knobs can point at other rustcs.
        "cargo" => {
            !has(&["install", "publish", "login", "owner", "yank"])
                && !starts(&["--manifest-path", "--target-dir", "--config", "-Z"])
        }
        "rustc" | "make" => true,
        // Interpreters: a script file inside, never inline code, never a
        // module by name (`-m` reaches PATH-installed modules), never stdin.
        "python3" | "python" => !short_cluster_has(&['c', 'm']) && !has(&["-"]),
        // pytest: test files inside, like `cargo test`. Never a module by
        // name (`-p`, `--pyargs`, `-W`, `--pdbcls`), another ini (`-c`, `-o`),
        // an `@file` of arguments, or `--basetemp` (a directory it deletes first).
        "pytest" => {
            !short_cluster_has(&['p', 'c', 'o', 'W'])
                && !has(&["--pyargs"])
                && !starts(&[
                    "--config-file",
                    "--override-ini",
                    "--pythonwarnings",
                    "--pdbcls",
                    "--basetemp",
                    "@",
                ])
        }
        "node" => {
            !short_cluster_has(&['e', 'p', 'r', 'i'])
                && !starts(&[
                    "--eval",
                    "--print",
                    "--require",
                    "--import",
                    "--input-type",
                    "-",
                ])
        }
        // git: local verbs only, no remotes, no other tree, no `-c key=value`
        // (a pager or a helper is a program), no exec path.
        "git" => {
            !has(&[
                "push",
                "pull",
                "fetch",
                "clone",
                "remote",
                "submodule",
                "config",
                "-C",
                "-c",
                "--git-dir",
                "--work-tree",
                "--exec-path",
                "--config-env",
            ]) && !starts(&[
                "--git-dir=",
                "--work-tree=",
                "--exec-path=",
                "--config-env=",
                "-C",
                "-c",
            ])
        }
        // find: no `-exec`/`-delete`/`-ok` (a second program, or a deletion).
        "find" => !has(&[
            "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprintf", "-fls",
            "-fprint0",
        ]),
        // Read-only observers. `env` is NOT here: `env sh -c …` runs a
        // program. `sort -o`/`tee` write where their argument says — the
        // argument check above keeps that inside.
        "ls" | "cat" | "head" | "tail" | "wc" | "grep" | "sort" | "uniq" | "diff" | "file"
        | "stat" | "echo" | "true" | "false" | "pwd" | "which" | "ss" | "tr" | "cut" => true,
        // The exercises' own graders.
        "check.sh" => program == "./check.sh" || program == "check.sh",
        // Everything else — shells, curl/wget/ssh/nc, rm, chmod, sudo, env,
        // an unknown tool — keeps the gate unless `workdir_programs` names it.
        _ => return None,
    };
    Some(verdict)
}

#[cfg(test)]
fn program_stays_inside(program: &str, args: &[&str]) -> bool {
    builtin_verdict(program, args).unwrap_or(false)
}

/// Never dispensed, even when named in `workdir_programs`: a shell, a launcher
/// of another program, an inline-code interpreter, the network, a package
/// manager. A granted shell is a granted everything.
const NEVER_WORKDIR_PROGRAMS: &[&str] = &[
    "sh",
    "bash",
    "dash",
    "zsh",
    "ksh",
    "mksh",
    "csh",
    "tcsh",
    "fish",
    "busybox",
    "env",
    "sudo",
    "su",
    "doas",
    "pkexec",
    "xargs",
    "nohup",
    "timeout",
    "nice",
    "ionice",
    "setsid",
    "stdbuf",
    "chroot",
    "unshare",
    "nsenter",
    "systemd-run",
    "strace",
    "ltrace",
    "gdb",
    "script",
    "expect",
    "watch",
    "flock",
    "parallel",
    "perl",
    "ruby",
    "php",
    "lua",
    "luajit",
    "tclsh",
    "wish",
    "awk",
    "gawk",
    "mawk",
    "nawk",
    "sed",
    "vim",
    "vi",
    "nvim",
    "emacs",
    "ed",
    "ex",
    "less",
    "more",
    "ssh",
    "scp",
    "sftp",
    "rsync",
    "curl",
    "wget",
    "nc",
    "ncat",
    "netcat",
    "socat",
    "telnet",
    "ftp",
    "tftp",
    "podman",
    "docker",
    "incus",
    "lxc",
    "kubectl",
    "pip",
    "pip3",
    "npm",
    "npx",
    "uv",
    "uvx",
    "apt",
    "apt-get",
    "dpkg",
];

/// Shells among [`NEVER_WORKDIR_PROGRAMS`].
const SHELLS: &[&str] = &[
    "sh", "bash", "dash", "zsh", "ksh", "mksh", "csh", "tcsh", "fish", "busybox",
];

/// What granting `program` to `allowed_commands` really grants, for the
/// policy-lift prompt, or `None` for an ordinary program. Same list as
/// the trust roots: what is never dispensed here is never one command there.
pub fn grant_warning(program: &str) -> Option<String> {
    let name = program.rsplit('/').next().unwrap_or(program);
    if SHELLS.contains(&name) {
        return Some(format!(
            "WARNING: a granted shell is a granted everything: '{name}' runs any command \
             line, past every other entry of allowed_commands"
        ));
    }
    NEVER_WORKDIR_PROGRAMS.contains(&name).then(|| {
        format!(
            "WARNING: '{name}' runs other programs, code or the network: granting it \
             grants more than one command"
        )
    })
}

/// `program` (bare name or absolute path) is named in `[trust]
/// workdir_programs` and is not one the list may never open.
fn workdir_program_listed(program: &str, listed: &[String]) -> bool {
    if listed.is_empty() || (program.contains('/') && !program.starts_with('/')) {
        return false;
    }
    let name = program.rsplit('/').next().unwrap_or(program);
    !name.is_empty() && !NEVER_WORKDIR_PROGRAMS.contains(&name) && listed.iter().any(|p| p == name)
}

/// A listed program may read a script out of one argument
/// (`sqlite3 db ".shell rm -rf /"`, `.output <path>`). Every word of every
/// argument is held to the same rule as an argument.
fn words_stay_inside(arg: &str, roots: &[PathBuf]) -> bool {
    arg.split_whitespace().all(|w| {
        let w = w.trim_matches(|c: char| "'\"`();,".contains(c));
        w.is_empty() || arg_stays_inside(w, roots)
    })
}

/// An argument that is a path must stay inside; one that merely looks like a
/// word is fine. `..` anywhere is not fine: `a/../../etc` is a word too.
fn arg_stays_inside(arg: &str, roots: &[PathBuf]) -> bool {
    if arg.split('/').any(|seg| seg == "..") {
        return false;
    }
    if arg.starts_with('/') || arg.starts_with('~') {
        return under_any(arg, roots);
    }
    // `--output=/etc/x` and `-o/etc/x` carry a destination glued to the
    // flag; `sort` and `git diff` wrote outside the root ungated. Fail closed.
    // Merge 2026-10-11: `-o../x` and `-o/etc/x=y` leaked through the first cut.
    if let Some(flag) = arg.strip_prefix('-') {
        let mut glued: Vec<&str> = Vec::new();
        if let Some((_, value)) = arg.split_once('=') {
            glued.push(value);
        }
        if !flag.starts_with('-') {
            let value = flag.trim_start_matches(|c: char| c.is_ascii_alphanumeric());
            if !value.is_empty() && value.len() < flag.len() {
                glued.push(value);
            }
        }
        for value in glued {
            if value.split('/').any(|seg| seg == "..") {
                return false;
            }
            if (value.starts_with('/') || value.starts_with('~')) && !under_any(value, roots) {
                return false;
            }
        }
    }
    true
}

/// `raw` (relative to the working directory, or `~`-prefixed, or absolute)
/// resolves under one of `roots`. Resolution is lexical then canonical when
/// the path exists — a symlink that points out is followed and refused.
fn under_any(raw: &str, roots: &[PathBuf]) -> bool {
    let Some(p) = resolve_path(raw) else {
        return false;
    };
    under_any_path(&p, roots)
}

fn under_any_path(p: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| p.starts_with(r))
}

fn resolve_root(raw: &str) -> Option<PathBuf> {
    let p = resolve_path(raw)?;
    // A root must exist and be a directory: a typo must not silently trust
    // nothing-that-exists-yet-but-will.
    p.is_dir().then_some(p)
}

/// Expand `~`, make absolute against the working directory, normalise `.` and
/// `..` lexically, then canonicalise as far as the path exists (so a symlink
/// inside the root pointing outside resolves outside).
fn resolve_path(raw: &str) -> Option<PathBuf> {
    if raw.is_empty() {
        return None;
    }
    let expanded: PathBuf = if let Some(rest) = raw.strip_prefix("~/") {
        dirs::home_dir()?.join(rest)
    } else if raw == "~" {
        dirs::home_dir()?
    } else {
        PathBuf::from(raw)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().ok()?.join(expanded)
    };
    let mut lexical = PathBuf::new();
    for c in absolute.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                lexical.pop();
            }
            other => lexical.push(other.as_os_str()),
        }
    }
    // Canonicalise the longest existing prefix, keep the rest lexical.
    let mut existing = lexical.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                existing.pop();
            }
            None => break,
        }
    }
    let mut out = std::fs::canonicalize(&existing).unwrap_or(existing);
    for name in tail.iter().rev() {
        out.push(name);
    }
    Some(out)
}

/// One line for banners: « trust: `<root>, <root>` » — or `None`.
pub fn label(trust: &TrustConfig) -> Option<String> {
    if trust.roots.is_empty() {
        return None;
    }
    Some(format!("trust: {}", trust.roots.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_root() -> (tempfile::TempDir, TrustConfig) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("inside")).unwrap();
        let cfg = TrustConfig {
            roots: vec![dir.path().to_string_lossy().to_string()],
            ..Default::default()
        };
        (dir, cfg)
    }

    /// A terminal may hold a shell: typing into it is never dispensed, even
    /// with a cwd inside a root.
    #[test]
    fn terminal_input_is_never_dispensed() {
        let (dir, cfg) = tmp_root();
        for a in [
            json!({"type": "TermSend", "name": "sh", "text": "ls", "keys": ["Enter"]}),
            json!({"type": "TermOpen", "name": "sh", "command": "bash", "cwd": dir.path()}),
        ] {
            assert!(!dispensed(&a, &cfg), "{a}");
        }
    }

    /// No roots: nothing is dispensed. The default configuration changes
    /// nothing about the gate — a failure would mean trust is on by default.
    #[test]
    fn no_roots_dispenses_nothing() {
        let a = json!({"type": "WriteFile", "path": "/tmp/x", "content": ""});
        assert!(!dispensed(&a, &TrustConfig::default()));
    }

    /// A write inside the root is dispensed; the same write one directory up
    /// is not; a `..` that lexically escapes is not.
    #[test]
    fn a_write_inside_is_dispensed_and_one_step_outside_is_not() {
        let (dir, cfg) = tmp_root();
        let inside = dir.path().join("inside/f.txt");
        assert!(dispensed(
            &json!({"type": "WriteFile", "path": inside, "content": "x"}),
            &cfg
        ));
        let outside = dir.path().parent().unwrap().join("f.txt");
        assert!(!dispensed(
            &json!({"type": "WriteFile", "path": outside, "content": "x"}),
            &cfg
        ));
        let escape = dir.path().join("inside/../../escape.txt");
        assert!(!dispensed(
            &json!({"type": "WriteFile", "path": escape, "content": "x"}),
            &cfg
        ));
    }

    /// A symlink inside the root pointing outside resolves outside.
    #[cfg(unix)]
    #[test]
    fn a_symlink_that_points_out_is_not_dispensed() {
        let (dir, cfg) = tmp_root();
        let link = dir.path().join("inside/link");
        std::os::unix::fs::symlink("/etc/hostname", &link).unwrap();
        assert!(!dispensed(&json!({"type": "ReadFile", "path": link}), &cfg));
    }

    /// A command is dispensed when its cwd is inside and its arguments stay
    /// inside; a shell never is; an absolute argument outside is not; `env`
    /// keeps the gate.
    #[test]
    fn a_command_is_dispensed_by_cwd_and_arguments_and_a_shell_never_is() {
        let (dir, cfg) = tmp_root();
        let cwd = dir.path().join("inside");
        let ok = json!({"type": "RunCommand", "command": "cargo", "args": ["test"], "cwd": cwd});
        assert!(dispensed(&ok, &cfg));
        let shell =
            json!({"type": "RunCommand", "command": "bash", "args": ["check.sh"], "cwd": cwd});
        assert!(!dispensed(&shell, &cfg));
        let outside_arg =
            json!({"type": "RunCommand", "command": "cat", "args": ["/etc/passwd"], "cwd": cwd});
        assert!(!dispensed(&outside_arg, &cfg));
        let dotdot =
            json!({"type": "RunCommand", "command": "cat", "args": ["../../x"], "cwd": cwd});
        assert!(!dispensed(&dotdot, &cfg));
        let with_env = json!({"type": "RunCommand", "command": "cargo", "args": ["test"], "cwd": cwd, "env": {"RUSTFLAGS": "-C x"}});
        assert!(!dispensed(&with_env, &cfg));
        let outside_cwd =
            json!({"type": "RunCommand", "command": "cargo", "args": ["test"], "cwd": "/tmp"});
        assert!(!dispensed(&outside_cwd, &cfg));
    }

    /// Flag smuggling: an inline-code interpreter, a git verb that reaches a
    /// remote, a `find -exec`, and any program off the list keep the gate,
    /// while the plain dev tools inside are dispensed. A failure would let a
    /// dispensed `python3 -c` write anywhere on the host from inside a trust
    /// root (review of 2026-08-17).
    #[test]
    fn a_flag_that_smuggles_a_program_or_a_destination_keeps_the_gate() {
        let (dir, cfg) = tmp_root();
        let cwd = dir.path().join("inside");
        let cmd = |program: &str, args: &[&str]| json!({"type": "RunCommand", "command": program, "args": args, "cwd": cwd});
        for kept in [
            cmd("python3", &["-c", "open('/etc/x','w')"]),
            cmd("python3", &["-m", "http.server"]),
            cmd("node", &["-e", "1"]),
            cmd("git", &["push", "origin", "main"]),
            cmd("git", &["-C", "/etc", "status"]),
            cmd("git", &["--git-dir=/etc/.git", "log"]),
            cmd("find", &[".", "-exec", "rm", "{}", ";"]),
            cmd("find", &[".", "-delete"]),
            cmd("cargo", &["install", "evil"]),
            cmd("cargo", &["test", "--manifest-path", "x/Cargo.toml"]),
            cmd("curl", &["http://127.0.0.1:8765/ping"]),
            cmd("wget", &["http://x/"]),
            cmd("rm", &["-rf", "."]),
            cmd("sudo", &["ls"]),
            // second wave (review said « incomplete »): combined short flags,
            // git -c, env as a launcher, a local file wearing a trusted name
            cmd("python3", &["-Sc", "print(1)"]),
            cmd("python3", &["-mhttp.server"]),
            cmd("python3", &["-"]),
            cmd("node", &["-pe", "1"]),
            cmd("git", &["-c", "core.pager=evil", "log"]),
            cmd("git", &["-cfoo=bar", "log"]),
            cmd("env", &["sh", "-c", "id"]),
            cmd("./sub/cargo", &["test"]),
            cmd("../evil/python3", &["x.py"]),
            cmd("cargo", &["-Zunstable-options", "test"]),
        ] {
            assert!(!dispensed(&kept, &cfg), "must keep the gate: {kept}");
        }
        for free in [
            cmd("cargo", &["test", "--", "--nocapture"]),
            cmd("python3", &["test_wordcount.py"]),
            cmd("git", &["status"]),
            cmd("git", &["diff", "--stat"]),
            cmd("find", &[".", "-name", "*.rs"]),
            cmd("./check.sh", &[]),
            cmd("grep", &["-n", "ERROR", "build.log"]),
            cmd("/usr/bin/python3", &["-u", "server.py", "8765"]),
        ] {
            assert!(dispensed(&free, &cfg), "must be dispensed: {free}");
        }
    }

    /// 2026-10-16: four `-p` runs under the Developer profile could not
    /// run a test. pytest is dispensed like `cargo test`; what loads code by
    /// name or reads more arguments keeps the gate; `python3 -m` stays refused.
    #[test]
    fn pytest_is_dispensed_and_what_loads_code_by_name_is_not() {
        let (dir, cfg) = tmp_root();
        let cwd = dir.path().join("inside");
        let cmd = |program: &str, args: &[&str]| json!({"type": "RunCommand", "command": program, "args": args, "cwd": cwd});
        for free in [
            cmd("pytest", &[]),
            cmd("pytest", &["-q", "-x", "tests/test_parse.py"]),
            cmd("pytest", &["-k", "not slow", "-vv"]),
            cmd("/usr/bin/pytest", &["--tb=short"]),
            cmd("cargo", &["test"]),
        ] {
            assert!(dispensed(&free, &cfg), "must be dispensed: {free}");
        }
        for kept in [
            cmd("python3", &["-m", "pytest"]),
            cmd("python3", &["-mpytest"]),
            cmd("python3", &["-c", "import pytest"]),
            cmd("pytest", &["-p", "evil_plugin"]),
            cmd("pytest", &["-pevil_plugin"]),
            cmd("pytest", &["-xp", "evil_plugin"]),
            cmd("pytest", &["--pyargs", "evil.tests"]),
            cmd("pytest", &["-c", "other.ini"]),
            cmd("pytest", &["--config-file=other.ini"]),
            cmd("pytest", &["-o", "addopts=-p evil"]),
            cmd("pytest", &["--override-ini=python_files=*.py"]),
            cmd("pytest", &["-W", "error::evil.Warning"]),
            cmd("pytest", &["--pythonwarnings=error::evil.Warning"]),
            cmd("pytest", &["--pdbcls=evil:Debugger"]),
            cmd("pytest", &["@args.txt"]),
            cmd("pytest", &["--basetemp=src"]),
            cmd("pytest", &["--basetemp", "src"]),
            cmd("pytest", &["/etc"]),
            cmd("./pytest", &[]),
        ] {
            assert!(!dispensed(&kept, &cfg), "must keep the gate: {kept}");
        }
        // Listing it does not lift its refusals: a known program is never
        // re-decided by `workdir_programs`.
        let listed = TrustConfig {
            workdir_programs: vec!["pytest".into()],
            ..cfg.clone()
        };
        assert!(!dispensed(&cmd("pytest", &["-p", "evil"]), &listed));
    }

    /// A composite is dispensed only if every child is (invariant 4); network,
    /// snapshots and Incus never are.
    #[test]
    fn a_composite_is_worth_its_children_and_the_host_is_never_dispensed() {
        let (dir, cfg) = tmp_root();
        let inside = dir.path().join("inside/f.txt");
        let good = json!({"type": "Pipeline", "steps": [
            {"type": "WriteFile", "path": inside, "content": "a"},
            {"type": "MoveToTrash", "path": inside}
        ]});
        assert!(dispensed(&good, &cfg));
        let mixed = json!({"type": "Pipeline", "steps": [
            {"type": "WriteFile", "path": inside, "content": "a"},
            {"type": "WriteFile", "path": "/tmp/out.txt", "content": "a"}
        ]});
        assert!(!dispensed(&mixed, &cfg));
        for never in [
            json!({"type": "HttpRequest", "method": "POST", "url": "http://x/"}),
            json!({"type": "RestoreSnapshot", "name": "s"}),
            json!({"type": "DeleteSnapshot", "name": "s"}),
            json!({"type": "RunIncus", "image": "i", "command": "ls"}),
            json!({"type": "ArtifactExpose", "id": "a"}),
            json!({"type": "Budget", "usd": 5}),
            json!({"type": "Click", "x": 1, "y": 1}),
        ] {
            assert!(!dispensed(&never, &cfg), "{never}");
        }
    }

    /// Bench 2026-10-11: the arguments stay inside; the program is
    /// what keeps the gate.
    #[test]
    fn nasm_is_refused_on_the_program_not_on_its_arguments() {
        let (dir, _cfg) = tmp_root();
        let roots = vec![resolve_root(&dir.path().to_string_lossy()).unwrap()];
        let args = ["-f", "elf64", "p01.s", "-o", "p01.o"];
        assert!(args.iter().all(|a| arg_stays_inside(a, &roots)));
        assert!(!program_stays_inside("nasm", &args));
        assert!(!program_stays_inside("ld", &["p01.o", "-o", "prog"]));
        assert!(program_stays_inside("python3", &["fib.py"]));
    }

    /// A destination glued to its flag is a path like any other; the
    /// same destination inside the root is still dispensed.
    #[test]
    fn a_destination_glued_to_its_flag_keeps_the_gate_outside_the_root() {
        let (dir, cfg) = tmp_root();
        let cwd = dir.path().join("inside");
        let cmd = |program: &str, args: &[&str]| json!({"type": "RunCommand", "command": program, "args": args, "cwd": cwd});
        let mut leaked = Vec::new();
        for a in [
            cmd("sort", &["-o/etc/x", "f"]),
            cmd("sort", &["--output=/etc/x", "f"]),
            cmd("git", &["diff", "--output=/etc/x"]),
            cmd("sort", &["-o", "/etc/x", "f"]),
            cmd("sort", &["--output=../x", "f"]),
            cmd("sort", &["-o../x", "f"]),
            cmd("sort", &["-o/etc/x=y", "f"]),
        ] {
            if dispensed(&a, &cfg) {
                leaked.push(a.to_string());
            }
        }
        assert!(leaked.is_empty(), "dispensed: {leaked:#?}");
        let inside = dir.path().join("inside/out.txt");
        let inside = inside.to_string_lossy();
        for a in [
            cmd("sort", &[&format!("-o{inside}"), "f"]),
            cmd("sort", &[&format!("--output={inside}"), "f"]),
            cmd("sort", &["-o./out.txt", "f"]),
            cmd("grep", &["--color=never", "x", "f"]),
        ] {
            assert!(dispensed(&a, &cfg), "must be dispensed: {a}");
        }
    }

    /// Bench 2026-10-11: 29/45 headless runs stopped on gcc, chmod,
    /// mkdir, sqlite3 under the root. Listed in `[trust] workdir_programs`,
    /// they are dispensed while every path argument stays inside.
    #[test]
    fn workdir_programs_are_dispensed_inside_the_root_and_nowhere_else() {
        let (dir, mut cfg) = tmp_root();
        cfg.workdir_programs = ["gcc", "cc", "chmod", "mkdir", "sqlite3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let root = dir.path().to_string_lossy().to_string();
        let cwd = dir.path().join("inside");
        let cmd = |program: &str, args: &[&str]| json!({"type": "RunCommand", "command": program, "args": args, "cwd": cwd});
        let x = format!("{root}/x");
        let d = format!("{root}/d");
        let db = format!("{root}/db.sqlite");
        let glued = format!("-o{root}/x");
        let long = format!("--output={root}/x");
        for free in [
            cmd("gcc", &["-o", &x, "main.c"]),
            cmd("gcc", &[&glued, "main.c"]),
            cmd("cc", &[&long, "main.c"]),
            cmd("chmod", &["+x", &x]),
            cmd("mkdir", &["-p", &d]),
            cmd("sqlite3", &[&db, "SELECT 1;"]),
            cmd("/usr/bin/gcc", &["-O2", "-o", "prog", "prog.c"]),
        ] {
            assert!(dispensed(&free, &cfg), "must be dispensed: {free}");
        }
        let mut leaked = Vec::new();
        for kept in [
            cmd("gcc", &["-o", "/etc/x", "main.c"]),
            cmd("gcc", &["-o/etc/x", "main.c"]),
            cmd("cc", &["--output=/etc/x", "main.c"]),
            cmd("gcc", &["-o../x", "main.c"]),
            cmd("chmod", &["+x", "/etc/passwd"]),
            cmd("chmod", &["777", "../../x"]),
            cmd("mkdir", &["/etc/d"]),
            cmd("sqlite3", &["/etc/db.sqlite", "SELECT 1;"]),
            cmd("sqlite3", &[&db, ".shell rm -rf /"]),
            cmd("sqlite3", &[&db, ".output /etc/x"]),
            cmd("./sub/gcc", &["main.c"]),
            cmd("nasm", &["-f", "elf64", "p01.s"]),
            cmd("gcc", &["main.c"])
                .as_object()
                .map(|o| {
                    let mut o = o.clone();
                    o.insert("cwd".into(), json!("/tmp"));
                    Value::Object(o)
                })
                .unwrap(),
        ] {
            if dispensed(&kept, &cfg) {
                leaked.push(kept.to_string());
            }
        }
        assert!(leaked.is_empty(), "dispensed: {leaked:#?}");
    }

    /// A shell or a launcher listed by mistake is still a granted everything:
    /// the list cannot open it.
    #[test]
    fn workdir_programs_cannot_list_a_shell_or_a_launcher() {
        let (dir, mut cfg) = tmp_root();
        cfg.workdir_programs = ["bash", "sh", "env", "sudo", "xargs", "curl", "timeout"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let cwd = dir.path().join("inside");
        for p in ["bash", "sh", "env", "sudo", "xargs", "curl", "timeout"] {
            let a = json!({"type": "RunCommand", "command": p, "args": ["x"], "cwd": cwd});
            assert!(!dispensed(&a, &cfg), "must keep the gate: {a}");
        }
    }

    /// Without the option, the same commands keep the gate: the default
    /// widens nothing.
    #[test]
    fn workdir_programs_default_to_none() {
        let (dir, cfg) = tmp_root();
        let cwd = dir.path().join("inside");
        let a = json!({"type": "RunCommand", "command": "gcc", "args": ["-o", "x", "main.c"], "cwd": cwd});
        assert!(!dispensed(&a, &cfg));
    }

    /// A root that does not exist trusts nothing (a typo is not a grant).
    #[test]
    fn a_missing_root_trusts_nothing() {
        let cfg = TrustConfig {
            roots: vec!["/definitely/not/here".into()],
            ..Default::default()
        };
        assert!(!dispensed(
            &json!({"type": "WriteFile", "path": "/definitely/not/here/x", "content": ""}),
            &cfg
        ));
    }
}
