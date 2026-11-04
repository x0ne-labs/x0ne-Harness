//! What a child process inherits from the runtime's own environment.
//!
//! Every process the runtime spawns goes through [`harden`]. One door, so that
//! the answer to "what does a command the model asked for get to see?" is a
//! list one can read, not the accident of whatever shell the operator happened
//! to launch `x0ne` from.
//!
//! Measured, 2026-08-14 audit (`AUDIT-2026-08-14.md`, section 4, "the central
//! central"): a canary planted in the operator's shell came straight back out.
//!
//! ```text
//! $ export X0NE_CANARY_TEST=fuite-1786762684
//! $ echo '{"type":"RunCommand","command":"printenv"}' | x0ne-runtime --mode stdio
//! ... X0NE_CANARY_TEST=fuite-1786762684 ... SSH_AUTH_SOCK=... CLAUDE_CODE_MESSAGING_TOKEN=...
//! ```
//!
//! That stdout is handed verbatim to the model (`x0ne-orchestrator`,
//! `conversation.rs:854`) and appended to the session log
//! (`x0ne-orchestrator`, `session.rs:100`), where it stays in clear text. Since
//! `~/.x0ne/.env` is loaded into the process by `dotenvy` (`x0ne-code`,
//! `main.rs:401`), every API key the operator owns was one `printenv` away from
//! the transcript. `printenv` is also on the read-only dispensation list
//! (`x0ne-orchestrator`, `agent/readonly_cmd.rs:63`), so it did not even stop
//! at the human gate.
//!
//! The fix is `env_clear()` plus this allowlist. It is the inverse of the usual
//! denylist reflex on purpose: a denylist has to predict the name of the next
//! secret, and it will not.

use std::sync::{OnceLock, RwLock};

/// The other half of the environment question, re-exported here so that one
/// module answers both: this file says what a child INHERITS from the operator
/// (an allowlist, because nobody can name the next secret), and
/// [`check_caller_env`] says what an ACTION may set on it (a closed denylist of
/// the variables that decide what executes — PATH, `LD_*`, and their family).
///
/// SECURITY: applying only one of the two is the 2026-08-15 defect. `harden`
/// cleared PATH and the action's own map put it back, unfiltered, one line
/// later (`executor/process.rs:228`).
pub use x0ne_actions::env_keys::{check_caller_env, is_safe_caller_env_key};

/// The variables a child may inherit, each with the thing that needs it.
///
/// SECURITY: this list is the entire boundary between the operator's shell and
/// a command the model chose. Anything not named here (or by
/// [`ENV_ALLOWLIST_PREFIXES`], or by `policy.env_passthrough`) does not exist
/// as far as the child is concerned. Adding an entry hands its value to every
/// process the agent can start, including through prompt injection — so the
/// second field is not decoration: an entry nobody can name a consumer for is
/// an entry that should not be here.
///
/// Deliberately absent, and each absence is a decision:
///
/// - `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `X0NE_BRAVE_API_KEY` and the rest
///   of `~/.x0ne/.env` — the runtime needs none of them; the orchestrator talks
///   to the models, the runtime executes.
/// - `GOOGLE_APPLICATION_CREDENTIALS` — a path to a service-account key. gcloud
///   finds its credentials under `HOME` (`~/.config/gcloud`) or
///   `CLOUDSDK_CONFIG`; this variable exists to point at a key file directly,
///   which is precisely what should not travel.
/// - `CLOUDSDK_AUTH_ACCESS_TOKEN` and `CLOUDSDK_AUTH_REFRESH_TOKEN` — bearer
///   tokens. Note the shape: `CLOUDSDK_*` is NOT a prefix entry for this
///   reason. gcloud's configuration namespace and its credential namespace
///   share a prefix, so a prefix rule here would have shipped the token with
///   the project name.
/// - `AWS_SECRET_ACCESS_KEY`, `GH_TOKEN`, `GITHUB_TOKEN`, … — same class. Not
///   enumerated: an allowlist does not have to know them.
pub const ENV_ALLOWLIST: &[(&str, &str)] = &[
    // ── Making a command run at all ──────────────────────────────────────────
    ("PATH", "every spawn: `Command::new(\"git\")` is resolved through it"),
    (
        "HOME",
        "gcloud (~/.config/gcloud), ssh (~/.ssh), git (~/.gitconfig), cargo, npm",
    ),
    ("PWD", "shells and build tools that report their own location"),
    ("SHELL", "tools that re-exec the user's shell (`git`, editors)"),
    ("USER", "ssh's default login name; git's fallback author"),
    ("LOGNAME", "same, the POSIX spelling — some tools read only this one"),
    // ── Temporary files ──────────────────────────────────────────────────────
    ("TMPDIR", "cargo, ffmpeg, whisper, tesseract all stage large files"),
    ("TMP", "the same need, spelled the way some toolchains expect"),
    ("TEMP", "the Windows-flavoured spelling some cross-platform tools still read"),
    // ── Text the runtime has to parse back ───────────────────────────────────
    (
        "LANG",
        "locale drives tool output; a child in C locale reports different dates than the scenario expects",
    ),
    ("LANGUAGE", "GNU message-language fallback list, same class as LANG"),
    ("TERM", "pagers and anything probing terminal capability"),
    ("TZ", "timestamps in child output must match the runtime's own"),
    // ── TLS trust, or every https call fails ─────────────────────────────────
    ("SSL_CERT_FILE", "openssl/reqwest CA bundle on distros that relocate it"),
    ("SSL_CERT_DIR", "openssl again, the hashed-directory form used by Debian"),
    ("CURL_CA_BUNDLE", "curl, which the read-only command list allows"),
    ("REQUESTS_CA_BUNDLE", "python tooling (gcloud ships its own python)"),
    ("NIX_SSL_CERT_FILE", "the same, on Nix hosts where the others are unset"),
    // ── The `x0ne cc` chantier: gcloud and ssh must keep working ─────────────
    (
        "CLOUDSDK_CONFIG",
        "gcloud config dir when it is not ~/.config/gcloud (x0ne cc)",
    ),
    ("CLOUDSDK_PYTHON", "the interpreter gcloud runs itself under (x0ne cc)"),
    ("CLOUDSDK_PYTHON_SITEPACKAGES", "companion of CLOUDSDK_PYTHON, same gcloud need"),
    ("CLOUDSDK_ACTIVE_CONFIG_NAME", "which named gcloud configuration to use"),
    ("CLOUDSDK_CORE_PROJECT", "project selection for `x0ne cc` deploys"),
    ("CLOUDSDK_COMPUTE_REGION", "region selection for `x0ne cc` deploys"),
    ("CLOUDSDK_COMPUTE_ZONE", "zone selection for `x0ne cc` deploys"),
    (
        "SSH_AUTH_SOCK",
        "ssh-agent socket — `gcloud compute ssh` and SshExec authenticate through it",
    ),
    // ── Desktop primitives (Screenshot, Click, TypeText, SendNotification) ───
    ("DISPLAY", "xdotool, scrot, identify — the X11 desktop family"),
    ("WAYLAND_DISPLAY", "wl-copy and the Wayland half of the same family"),
    ("XAUTHORITY", "X11 cookie file; without it DISPLAY alone opens nothing"),
    ("XDG_RUNTIME_DIR", "Wayland socket and D-Bus session live under it"),
    ("XDG_SESSION_TYPE", "how the desktop code picks X11 vs Wayland"),
    (
        "DBUS_SESSION_BUS_ADDRESS",
        "notify-send (SendNotification) reaches the session bus through it",
    ),
];

/// Allowlisted by prefix, for families whose members cannot be enumerated.
///
/// SECURITY: a prefix is a much wider grant than a name — see the
/// `CLOUDSDK_AUTH_*` reasoning on [`ENV_ALLOWLIST`] for the shape of the
/// mistake. Only families that hold no credentials belong here.
pub const ENV_ALLOWLIST_PREFIXES: &[(&str, &str)] = &[(
    "LC_",
    "POSIX locale categories (LC_ALL, LC_CTYPE, LC_NUMERIC, …) — open-ended by standard, and none of them can hold a secret",
)];

// ── Operator-configured extension ─────────────────────────────────────────────

/// Extra names the operator opted into via `policy.env_passthrough`.
///
/// SECURITY: the default is empty — STRICT. An uninitialised slot behaves
/// exactly like a configured empty list, so forgetting [`install_passthrough`]
/// fails closed (nothing extra passes) rather than open.
static PASSTHROUGH: OnceLock<RwLock<Vec<String>>> = OnceLock::new();

fn slot() -> &'static RwLock<Vec<String>> {
    PASSTHROUGH.get_or_init(|| RwLock::new(Vec::new()))
}

/// Install the operator's `policy.env_passthrough` list. Called once from the
/// binary entry points after the config is loaded; tests call it directly.
/// Returns the previous list so a test can restore it.
pub fn install_passthrough(names: &[String]) -> Vec<String> {
    let mut guard = slot().write().unwrap();
    std::mem::replace(&mut *guard, names.to_vec())
}

/// Is `name` allowed through to a child?
pub fn is_allowed(name: &str) -> bool {
    if ENV_ALLOWLIST.iter().any(|(n, _)| *n == name) {
        return true;
    }
    if ENV_ALLOWLIST_PREFIXES
        .iter()
        .any(|(p, _)| name.starts_with(p))
    {
        return true;
    }
    slot().read().unwrap().iter().any(|n| n == name)
}

/// The pairs a child is allowed to receive, read from the runtime's own
/// environment at spawn time.
fn allowed_pairs() -> Vec<(String, String)> {
    std::env::vars().filter(|(k, _)| is_allowed(k)).collect()
}

// ── Applying it ───────────────────────────────────────────────────────────────

/// The two `Command` types the runtime uses, behind one method pair, so that
/// [`harden`] has a single body and no site can be hardened "the other way".
pub trait ChildCommand {
    fn clear_env(&mut self);
    fn set_env(&mut self, key: &str, value: &str);
}

impl ChildCommand for std::process::Command {
    fn clear_env(&mut self) {
        self.env_clear();
    }
    fn set_env(&mut self, key: &str, value: &str) {
        self.env(key, value);
    }
}

impl ChildCommand for tokio::process::Command {
    fn clear_env(&mut self) {
        self.env_clear();
    }
    fn set_env(&mut self, key: &str, value: &str) {
        self.env(key, value);
    }
}

/// Drop the runtime's environment and rebuild it from the allowlist.
///
/// SECURITY: call this on EVERY command the runtime spawns, before any
/// caller-supplied `.env()` — an action's explicit `env` map is the caller's
/// stated intent and must survive, which it does because `env_clear` happens
/// here and the explicit values are applied after. Reversing the order silently
/// erases them.
///
/// Removing the `clear_env` call re-opens the 2026-08-14 leak in full; the
/// regression tests in `x0ne-runtime/tests/host_secrets_stay_on_the_host.rs`
/// are what notice.
pub fn harden<C: ChildCommand>(cmd: &mut C) {
    cmd.clear_env();
    for (k, v) in allowed_pairs() {
        cmd.set_env(&k, &v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole module exists for. A name nobody put on the list
    /// is invisible to a child, whatever it is called.
    #[test]
    fn a_name_outside_the_allowlist_is_not_allowed() {
        for evil in [
            "X0NE_CANARY_TEST",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GH_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ] {
            assert!(!is_allowed(evil), "{evil} would reach a child process");
        }
    }

    /// The `CLOUDSDK_` shape, asserted rather than trusted to the comment: the
    /// configuration names pass and the credential names do not. A future
    /// `("CLOUDSDK_", …)` prefix entry would ship the token with the project.
    #[test]
    fn gcloud_config_passes_but_gcloud_credentials_do_not() {
        assert!(is_allowed("CLOUDSDK_CONFIG"));
        assert!(is_allowed("CLOUDSDK_CORE_PROJECT"));
        assert!(!is_allowed("CLOUDSDK_AUTH_ACCESS_TOKEN"));
        assert!(!is_allowed("CLOUDSDK_AUTH_REFRESH_TOKEN"));
    }

    #[test]
    fn the_locale_family_passes_by_prefix() {
        assert!(is_allowed("LC_ALL"));
        assert!(is_allowed("LC_CTYPE"));
        assert!(is_allowed("LC_TIME"));
    }

    /// Every entry names who needs it. The justification is the only thing
    /// standing between this list and the denylist it must not become.
    #[test]
    fn every_allowlist_entry_says_who_needs_it() {
        for (name, why) in ENV_ALLOWLIST.iter().chain(ENV_ALLOWLIST_PREFIXES) {
            assert!(
                why.len() > 20,
                "{name} is allowed through with no stated consumer"
            );
            assert!(!name.is_empty());
        }
    }

    /// PATH is not optional: without it `Command::new("git")` fails to resolve
    /// and every bare-command scenario turns into ENOENT.
    #[test]
    fn path_and_home_are_on_the_list() {
        assert!(is_allowed("PATH"));
        assert!(is_allowed("HOME"));
    }

    /// `env_passthrough` widens the list and nothing else — and its default is
    /// empty, so a runtime whose entry point forgot to install it is strict.
    #[test]
    fn passthrough_widens_only_what_the_operator_named() {
        let previous = install_passthrough(&["MY_BUILD_FLAG".to_string()]);
        assert!(is_allowed("MY_BUILD_FLAG"));
        assert!(!is_allowed("MY_OTHER_FLAG"));
        install_passthrough(&previous);
        assert!(!is_allowed("MY_BUILD_FLAG"), "the slot must be restorable");
    }

    /// `harden` must be applied before caller-supplied `.env()`, never after.
    /// Asserted through a real child because the ordering bug is invisible in
    /// the builder: `env_clear` after `env` compiles and erases the value.
    #[tokio::test]
    async fn an_explicit_env_value_survives_the_clear() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "printf %s \"$EXPLICIT\""]);
        harden(&mut cmd);
        cmd.env("EXPLICIT", "kept");
        let out = cmd.output().await.expect("sh must run");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "kept");
    }

    /// Remove every `#[cfg(test)] mod … { … }` block, brace-matched, leaving
    /// the production text of a source file.
    ///
    /// A file can hold several test modules with shipped code between them
    /// (`system.rs`: modules at 715 and 1512, `disk_usage` at 1138), so cutting
    /// at the first attribute silently stops scanning two thirds of the way in.
    fn strip_test_modules(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(i) = rest.find("#[cfg(test)]") {
            // Only a `mod` block is stripped; `#[cfg(test)]` on a single item
            // (or inside a comment, as in `desktop.rs:21`) is left in place.
            let after = &rest[i..];
            let Some(m) = after.find("mod ") else {
                out.push_str(&rest[..i + 12]);
                rest = &rest[i + 12..];
                continue;
            };
            let Some(open) = after[m..].find('{').map(|j| m + j) else {
                out.push_str(&rest[..i + 12]);
                rest = &rest[i + 12..];
                continue;
            };
            // `mod` must follow the attribute closely, or this is some other
            // item that happens to precede an unrelated module.
            if after[..m].trim().len() > "#[cfg(test)]".len() {
                out.push_str(&rest[..i + 12]);
                rest = &rest[i + 12..];
                continue;
            }
            let mut depth = 0usize;
            let mut end = None;
            for (off, ch) in after[open..].char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(open + off + 1);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            out.push_str(&rest[..i]);
            match end {
                Some(e) => rest = &after[e..],
                None => {
                    // Unbalanced — the module runs to EOF, which is the usual
                    // shape. Everything left is test code.
                    rest = "";
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// Every spawn the crate performs goes through the gate.
    ///
    /// This is the test that has to exist, because the defect class this
    /// repository keeps rediscovering is not the wrong fix — it is the site
    /// written afterwards that nobody applied the fix to. The stdin repair of
    /// 2026-08-14 left `podman cp`, `podman rm -f` and `podman --version`
    /// behind, and nothing noticed for a day (`sandbox.rs`, review round 3).
    ///
    /// It reads the source rather than spawning, and that is the honest bound
    /// of what it proves: it catches a new `Command::new(…)` written without
    /// `harden`, which is the shape of the defect. A spawn reached through some
    /// other alias slips past — as it does for the stdin scan next door.
    ///
    /// The directory is walked rather than listed, so a NEW executor file with
    /// a spawn in it is covered the day it appears, not the day someone
    /// remembers to add it here.
    #[test]
    fn every_spawn_in_the_crate_goes_through_the_env_gate() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    // `bin/` holds operator tooling (`x0ne-mcp-audit`) and the
                    // scenario runner, neither of which executes a model's
                    // choice. `dataset_cli/` is offline dataset work.
                    let name = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    if name != "bin" && name != "dataset_cli" {
                        stack.push(path);
                    }
                } else if path.extension().is_some_and(|e| e == "rs") {
                    files.push(path);
                }
            }
        }

        let mut checked = 0;
        for path in files {
            let text = std::fs::read_to_string(&path).expect("read source");

            // This file is the gate, and its prose quotes the very pattern the
            // scan looks for. Excluding it is safe only while it spawns nothing
            // itself, which is asserted rather than assumed.
            if path.file_name().is_some_and(|n| n == "child_env.rs") {
                let head = &text[..text.find("#[cfg(test)]").expect("its own tests")];
                for run in [".spawn()", ".output()", ".status()"] {
                    assert!(
                        !head.contains(run),
                        "child_env.rs now runs a command of its own ({run}); it is \
                         excluded from the scan on the premise that it does not"
                    );
                }
                continue;
            }
            // Production half only: test modules spawn helpers of their own.
            // Cutting at the first `#[cfg(test)]` is NOT enough and the first
            // version of this test did exactly that: `desktop.rs:21` merely
            // mentions the attribute in a comment, which hid all four of its
            // spawns, and `system.rs` has a test module at 715 with production
            // code after it, which hid a third of its own.
            let head = strip_test_modules(&text);
            let head = head.as_str();
            let mut from = 0;
            while let Some(i) = head[from..].find("Command::new(") {
                let at = from + i;
                // A builder chain ends where it is run; everything it
                // configures — `harden` included — sits between the two.
                let end = [".output()", ".status()", ".spawn()", "\n}\n"]
                    .iter()
                    .filter_map(|t| head[at..].find(t).map(|j| at + j))
                    .min()
                    .unwrap_or(head.len());
                assert!(
                    head[at..end].contains("child_env::harden")
                        || head[at..end].contains("harden(&mut cmd)"),
                    "{}: this spawn inherits the operator's environment, \
                     including every secret in ~/.x0ne/.env — the 2026-08-14 \
                     leak, reopened:\n{}",
                    path.display(),
                    &head[at..end]
                );
                checked += 1;
                from = end.max(at + 1);
            }
        }
        assert!(
            checked >= 16,
            "the scan found only {checked} spawn sites; it saw 16 when it was \
             written, so it has stopped seeing some of them"
        );
    }

    /// The measurement of 2026-08-14, run against a real child: a variable
    /// planted in this process's environment does not come back out of one.
    #[tokio::test]
    async fn a_planted_variable_does_not_come_back_out_of_a_child() {
        // SAFETY: single-threaded within this test's own scope; the value is a
        // marker, and no other test reads it.
        std::env::set_var("X0NE_CANARY_TEST", "fuite-unit");

        let mut cmd = tokio::process::Command::new("env");
        harden(&mut cmd);
        let out = cmd.output().await.expect("env must run");
        let seen = String::from_utf8_lossy(&out.stdout).into_owned();

        std::env::remove_var("X0NE_CANARY_TEST");

        assert!(
            !seen.contains("X0NE_CANARY_TEST"),
            "the host's environment reached the child:\n{seen}"
        );
        assert!(
            seen.contains("PATH="),
            "the allowlist must still let PATH through"
        );
    }
}
