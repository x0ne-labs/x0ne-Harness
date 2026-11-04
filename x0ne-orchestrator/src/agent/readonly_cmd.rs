//! Which `RunCommand` invocations observe the host without changing it.
//!
//! `RunCommand` classes as `SideEffectClass::ProcessSpawn`, and every
//! ProcessSpawn is gated by the human. That is correct as a default — spawning
//! a process is spawning a process, and the runtime cannot know what the
//! binary does. But applied uniformly it produced this, nine times in one
//! session (field report, 2026-08-13):
//!
//! ```text
//! $ cat playground/README.md
//! ⚠ this action changes state on the host and needs your approval
//! ```
//!
//! The warning is false. `cat` changes nothing. The cost of saying it anyway
//! is not zero and not merely cosmetic: a gate that fires on reads trains the
//! operator to answer `1` without reading, and then it no longer guards the
//! writes either. Approval fatigue is the failure mode this module exists to
//! avoid, and it is a *security* argument, not an ergonomic one.
//!
//! # What makes this safe to do at all
//!
//! `RunCommand` executes `Command::new(command).args(args)`
//! (`x0ne-runtime/src/executor/process.rs:191`). There is **no shell**. So
//! `;`, `|`, `&&`, `>` and `$(…)` arrive at the program as literal argv
//! strings, not as operators — `cat "a; rm -rf b"` looks for a file with a
//! silly name and fails. The only way to reach a shell's grammar is to *name*
//! a shell as the program, and no shell is on the table below.
//!
//! # The rule the table follows
//!
//! A program earns a place only if **no argv can make it write, delete, or
//! execute anything**. That is a strong claim, so the exclusions are as
//! interesting as the inclusions:
//!
//! - `sed` — `-i` edits in place.
//! - `awk` — `print > "file"` writes, `system()` executes.
//! - `env` — `env FOO=1 rm -rf /` is a launcher wearing a disguise.
//! - `xargs`, `parallel`, `watch`, `timeout`, `nohup` — same, launchers.
//! - `cargo check`, `cargo tree` — compiling runs `build.rs` and proc macros,
//!   which is arbitrary code execution by another name. No `cargo` subcommand
//!   qualifies, including the ones that look inert.
//! - `tar`, `unzip`, `curl`, `wget` — all write files.
//!
//! Anything absent from the table keeps the full gate. The table is closed and
//! adding to it requires the same argument: *no argv can make it write*.
//!
//! # Programs that read by default and write when told to
//!
//! Some tools are a read and a write wearing one name, and the table below
//! cannot hold them: membership is decided by the program alone, so a `true`
//! there dispenses EVERY argv. They live in the `match` instead, next to
//! `find`, and their action flags are the whole difference.
//!
//! `sort` was in the table, under a comment asserting that none of its
//! neighbours "can open a file for writing". That was false, and it was
//! measured end to end on the demo enclave's own policy on 2026-08-15:
//!
//! ```text
//! RunCommand sort -o <outside-the-write-root>/planted.txt <home>/.ssh/id_rsa
//!   → success; the key was copied out, and no human was ever asked.
//! ```
//!
//! Two failures compounded there. `sort -o` writes, so the read-only verdict
//! removed the gate from a write; and `RunCommand` argv paths are not confined
//! at all (the argv-path gap), so the write landed outside every root. The gate was
//! the only thing standing there, and this module had waved it off.
//! `uniq IN OUT` writes its second operand — measured the same day. `tree -o`
//! is documented to write; not measured here, `tree` is not installed on this
//! workstation.

/// Programs that cannot write, delete, or execute under **any** argv.
///
/// Matched against the command's file name, so `/bin/cat` and `cat` both hit
/// and no `../` dance changes which program is named.
const ALWAYS_READ_ONLY: &[&str] = &[
    // Inspect the filesystem. `tree` is NOT here: `tree -o FILE` writes.
    "ls",
    "pwd",
    "cat",
    "head",
    "tail",
    "wc",
    "file",
    "stat",
    "du",
    "df",
    "readlink",
    "realpath",
    "basename",
    "dirname",
    "cksum",
    "md5sum",
    "sha1sum",
    "sha256sum",
    // Search. GNU grep only: it has no flag that runs a program or names an
    // output file (checked against grep 3.11, 2026-08-15). `rg`, `ag` and
    // `ack` are NOT here — see `SEARCH_TOOLS_THAT_EXECUTE` below.
    "grep",
    "egrep",
    "fgrep",
    // Compare and reshape a stream. These emit on stdout and have no flag or
    // operand that names an output file — checked one by one, 2026-08-15,
    // after `sort` and `uniq` were found in this group and both write.
    // `sort -o` and `uniq IN OUT` now live in the `match` below.
    "diff",
    "cmp",
    "cut",
    "nl",
    "column",
    "jq",
    "tr",
    "rev",
    "fold",
    "join",
    "paste",
    "comm",
    "expand",
    "unexpand",
    // Report on the machine. `date -s` and `hostname NEWNAME` do mutate, so
    // these two are an exception to the rule above, taken knowingly: both
    // changes are privileged, and a runtime that already runs as root has
    // lost this argument long before `date`. Measured unprivileged,
    // 2026-08-15: "cannot set date: Operation not permitted" and "you must be
    // root to change the host name". If x0ne is ever run as root, they belong
    // in the `match` below with `-s` and a bare operand as their action flags.
    "date",
    "whoami",
    "id",
    "hostname",
    "uname",
    "uptime",
    "ps",
    "free",
    "nproc",
    "arch",
    "locale",
    "printenv",
    "groups",
    "tty",
    // Ask where a program is. `which`/`type` resolve a name; they do not run it.
    "which",
    "whereis",
    // Emit their arguments.
    "echo",
    "printf",
    "true",
    "false",
];

/// Subcommands of `git` that only read the repository.
///
/// Deliberately absent: `tag` and `remote`, which list when bare and *write*
/// when given an argument — a distinction too fragile to encode here; `stash`,
/// whose `list` is safe and whose everything-else is not; and naturally
/// `checkout`, `reset`, `clean`, `push`, `commit`.
const GIT_READ_ONLY: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "blame",
    "describe",
    "rev-parse",
    "ls-files",
    "ls-tree",
    "ls-remote",
    "cat-file",
    "shortlog",
    "grep",
    "whatchanged",
    "reflog",
];

/// Why the modern search tools are absent from the table above.
///
/// `rg --pre COMMAND` runs COMMAND on every file it is about to search, which
/// is arbitrary code execution behind a flag that reads like a filter.
/// Measured on ripgrep 14.1.1, 2026-08-15: `rg --pre ./pre.sh hello target.txt`
/// ran the script, which wrote a canary file. `rg` sat in `ALWAYS_READ_ONLY`,
/// where membership dispenses EVERY argv, so that execution never met the
/// human gate. `ag --pager` and `ack --pager` are the same motif (documented;
/// neither is installed on the machine this was measured on).
///
/// THE FIX IS REMOVAL, NOT A PREDICATE, and the choice is worth stating
/// because `find` right below does get a predicate. Three reasons they differ:
///
/// 1. The table's contract is absolute — "no argv can make it write, delete,
///    or execute". `find` still satisfies it once its action predicates are
///    excluded; `rg` cannot satisfy it at all, so it does not belong in a
///    table whose entry price is that promise.
/// 2. `find`'s action set is CLOSED and stable — POSIX plus a handful of GNU
///    extensions that have not moved in decades. ripgrep's execution surface
///    is versioned: `--pre` and `--pre-glob` today, whatever 15.x adds
///    tomorrow. A denylist there fails OPEN on every future release, silently
///    — which is this repo's recurring defect wearing a new hat.
/// 3. The cost is bounded and small. GNU `grep` stays dispensed and does the
///    same job, so the common case is unchanged; `rg` is still perfectly
///    runnable, it just asks once. A question is a much cheaper mistake than
///    a silent `--pre`.
///
/// This list is documentation, not a check — the tools are simply absent from
/// the table. A test asserts they stay absent.
#[cfg_attr(not(test), allow(dead_code))] // Only that test reads it.
const SEARCH_TOOLS_THAT_EXECUTE: &[&str] = &["rg", "ag", "ack"];

/// `find` predicates that make it act rather than report.
const FIND_ACTIONS: &[&str] = &[
    "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fls", "-fprint", "-fprint0", "-fprintf",
];

/// Does this argv name an output file with `-o` / `--output`?
///
/// Used by `sort` and `tree`, whose only write is that flag.
///
/// DELIBERATELY OVER-APPROXIMATE on both forms, because both can be spelled
/// more ways than they look.
///
/// SHORT: GNU short options cluster and may take their value attached, so the
/// flag arrives as `-o`, `-oFILE`, or `-uo FILE`. Rather than parse the
/// cluster, ANY single-dash argument containing an `o` is treated as an output
/// flag. In both programs `o` means output and nothing else. The cost is a
/// false gate on the ATTACHED separator form `sort -to` (field separator `o`)
/// — one needless question. Note that `sort -t o`, written separately, is NOT
/// a false positive: `-t` holds no `o` and the `o` is an operand. An earlier
/// version of this comment claimed otherwise and was simply wrong.
///
/// LONG: `getopt_long` accepts any UNAMBIGUOUS ABBREVIATION, and `output` is
/// the only long option starting with `o` in either program — so `--o=F`,
/// `--ou=F`, `--outp=F` and `--o F` all write. Measured on coreutils 9.7,
/// 2026-08-15: all four created the file. A `starts_with("output")` test
/// therefore missed every abbreviation and called them read-only, which is why
/// the comparison runs the OTHER WAY: the argument must be a prefix OF
/// `output`, not the reverse. The empty string is excluded so a bare `--`
/// terminator is not read as an output flag.
fn names_an_output_file(args: &[String]) -> bool {
    args.iter().any(|a| {
        if let Some(long) = a.strip_prefix("--") {
            // `--output=FILE` and every abbreviation of it: compare the part
            // before `=` against `output` as a PREFIX OF IT.
            let name = long.split('=').next().unwrap_or("");
            !name.is_empty() && "output".starts_with(name)
        } else if let Some(short) = a.strip_prefix('-') {
            short.contains('o')
        } else {
            false
        }
    })
}

/// How many argv entries are operands rather than flags.
///
/// Used by `uniq`, whose SECOND operand is an output file
/// (`uniq INPUT OUTPUT` — measured writing on 2026-08-15).
///
/// Also deliberately over-approximate: a short option that takes a separate
/// value (`-f 2`, `-s 3`, `-w 10`) leaves its value looking like an operand,
/// and this counts it as one. `uniq -f 2 in.txt` therefore counts two and gets
/// gated, which is a needless question rather than an unguarded write. Fail
/// closed: undercounting operands is what would dispense a write.
fn operand_count(args: &[String]) -> usize {
    args.iter().filter(|a| !a.starts_with('-')).count()
}

/// Argv paths a read is never dispensed for, whatever program names them.
///
/// SECURITY: `/proc/<pid>/environ` is the whole environment of another process,
/// and the process next door is the orchestrator — which holds every key in
/// `~/.x0ne/.env`. The August 2026 fix cleared what a child INHERITS
/// (`x0ne-runtime/src/child_env.rs`); it did nothing about what a child can go
/// and READ. Measured end to end on 2026-08-15, two dispensed commands:
///
/// ```text
/// cat /proc/self/status   → PPid: <the orchestrator>
/// cat /proc/<ppid>/environ → ANTHROPIC_API_KEY=…
/// ```
///
/// Both were `cat`, both were classed read-only, and neither was ever shown to
/// a human. They still are reads — the verdict below is not that they mutate,
/// it is that "read" and "harmless enough to skip the human" are not the same
/// claim for this one file.
///
/// # The bound of what this fixes
///
/// This removes the DISPENSATION, not the access. `RunCommand` does not confine
/// argv paths at all (the argv-path gap), so an approved `cat /proc/N/environ` still
/// runs — the human is now asked first, and that is the entire change. Nor does
/// a final component match every route to the same bytes: `cat` with
/// `cwd=/proc/N` and `args=["environ"]` matches (the component is compared),
/// but a symlink pointing at it does not, and `is_read_only_command` never sees
/// `cwd` anyway. Closing that needs argv confinement, which is the argv-path gap.
fn names_a_process_environment(args: &[String]) -> bool {
    args.iter().any(|a| {
        std::path::Path::new(a)
            .file_name()
            .is_some_and(|n| n == "environ")
    })
}

/// Whether `command args…` observes the host without changing it — the argv
/// half of the verdict. Private: [`is_read_only_command`] is the answer callers
/// need, and it has one more question to ask.
fn argv_is_read_only(command: &str, args: &[String]) -> bool {
    // Compare on the file name so `/usr/bin/cat` and `cat` are one case. A
    // command that is only a directory (`.`, `/`) has no file name and is
    // rejected rather than guessed at.
    let Some(program) = std::path::Path::new(command)
        .file_name()
        .and_then(|s| s.to_str())
    else {
        return false;
    };

    if ALWAYS_READ_ONLY.contains(&program) {
        return true;
    }

    match program {
        // `find` reports by default and acts only when told to. The action
        // predicates are the whole difference, and `-exec` in particular turns
        // it into a launcher for anything.
        // SECURITY: these three read by default and write when told to, so the
        // verdict depends on argv. `sort` sat in the table above until
        // 2026-08-15, when `sort -o <outside-the-root> <home>/.ssh/id_rsa` was
        // measured copying a key past the gate and past the write root.
        "sort" | "tree" => !names_an_output_file(args),

        // `uniq INPUT OUTPUT` writes OUTPUT. One operand is a read.
        "uniq" => operand_count(args) < 2,

        "find" | "fd" | "fdfind" => !args.iter().any(|a| {
            FIND_ACTIONS.contains(&a.as_str())
                // `--exec`/`-x` are fd's spelling of the same thing.
                || a == "--exec" || a == "-x" || a == "--exec-batch" || a == "-X"
        }),

        // The subcommand decides, and it must be the *first* argument. A
        // leading global flag is refused rather than skipped over: `git -c
        // core.pager=… log` and `git --exec-path=/tmp status` both run a
        // command of the caller's choosing while looking like a read.
        // `ip` reports unless told to act. Only display flags may come
        // first (`-n NS`, `-batch FILE`, `-force` are refused), then a known
        // object, then nothing, `show`/`list`/`get`, and no acting verb.
        "ip" => ip_is_read_only(args),
        // `ss -K` closes sockets; `ss -D FILE` writes a file.
        "ss" => !args.iter().any(|a| {
            a.starts_with("--kill")
                || a.starts_with("--diag")
                || (a.starts_with('-')
                    && !a.starts_with("--")
                    && (a.contains('K') || a.contains('D')))
        }),

        "git" => args.first().is_some_and(|sub| {
            GIT_READ_ONLY.contains(&sub.as_str())
                // `--ext-diff` hands the diff to an external program named in
                // gitconfig. Rare, but it is exactly the hole this guards.
                && !args.iter().any(|a| a == "--ext-diff")
        }),

        _ => false,
    }
}

fn ip_is_read_only(args: &[String]) -> bool {
    const FLAGS: &[&str] = &[
        "-4",
        "-6",
        "-br",
        "-brief",
        "-c",
        "-color",
        "-s",
        "-stats",
        "-statistics",
        "-d",
        "-details",
        "-j",
        "-json",
        "-p",
        "-pretty",
        "-o",
        "-oneline",
    ];
    const OBJECTS: &[&str] = &[
        "a",
        "addr",
        "address",
        "l",
        "link",
        "r",
        "ro",
        "route",
        "n",
        "neigh",
        "neighbour",
        "neighbor",
        "ru",
        "rule",
        "maddr",
        "maddress",
    ];
    const VERBS: &[&str] = &["show", "sh", "list", "ls", "lst", "get"];
    let mut rest = args
        .iter()
        .map(String::as_str)
        .skip_while(|a| FLAGS.contains(a));
    let Some(object) = rest.next() else {
        return false;
    };
    if !OBJECTS.contains(&object) {
        return false;
    }
    let tail: Vec<&str> = rest.collect();
    match tail.first() {
        None => true,
        Some(v) if VERBS.contains(v) => !tail[1..].iter().any(|a| {
            matches!(
                *a,
                "exec"
                    | "flush"
                    | "add"
                    | "del"
                    | "delete"
                    | "set"
                    | "change"
                    | "replace"
                    | "append"
                    | "prepend"
                    | "save"
                    | "restore"
            )
        }),
        // `ip -br addr dev eth0`-style filters without a verb are not accepted.
        Some(_) => false,
    }
}

/// Whether this invocation may skip the human gate.
///
/// `false` is always the safe answer and is what every unrecognised program
/// gets. A `true` here removes the human gate for that one invocation, so the
/// question to ask of any change is not "is this usually harmless?" but "can
/// *no* argv make this write?".
///
/// SECURITY: `env` is a parameter, and that is the fix of 2026-08-15. The
/// verdict used to be taken from `command` and `args` alone while a third
/// field — model-written, invisible here — chose which binary the name
/// resolved to:
///
/// ```text
/// {"command":"cat","args":["f"],"env":{"PATH":"/tmp/evil"}}  → /tmp/evil/cat
/// ```
///
/// The runtime now refuses that key outright (`x0ne_actions::env_keys`), so
/// this could have been narrowed to "no program-choosing key". It is not, and
/// the choice is deliberate: **any** non-empty `env` keeps the gate.
///
/// Two reasons. The dispensation's whole premise is that what runs is exactly
/// what the operator read in the command line, and an `env` map is a second
/// input to the child's behaviour that the prompt does not render — approving
/// what one cannot see is the thing invariant 5 forbids. And matching the two
/// rules to each other makes this file's verdict depend on a list living in
/// another crate: the day a variable is added there and not here, the gap is
/// silent. An empty map is the common case (the model sets `env` on maybe one
/// RunCommand in a hundred), so the cost is a prompt almost nobody sees.
pub fn is_read_only_command(
    command: &str,
    args: &[String],
    env: &std::collections::HashMap<String, String>,
    cwd: Option<&str>,
) -> bool {
    if !env.is_empty() {
        return false;
    }
    if names_a_process_environment(args) {
        return false;
    }
    // SECURITY: `deny_path_patterns` is the one list a human "yes"
    // cannot lift — and a dispensed `cat` traversed it without anyone being
    // asked (measured end to end, exercises 06/08/10, sentinel bytes off the
    // disk). A read of denied material is still a read, but it is exactly
    // the read the operator wrote that list about: the gate stays. Same
    // matcher and same list as the runtime (`x0ne_actions::deny_paths`,
    // the shared-function pattern) — a miss here only keeps today's behaviour, a false
    // positive only asks a human.
    if args.iter().any(|a| names_denied_material(a)) {
        return false;
    }
    // SECURITY: a whitelist, not a blacklist. Every operand, and
    // `cwd` itself, must resolve under the workspace; the deny list above
    // is the second barrier for what lives inside it.
    let root = workspace_root();
    let Some(resolved) = resolved_operands(args, cwd, &root) else {
        return false;
    };
    if resolved
        .iter()
        .any(|p| x0ne_actions::deny_paths::matches_default_deny(&p.display().to_string()).is_some())
    {
        return false;
    }
    argv_is_read_only(command, args)
}

static WORKSPACE_ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// The directory a dispensed read may not leave. Set once by the frontend
/// after project detection; a second call is ignored. Unset: the process cwd.
pub fn set_workspace_root(root: &std::path::Path) {
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let _ = WORKSPACE_ROOT.set(canon);
}

/// Fails closed: with no readable cwd nothing resolves under the root, so
/// every read keeps the gate.
pub fn workspace_root() -> std::path::PathBuf {
    if let Some(r) = WORKSPACE_ROOT.get() {
        return r.clone();
    }
    std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .unwrap_or_else(|_| std::path::PathBuf::from("/nonexistent-workspace"))
}

/// The absolute path of every operand, or `None` as soon as one — or `cwd` —
/// escapes `root`. Symlinks are followed down to the deepest existing
/// ancestor, so a link planted inside cannot point outside.
pub(crate) fn resolved_operands(
    args: &[String],
    cwd: Option<&str>,
    root: &std::path::Path,
) -> Option<Vec<std::path::PathBuf>> {
    let base = match cwd {
        None => root.to_path_buf(),
        Some(c) => resolve_under(&expand_home(c), root, root)?,
    };
    args.iter()
        .filter_map(|a| operand_of(a))
        .map(|op| resolve_under(&expand_home(op), &base, root))
        .collect()
}

/// The path-like part of one argv entry: an operand, the value of
/// `--flag=value`, or what follows a short flag when it carries a `/`.
fn operand_of(arg: &str) -> Option<&str> {
    if arg == "-" {
        return None;
    }
    if !arg.starts_with('-') {
        return Some(arg);
    }
    if let Some((_, v)) = arg.split_once('=') {
        return (!v.is_empty()).then_some(v);
    }
    arg.find('/').map(|i| &arg[i..])
}

fn expand_home(s: &str) -> std::path::PathBuf {
    let home = || std::env::var_os("HOME").map(std::path::PathBuf::from);
    match s.strip_prefix("~/") {
        Some(rest) => home().map(|h| h.join(rest)),
        None if s == "~" => home(),
        None => None,
    }
    .unwrap_or_else(|| std::path::PathBuf::from(s))
}

/// `path` (relative to `base`) made absolute, `..` folded, the deepest
/// existing ancestor canonicalized; `Some` only if the result is under `root`.
fn resolve_under(
    path: &std::path::Path,
    base: &std::path::Path,
    root: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let joined = if path.is_relative() {
        base.join(path)
    } else {
        path.to_path_buf()
    };
    let mut lexical = std::path::PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::ParentDir => {
                lexical.pop();
            }
            std::path::Component::CurDir => {}
            other => lexical.push(other),
        }
    }
    let mut existing = lexical.as_path();
    let mut tail = Vec::new();
    while !existing.exists() {
        let parent = existing.parent()?;
        if let Some(name) = existing.file_name() {
            tail.push(name.to_os_string());
        }
        existing = parent;
    }
    let mut canon = std::fs::canonicalize(existing).ok()?;
    for name in tail.iter().rev() {
        canon.push(name);
    }
    canon.starts_with(root).then_some(canon)
}

/// Does this argument name a path the default deny patterns cover? Tries the
/// forms an argv actually carries: the raw string, `--flag=value`, `~`
/// expansion, the cwd-relative form, and — against symlink evasion — the
/// canonicalized path when the file exists.
fn names_denied_material(arg: &str) -> bool {
    let candidate = match arg.split_once('=') {
        Some((_, v)) => v,
        None if arg.starts_with('-') => return false, // a bare flag names no file
        None => arg,
    };
    if candidate.is_empty() {
        return false;
    }
    let mut forms: Vec<String> = vec![candidate.to_string()];
    let expanded = match candidate.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(h) => std::path::Path::new(&h).join(rest),
            None => std::path::PathBuf::from(candidate),
        },
        None => std::path::PathBuf::from(candidate),
    };
    forms.push(expanded.display().to_string());
    if expanded.is_relative() {
        if let Ok(cwd) = std::env::current_dir() {
            forms.push(cwd.join(&expanded).display().to_string());
        }
    }
    if let Ok(canon) = std::fs::canonicalize(&expanded) {
        forms.push(canon.display().to_string());
    }
    forms
        .iter()
        .any(|f| x0ne_actions::deny_paths::matches_default_deny(f).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// The argv-only verdict, for the tests that predate `env` and are about
    /// the program table. The `env`-aware property has tests of its own below.
    fn ro(command: &str, args: &[String]) -> bool {
        super::is_read_only_command(command, args, &HashMap::new(), None)
    }

    // `ip addr` and friends only report; `ip … set`, `-batch`, `netns
    // exec` and `ss -K` act, and keep the gate.
    #[test]
    fn ip_and_ss_reports_are_read_only() {
        for a in [
            &["addr"][..],
            &["a"],
            &["-br", "addr", "show"],
            &["route"],
            &["-4", "route", "get", "1.1.1.1"],
            &["link", "show"],
            &["neigh"],
        ] {
            assert!(super::argv_is_read_only("ip", &argv(a)), "ip {a:?}");
        }
        for a in [
            &["link", "set", "eth0", "down"][..],
            &["addr", "add", "10.0.0.1/24", "dev", "eth0"],
            &["route", "flush", "all"],
            &["-batch", "f"],
            &["netns", "exec", "x", "sh"],
            &["-n", "x", "addr"],
            &[],
        ] {
            assert!(!super::argv_is_read_only("ip", &argv(a)), "ip {a:?}");
        }
        assert!(super::argv_is_read_only("ss", &argv(&["-tlnp"])));
        assert!(!super::argv_is_read_only(
            "ss",
            &argv(&["-K", "dport", "22"])
        ));
        assert!(!super::argv_is_read_only("ss", &argv(&["--kill"])));
    }

    #[test]
    fn a_read_of_denied_material_keeps_the_gate() {
        // SECURITY: a failure would mean a gate-exempt `cat` reads ~/.ssh
        // or a *.pem with no human asked — the traversal measured in exercises 06/08/10.
        assert!(!ro("cat", &argv(&["/home/u/.ssh/id_rsa"])));
        assert!(!ro("cat", &argv(&["~/.ssh/config"])));
        assert!(!ro("grep", &argv(&["-r", "key", "/srv/app/secrets/"])));
        assert!(!ro("head", &argv(&["--lines=5", "/etc/ssl/private/x.pem"])));
        // A symlink to denied material is followed before the verdict.
        let dir = std::env::temp_dir().join(format!("ro82-{}", std::process::id()));
        let ssh = dir.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("id_rsa"), b"k").unwrap();
        let link = dir.join("innocent.txt");
        let _ = std::os::unix::fs::symlink(ssh.join("id_rsa"), &link);
        assert!(!ro("cat", &argv(&[link.to_str().unwrap()])));
        std::fs::remove_dir_all(&dir).unwrap();
        // And an ordinary read stays exempt: the check does not void the exemption.
        assert!(ro("cat", &argv(&["README.md"])));
    }

    #[test]
    fn plain_reads_are_recognised() {
        assert!(ro("cat", &argv(&["README.md"])));
        assert!(ro("ls", &argv(&["-la", "playground"])));
        assert!(ro("grep", &argv(&["-rn", "foo", "src/"])));
        assert!(ro("wc", &argv(&["-l", "a.rs"])));
    }

    /// The gate keys on the program, not on the string that spelled it.
    #[test]
    fn an_absolute_path_names_the_same_program() {
        assert!(ro("/bin/cat", &argv(&["x"])));
        assert!(ro("/usr/bin/ls", &[]));
        assert!(!ro("/bin/rm", &argv(&["-rf", "/"])));
    }

    /// The premise of the whole module: `RunCommand` has no shell, so these
    /// are filenames, not syntax. If X0NE ever grows a shell-backed variant,
    /// this test is where that change must be noticed — a read-only verdict
    /// would then be handing over the shell's entire grammar.
    #[test]
    fn shell_metacharacters_are_inert_arguments() {
        assert!(ro("cat", &argv(&["a; rm -rf b"])));
        assert!(ro("cat", &argv(&["$(whoami)"])));
        assert!(ro("grep", &argv(&["x", "a | tee b"])));
    }

    /// A shell is the one program that would undo the no-shell premise, so no
    /// shell may be classed read-only however innocent the argv looks.
    #[test]
    fn no_shell_is_ever_read_only() {
        for sh in ["sh", "bash", "zsh", "dash", "fish", "/bin/sh", "/bin/bash"] {
            assert!(
                !ro(sh, &argv(&["-c", "cat README.md"])),
                "{sh} was classed read-only"
            );
        }
    }

    /// Programs that launch other programs inherit whatever they launch, so
    /// they are never read-only no matter what they are pointed at.
    #[test]
    fn launchers_are_never_read_only() {
        assert!(!ro("env", &argv(&["cat", "x"])));
        assert!(!ro("xargs", &argv(&["cat"])));
        assert!(!ro("timeout", &argv(&["5", "cat", "x"])));
        assert!(!ro("nohup", &argv(&["cat"])));
        assert!(!ro("watch", &argv(&["ls"])));
        assert!(!ro("sudo", &argv(&["cat", "/etc/shadow"])));
    }

    /// SECURITY: `sort -o` writes, and a read-only verdict removes the human
    /// gate from whatever it writes.
    ///
    /// `sort` lived in `ALWAYS_READ_ONLY` under a comment claiming none of its
    /// group "can open a file for writing". Measured on the demo enclave's own
    /// policy, 2026-08-15:
    /// `sort -o <outside-the-write-root>/planted.txt <home>/.ssh/id_rsa`
    /// succeeded — an SSH key, one that `ReadFile` refuses through
    /// `deny_path_patterns`, copied outside every configured root with no
    /// human asked. `RunCommand` does not confine argv paths (the argv-path gap),
    /// so this gate was the only barrier there was.
    ///
    /// A failure here means that hole is open again.
    #[test]
    fn sort_writing_to_a_file_is_not_read_only() {
        assert!(ro("sort", &argv(&["in.txt"])));
        assert!(ro("sort", &argv(&["-u", "-n", "in.txt"])));

        for writing in [
            argv(&["-o", "out.txt", "in.txt"]),
            argv(&["--output", "out.txt", "in.txt"]),
            argv(&["--output=out.txt", "in.txt"]),
            // Attached value, and clustered with another short flag: three
            // spellings of one flag, which is why the check is by letter.
            argv(&["-oout.txt", "in.txt"]),
            argv(&["-uo", "out.txt", "in.txt"]),
        ] {
            assert!(
                !ro("sort", &writing),
                "sort {writing:?} was classed read-only — it writes"
            );
        }
    }

    /// SECURITY: `getopt_long` takes any unambiguous abbreviation, so `--output`
    /// has six spellings and the check must catch all of them.
    ///
    /// `output` is the only long option beginning with `o` in either `sort` or
    /// `tree`, which makes `--o`, `--ou`, `--out`, `--outp` and `--outpu` all
    /// legal and all writing. Measured on coreutils 9.7, 2026-08-15: `sort
    /// --o=F IN`, `--ou=F`, `--outp=F` and `--o F IN` each created the file.
    /// The first version of this check asked `long.starts_with("output")`,
    /// which is false for `o=F` — so every abbreviation was classed read-only
    /// and dispensed from the gate. The comparison runs the other way now.
    ///
    /// A failure here means the abbreviations are dispensed again, and with
    /// them a write to any path the process can reach.
    #[test]
    fn every_abbreviation_of_output_is_caught() {
        for spelling in ["--o", "--ou", "--out", "--outp", "--outpu", "--output"] {
            // Separated value: `sort --ou F in.txt`.
            assert!(
                !ro("sort", &argv(&[spelling, "F", "in.txt"])),
                "sort {spelling} F was classed read-only"
            );
            // Attached value: `sort --ou=F in.txt`.
            let attached = format!("{spelling}=F");
            assert!(
                !ro("sort", &argv(&[&attached, "in.txt"])),
                "sort {attached} was classed read-only"
            );
        }
    }

    /// The abbreviation rule must not swallow every long flag.
    ///
    /// `"output".starts_with(name)` is a wide net, so this pins the other side:
    /// a long option that is NOT a prefix of `output`, and the bare `--`
    /// terminator, must leave the command a read. Without the emptiness guard
    /// `--` alone would match, since every string starts with "".
    #[test]
    fn long_flags_that_are_not_output_stay_read_only() {
        for benign in ["--unique", "--numeric-sort", "--reverse", "--stable", "--"] {
            assert!(
                ro("sort", &argv(&[benign, "in.txt"])),
                "sort {benign} was gated needlessly"
            );
        }
        // Separated short form: `-t` holds no `o`, and the `o` is an operand.
        assert!(ro("sort", &argv(&["-t", "o", "in.txt"])));
    }

    /// SECURITY: `rg --pre COMMAND` executes COMMAND, so no search tool with a
    /// preprocessor or a pager flag may sit in the unconditional table.
    ///
    /// Measured on ripgrep 14.1.1, 2026-08-15: `rg --pre ./pre.sh hello
    /// target.txt` ran the script and it wrote a canary file. `rg` was in
    /// `ALWAYS_READ_ONLY`, where membership dispenses every argv, so that
    /// execution never reached the human gate — a direct violation of the
    /// module's own rule, "no argv can make it write, delete, or execute".
    ///
    /// They are removed rather than given a predicate: their execution surface
    /// is version-scoped, so a denylist fails open on each new release. GNU
    /// `grep` keeps the dispensation and covers the same job.
    ///
    /// A failure here means one of them is back in the table and `--pre` is
    /// dispensed again.
    #[test]
    fn search_tools_that_can_execute_are_not_dispensed() {
        for tool in SEARCH_TOOLS_THAT_EXECUTE {
            assert!(
                !ALWAYS_READ_ONLY.contains(tool),
                "{tool} is in the unconditional table, so `--pre`/`--pager` is dispensed"
            );
            assert!(
                !ro(tool, &argv(&["--pre", "./evil.sh", "x", "."])),
                "{tool} --pre was classed read-only"
            );
            // Even the innocent invocation keeps the gate: that is the cost
            // of removal, and it is the intended one.
            assert!(!ro(tool, &argv(&["pattern", "src/"])));
        }
        // The one that stays, and why the cost is small.
        assert!(ro("grep", &argv(&["-rn", "pattern", "src/"])));
    }

    /// `uniq INPUT OUTPUT` writes its second operand (measured 2026-08-15).
    ///
    /// The count is deliberately loose: a separated flag value is counted as an
    /// operand, so `uniq -f 2 in.txt` is gated needlessly. A failure here in
    /// the other direction — a two-operand `uniq` classed read-only — is a
    /// write handed through without a question.
    #[test]
    fn uniq_with_an_output_operand_is_not_read_only() {
        assert!(ro("uniq", &argv(&["in.txt"])));
        assert!(ro("uniq", &argv(&["-c", "in.txt"])));
        assert!(ro("uniq", &[]));

        assert!(
            !ro("uniq", &argv(&["in.txt", "out.txt"])),
            "uniq IN OUT writes OUT"
        );
        assert!(
            !ro("uniq", &argv(&["-c", "in.txt", "out.txt"])),
            "a flag does not change which operand is the output"
        );
    }

    /// `tree -o FILE` writes. Documented behaviour of GNU tree; not measured
    /// here, `tree` is absent from this workstation. It was in the table with
    /// the filesystem inspectors, where every argv was dispensed.
    #[test]
    fn tree_writing_to_a_file_is_not_read_only() {
        assert!(ro("tree", &argv(&["-L", "2", "src"])));
        assert!(!ro("tree", &argv(&["-o", "out.txt", "."])));
        assert!(!ro("tree", &argv(&["--output=out.txt", "."])));
    }

    /// The property behind all three: a program whose write is a flag must be
    /// judged on argv, never on its name alone.
    ///
    /// Stated as a loop over the whole set so that adding a fourth such program
    /// to `ALWAYS_READ_ONLY` — where membership dispenses EVERY argv — fails
    /// here instead of shipping. A failure means some program now returns the
    /// same verdict for its reading and its writing form.
    #[test]
    fn no_program_that_writes_on_a_flag_sits_in_the_unconditional_table() {
        for (program, writing_argv) in [
            ("sort", argv(&["-o", "out.txt", "in.txt"])),
            ("uniq", argv(&["in.txt", "out.txt"])),
            ("tree", argv(&["-o", "out.txt", "."])),
        ] {
            assert!(
                !ALWAYS_READ_ONLY.contains(&program),
                "{program} is in the unconditional table, so its writing argv is dispensed"
            );
            assert!(
                !ro(program, &writing_argv),
                "{program} {writing_argv:?} was classed read-only"
            );
        }
    }

    /// `sed -i` and `awk`'s `print >` write. Neither is on the table, and this
    /// pins that they stay off it — they are the two most tempting additions.
    #[test]
    fn stream_editors_that_can_write_are_excluded() {
        assert!(!ro("sed", &argv(&["-n", "1,5p", "a.rs"])));
        assert!(!ro("awk", &argv(&["{print $1}", "a.rs"])));
    }

    /// Compiling executes `build.rs` and proc macros. `cargo check` looks
    /// inert and is not.
    #[test]
    fn no_cargo_subcommand_is_read_only() {
        for sub in ["check", "tree", "metadata", "test", "build"] {
            assert!(
                !ro("cargo", &argv(&[sub])),
                "cargo {sub} was classed read-only"
            );
        }
    }

    #[test]
    fn git_reads_pass_and_git_writes_do_not() {
        assert!(ro("git", &argv(&["status"])));
        assert!(ro("git", &argv(&["diff", "--stat"])));
        assert!(ro("git", &argv(&["log", "-5"])));

        for sub in ["checkout", "reset", "clean", "push", "commit", "rebase"] {
            assert!(!ro("git", &argv(&[sub])), "git {sub} was classed read-only");
        }
    }

    /// `git tag` lists and `git tag x` creates; `git remote` lists and `git
    /// remote add` writes. One word of argv separates them, so both stay off
    /// the list entirely rather than being split by a rule that would rot.
    #[test]
    fn git_subcommands_that_read_when_bare_are_excluded() {
        assert!(!ro("git", &argv(&["tag"])));
        assert!(!ro("git", &argv(&["remote"])));
        assert!(!ro("git", &argv(&["stash", "list"])));
    }

    /// A global flag before the subcommand can name the program git runs, so
    /// the subcommand must be argv[0] and nothing may precede it.
    #[test]
    fn a_global_flag_before_the_git_subcommand_is_refused() {
        assert!(!ro(
            "git",
            &argv(&["-c", "core.pager=sh -c 'rm -rf x'", "log"])
        ));
        assert!(!ro("git", &argv(&["--exec-path=/tmp/evil", "status"])));
        assert!(!ro("git", &argv(&["diff", "--ext-diff"])));
    }

    /// `find` is a reporter until `-exec` makes it a launcher.
    #[test]
    fn find_reports_but_never_acts() {
        assert!(ro("find", &argv(&[".", "-name", "*.rs"])));
        assert!(!ro("find", &argv(&[".", "-delete"])));
        assert!(!ro("find", &argv(&[".", "-exec", "rm", "{}", ";"])));
        assert!(!ro("fd", &argv(&["-x", "rm"])));
    }

    /// An unknown program keeps the gate. This is the default the whole
    /// module rests on: the table is an allowlist, never a denylist.
    #[test]
    fn an_unknown_program_keeps_the_gate() {
        assert!(!ro("my-deploy-script", &[]));
        assert!(!ro("", &[]));
        assert!(!ro("/", &[]));
    }

    /// SECURITY: an action carrying an `env` map is never dispensed.
    ///
    /// The defect of 2026-08-15, at this end of it. The verdict was taken from
    /// `command` and `args` while a third field decided which program those two
    /// named: `{"command":"cat","args":["f"],"env":{"PATH":"/tmp/evil"}}` ran
    /// `/tmp/evil/cat` and the operator was never asked, because `cat` is on
    /// the table.
    ///
    /// The rule is deliberately wider than the runtime's key filter: ANY
    /// non-empty map keeps the gate, including a harmless `MY_VAR`. Narrowing
    /// it to "no program-choosing key" would make this file's verdict depend on
    /// a list in another crate, and the day that list gains an entry this one
    /// does not, the gap is silent — the exact shape of this repository's
    /// recurring defect.
    ///
    /// A failure here means an invisible field can once again decide what runs
    /// behind an approved name.
    #[test]
    fn an_action_with_env_is_never_dispensed_from_the_gate() {
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/tmp/evil".to_string());
        assert!(
            !is_read_only_command("cat", &argv(&["README.md"]), &env, None),
            "a model-supplied PATH was dispensed from the human gate"
        );

        // Not only the dangerous keys: the map itself is a second input to the
        // child that the approval prompt does not render.
        let mut ordinary = HashMap::new();
        ordinary.insert("MY_VAR".to_string(), "1".to_string());
        assert!(!is_read_only_command(
            "cat",
            &argv(&["README.md"]),
            &ordinary,
            None
        ));
        assert!(!is_read_only_command("ls", &[], &ordinary, None));

        // And the common case is untouched: an empty map still dispenses.
        assert!(is_read_only_command(
            "cat",
            &argv(&["README.md"]),
            &HashMap::new(),
            None
        ));
    }

    /// SECURITY: reading another process's environment asks the human.
    ///
    /// The August 2026 fix closed what a child INHERITS. It did nothing about
    /// what a child can go and read, and the process next door is the
    /// orchestrator, which holds every key in `~/.x0ne/.env`. Measured end to
    /// end on 2026-08-15 with two dispensed commands:
    ///
    /// ```text
    /// cat /proc/self/status    → PPid: <the orchestrator>
    /// cat /proc/<ppid>/environ → ANTHROPIC_API_KEY=…
    /// ```
    ///
    /// The bound is stated in `names_a_process_environment`: this removes the
    /// dispensation, not the access. `RunCommand` argv paths are unconfined
    /// (the argv-path gap), so an approved read still succeeds — the change is that a
    /// human sees it first.
    #[test]
    fn reading_a_parents_environ_asks_the_human() {
        for path in [
            "/proc/self/environ",
            "/proc/1/environ",
            "/proc/12345/environ",
            "/proc/12345/task/12346/environ",
            // Relative, for the `cwd=/proc/N` route. `cwd` never reaches this
            // function, so the final component is what there is to match on.
            "environ",
        ] {
            assert!(
                !is_read_only_command("cat", &argv(&[path]), &HashMap::new(), None),
                "{path} was dispensed: the orchestrator's API keys are readable \
                 with no human asked"
            );
            assert!(!is_read_only_command(
                "head",
                &argv(&["-c", "4096", path]),
                &HashMap::new(),
                None
            ));
            assert!(!is_read_only_command(
                "grep",
                &argv(&["KEY", path]),
                &HashMap::new(),
                None
            ));
        }

        // The rest of /proc is not an environment; whether it is dispensed is
        // the whitelist's question (`a_read_outside_the_working_directory…`).
        assert!(!names_a_process_environment(&argv(&["/proc/self/status"])));
        assert!(!names_a_process_environment(&argv(&["/proc/cpuinfo"])));
        assert!(!names_a_process_environment(&argv(&["environment.md"])));
    }

    /// SECURITY: `.env` holds a project's secrets and `~/.x0ne/.env` every
    /// API key of the orchestrator; neither is on the shared deny list, so a
    /// dispensed `cat` reads them with nobody asked. Red on 2026-10-05.
    #[test]
    fn a_read_of_a_dotenv_file_keeps_the_gate() {
        for path in [
            ".env",
            "./.env",
            "config/.env.local",
            "~/.x0ne/.env",
            "/srv/app/.env",
        ] {
            assert!(!ro("cat", &argv(&[path])), "{path} was dispensed");
            assert!(
                !ro("grep", &argv(&["KEY", path])),
                "{path} was dispensed to grep"
            );
        }
    }

    /// The dispensation is a whitelist of the working directory, not a list of
    /// known secret stores: a read that leaves the directory asks a human.
    /// Red on 2026-10-05: an absolute path anywhere on the host was dispensed.
    #[test]
    fn a_read_outside_the_working_directory_keeps_the_gate() {
        let outside = tempfile::tempdir().expect("a directory outside the workspace");
        let file = outside.path().join("notes.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(!ro("cat", &argv(&[file.to_str().unwrap()])));
        assert!(!ro("head", &argv(&["-n", "1", "/var/log/syslog"])));
        assert!(!ro("cat", &argv(&["../../../../../../home/u/notes.txt"])));
        assert!(!ro(
            "grep",
            &argv(&["-r", "x", "--include=*.rs", "/usr/src"])
        ));
        // Relative reads inside the directory stay dispensed.
        assert!(ro("cat", &argv(&["README.md"])));
        assert!(ro("grep", &argv(&["-rn", "pattern", "src/"])));
    }
}
