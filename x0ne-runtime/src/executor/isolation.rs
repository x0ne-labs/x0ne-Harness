//! OS-level confinement of the commands the agent starts.
//!
//! Every agent-chosen spawn (`RunCommand`, `Listen`, `SafeTest`,
//! `ArtifactServe`) goes through [`plan`]. In the default mode the child runs
//! under bubblewrap: root filesystem read-only, `allowed_write_paths` bound
//! read-write, a private `/tmp`, secret directories hidden, own PID/IPC/UTS
//! namespaces, and its own network namespace (loopback only).
//!
//! When bubblewrap cannot run, [`plan`] refuses. There is no fallback to the
//! host: that takes `--isolation none` on the runtime's command line.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::config::{
    DesktopConfig, IsolationConfig, IsolationMode, IsolationNetwork, PolicyConfig,
};

/// Hidden from every isolated command, relative to `$HOME`. Directories get an
/// empty tmpfs, files get `/dev/null`. Entries that do not exist are skipped.
pub const DEFAULT_HIDDEN_HOME_PATHS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".kube",
    ".docker",
    ".x0ne",
    ".x0ne-secrets",
    ".config/x0ne",
    ".config/gh",
    ".config/hub",
    ".netrc",
    ".git-credentials",
    ".password-store",
    ".local/share/keyrings",
    ".pgpass",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".vault-token",
    ".terraform.d",
    ".mozilla",
    ".thunderbird",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    // Other agents' credentials (audit 2026-11-02, A1).
    ".claude/.credentials.json",
    ".codex/auth.json",
];

/// Daemon sockets reachable through a supplementary group, which bwrap keeps:
/// measured 2026-09-23, `incus-admin` reached `/var/lib/incus/unix.socket`
/// from inside the sandbox (root-equivalent). The parent directories are
/// hidden, since a read-only mount does not stop `connect()`.
pub const DEFAULT_HIDDEN_ABS_PATHS: &[&str] = &[
    "/var/lib/incus",
    "/var/lib/lxd",
    "/var/snap/lxd/common/lxd",
    "/run/incus",
    "/run/libvirt",
    "/run/docker.sock",
    "/run/docker",
    "/run/containerd",
    "/run/podman",
];

/// Fixed locations, never `$PATH`: a PATH entry the agent can write to must not
/// be able to substitute a fake `bwrap` that just execs its argument.
const BWRAP_LOCATIONS: &[&str] = &["/usr/bin/bwrap", "/usr/local/bin/bwrap", "/bin/bwrap"];

/// `io::ErrorKind` carried by a refusal, so spawn helpers returning
/// `io::Result` can hand it up without a new error type.
pub const UNAVAILABLE_KIND: io::ErrorKind = io::ErrorKind::Unsupported;

/// What confines the next agent command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confinement {
    Bwrap(PathBuf),
    /// `--isolation container`: the runtime runs inside Incus/Podman.
    Container,
    /// `--isolation none`: host execution, chosen by a human.
    HostOptOut,
}

/// Per-site adjustments. `Default` is the strict profile.
#[derive(Debug, Clone, Default)]
pub struct Options<'a> {
    /// One more read-write root (the artifact directory of `ArtifactServe`).
    pub extra_write: Option<&'a Path>,
    /// Share the host network regardless of `--isolation-network`. Only for a
    /// server the host must reach (`ArtifactServe`).
    pub share_network: bool,
    /// Keep the controlling terminal (no `--new-session`), for a program run in
    /// a terminal the runtime owns: C-c and job control need it. TIOCSTI can
    /// then only inject into that same terminal.
    pub keep_tty: bool,
    /// More paths to hide (the private tmux socket directory).
    pub extra_hidden: &'a [PathBuf],
}

/// The program to spawn, its argv, and the directory to spawn it in.
#[derive(Debug, Clone)]
pub struct Spawn {
    pub program: OsString,
    pub args: Vec<OsString>,
    /// Set only for direct spawns; under bwrap the directory is `--chdir`.
    pub current_dir: Option<PathBuf>,
    pub confinement: Confinement,
}

/// Resolve the confinement for this configuration. Probes bubblewrap once per
/// binary path and caches the verdict.
pub fn confinement(iso: &IsolationConfig) -> Result<Confinement, String> {
    match iso.mode {
        IsolationMode::Container => {
            let root = iso
                .container_probe_root
                .as_deref()
                .unwrap_or(Path::new("/"));
            return match container_evidence(root) {
                Some(_) => Ok(Confinement::Container),
                None => Err(format!(
                    "--isolation container, but no container was detected (none of {} \
                     under {})",
                    CONTAINER_MARKERS.join(", "),
                    root.display()
                )),
            };
        }
        IsolationMode::None => return Ok(Confinement::HostOptOut),
        IsolationMode::Bwrap => {}
    }
    let bin = match &iso.bwrap_override {
        Some(p) => p.clone(),
        None => match BWRAP_LOCATIONS
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
        {
            Some(p) => p,
            None => {
                return Err(format!(
                    "bubblewrap not found (looked in {})",
                    BWRAP_LOCATIONS.join(", ")
                ))
            }
        },
    };
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Result<(), String>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    let verdict = guard
        .entry(bin.clone())
        .or_insert_with(|| probe_bwrap(&bin))
        .clone();
    verdict.map(|()| Confinement::Bwrap(bin))
}

/// Files a container engine leaves in its guests: Podman, Docker, systemd's
/// `container` detection, the Incus/LXD guest API. The process environment is
/// not consulted: it is the caller's to set.
pub const CONTAINER_MARKERS: &[&str] = &[
    "run/.containerenv",
    ".dockerenv",
    "run/systemd/container",
    "dev/incus",
    "dev/lxd",
    "proc/1/environ:container=",
];

/// The first marker found under `root`, or `None` on a bare host.
pub fn container_evidence(root: &Path) -> Option<&'static str> {
    CONTAINER_MARKERS
        .iter()
        .copied()
        .find(|m| match m.split_once(':') {
            Some((file, needle)) => std::fs::read(root.join(file))
                .map(|b| {
                    b.split(|c| *c == 0)
                        .any(|kv| kv.starts_with(needle.as_bytes()))
                })
                .unwrap_or(false),
            None => root.join(m).exists(),
        })
}

static CONFINE_HELPER: OnceLock<PathBuf> = OnceLock::new();

/// `x0ne-runtime`'s `main` registers its own binary as the confine helper.
pub fn register_confine_helper(exe: PathBuf) {
    let _ = CONFINE_HELPER.set(exe);
}

/// The binary bwrap starts first (`sandbox_child`). Never `$PATH`: the test
/// seam, the registered runtime, else `x0ne-runtime` next to this binary.
fn confine_helper(iso: &IsolationConfig, writes: &[PathBuf]) -> Result<PathBuf, String> {
    let p = match (&iso.confine_helper_override, CONFINE_HELPER.get()) {
        (Some(p), _) | (None, Some(p)) => p.clone(),
        (None, None) => {
            let exe = std::env::current_exe()
                .map_err(|e| format!("cannot locate the confine helper: {e}"))?;
            let sibling = exe.with_file_name("x0ne-runtime");
            // Unit tests run from target/<profile>/deps; the binary sits one up.
            #[cfg(test)]
            let sibling = match exe.parent().and_then(Path::parent) {
                Some(up) if !sibling.is_file() => up.join("x0ne-runtime"),
                _ => sibling,
            };
            sibling
        }
    };
    let real = std::fs::canonicalize(&p)
        .map_err(|e| format!("confine helper {} unusable: {e}", p.display()))?;
    // The agent could replace a helper it can write, and drop this layer.
    if writes.iter().any(|w| under(&real, w)) {
        return Err(format!(
            "confine helper {} lies inside a writable root",
            real.display()
        ));
    }
    Ok(real)
}

/// One-line status for the startup banner.
pub fn describe(iso: &IsolationConfig) -> String {
    let net = match iso.network {
        IsolationNetwork::Off => "network off",
        IsolationNetwork::Host => "network SHARED with host",
    };
    match confinement(iso) {
        Ok(Confinement::Bwrap(p)) => format!("bwrap ({}, {net})", p.display()),
        Ok(Confinement::Container) => "container (--isolation container)".into(),
        Ok(Confinement::HostOptOut) => "NONE: commands run on this host (--isolation none)".into(),
        Err(why) => format!("UNAVAILABLE ({why}): agent commands will be refused"),
    }
}

/// The refusal text. It names the human-only way out and nothing else.
pub fn refusal(why: &str) -> String {
    format!(
        "command not run: OS isolation is unavailable ({why}). Install bubblewrap \
         and allow unprivileged user namespaces, or restart the runtime with \
         `--isolation none` to run agent commands directly on this host."
    )
}

/// Run the smallest sandbox bwrap can build. Its stderr is the reason given
/// to the operator (e.g. user namespaces disabled).
fn probe_bwrap(bin: &Path) -> Result<(), String> {
    let mut cmd = std::process::Command::new(bin);
    crate::child_env::harden(&mut cmd);
    let out = cmd
        .args([
            "--unshare-all",
            "--die-with-parent",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--",
            "true",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!(
            "{} failed: {}",
            bin.display(),
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Err(format!("{}: {e}", bin.display())),
    }
}

/// Build the spawn for an agent command.
///
/// Errors: `NotFound` when the command or `cwd` does not exist (checked on the
/// host so `disambiguate_enoent` keeps working under bwrap), and
/// [`UNAVAILABLE_KIND`] carrying [`refusal`] when no isolation can be built.
pub fn plan(
    iso: &IsolationConfig,
    policy: &PolicyConfig,
    desktop: &DesktopConfig,
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    opts: &Options<'_>,
) -> io::Result<Spawn> {
    let confinement =
        confinement(iso).map_err(|why| io::Error::new(UNAVAILABLE_KIND, refusal(&why)))?;
    let locked = !desktop.allowed_apps.is_empty();
    if locked && !matches!(confinement, Confinement::Bwrap(_)) {
        return Err(desktop_refusal(
            "the runtime was started with --isolation none or container, so commands run unconfined",
        ));
    }
    let bin = match &confinement {
        Confinement::Bwrap(bin) => bin.clone(),
        _ => {
            return Ok(Spawn {
                program: command.into(),
                args: args.iter().map(OsString::from).collect(),
                current_dir: cwd.map(PathBuf::from),
                confinement,
            })
        }
    };

    let given_cwd = cwd;
    let cwd = match cwd {
        Some(d) => {
            let p = PathBuf::from(d);
            if !p.is_dir() {
                return Err(io::Error::new(io::ErrorKind::NotFound, format!("cwd {d}")));
            }
            std::fs::canonicalize(&p)?
        }
        None => std::env::current_dir()?,
    };
    if !command_exists(command, &cwd) {
        return Err(io::Error::new(io::ErrorKind::NotFound, command.to_string()));
    }

    let mut writes = crate::executor::confine::normalize_roots(&policy.allowed_write_paths);
    if let Some(extra) = opts.extra_write.and_then(|p| std::fs::canonicalize(p).ok()) {
        writes.push(extra);
    }
    let reads = crate::executor::confine::normalize_roots(&policy.allowed_read_paths);
    // H1-1 (2026-10-30): the cwd used to be a Landlock read root on its own, so
    // `cat token` with `cwd` under $HOME read what no policy root covered. The
    // cwd is where the command runs, not a reason to read it: `read_roots` no
    // longer lists it, and one the action names outside every root is refused
    // before anything is spawned. With no cwd the command runs in the
    // runtime's own working directory, chosen by the operator, under the same
    // Landlock rules.
    if given_cwd.is_some() && !reads.iter().chain(writes.iter()).any(|r| under(&cwd, r)) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "command not run: cwd {} lies outside every [policy] allowed_read_paths \
                 and allowed_write_paths root. A command runs inside a root of the \
                 policy: set cwd to a directory under one, or omit it.",
                cwd.display()
            ),
        ));
    }
    let home = dirs::home_dir();
    let mut hidden = hidden_paths(home.as_deref(), &iso.hidden_paths);
    for extra in opts.extra_hidden {
        if let Ok(real) = std::fs::canonicalize(extra) {
            if !hidden.iter().any(|h| h.path == real) {
                hidden.push(Hidden {
                    is_dir: real.is_dir(),
                    path: real,
                });
            }
        }
    }
    let share_net = opts.share_network || iso.network == IsolationNetwork::Host;
    // The session sockets, hidden from every command (not only under the
    // list): a hide is by path, so one inside a write root could be renamed by
    // a file action and reached under another name.
    for s in desktop_sockets(|k| std::env::var_os(k)) {
        let Ok(real) = std::fs::canonicalize(&s) else {
            continue;
        };
        if locked && writes.iter().any(|w| under(&real, w)) {
            return Err(desktop_refusal(&format!(
                "the session socket {} lies inside a write root",
                real.display()
            )));
        }
        if !hidden.iter().any(|h| h.path == real) {
            hidden.push(Hidden {
                is_dir: real.is_dir(),
                path: real,
            });
        }
    }
    if locked && share_net && landlock_abi() < 6 {
        return Err(desktop_refusal(
            "the network is shared on a kernel whose Landlock (ABI < 6) cannot scope abstract \
             sockets, where an X server listens",
        ));
    }

    let helper = confine_helper(iso, &writes)
        .map_err(|why| io::Error::new(UNAVAILABLE_KIND, refusal(&why)))?;
    let mut argv = bwrap_args(&writes, &reads, &hidden, &cwd, share_net);
    if opts.keep_tty {
        argv.retain(|a| a != "--new-session");
    }
    let spec = crate::executor::sandbox_child::Spec {
        limits: crate::executor::sandbox_child::Limits::from_config(&iso.limits),
        read: crate::executor::sandbox_child::read_roots(home.as_deref(), &reads),
        write: writes.clone(),
        argv: std::iter::once(OsString::from(command))
            .chain(args.iter().map(OsString::from))
            .collect(),
    };
    argv.push("--".into());
    argv.push(helper.into_os_string());
    argv.push(crate::executor::sandbox_child::ARG.into());
    argv.extend(spec.to_args());
    Ok(Spawn {
        program: bin.into_os_string(),
        args: argv,
        current_dir: None,
        confinement,
    })
}

/// A hidden path, resolved on the host, and whether it is a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hidden {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// Built-in list + operator extras + the per-user runtime directory (agent
/// sockets: gpg-agent, ssh-agent, D-Bus). Symlinks are resolved so the target
/// is hidden too; missing entries are skipped (bwrap cannot mount on them).
pub fn hidden_paths(home: Option<&Path>, extra: &[String]) -> Vec<Hidden> {
    let mut raw: Vec<PathBuf> = Vec::new();
    if let Some(h) = home {
        raw.extend(DEFAULT_HIDDEN_HOME_PATHS.iter().map(|r| h.join(r)));
    }
    raw.extend(DEFAULT_HIDDEN_ABS_PATHS.iter().map(PathBuf::from));
    raw.extend(extra.iter().map(|e| crate::config::expand_tilde(e)));
    if let Ok(meta) = std::fs::metadata("/proc/self") {
        raw.push(PathBuf::from(format!("/run/user/{}", meta.uid())));
    }
    if let Some(x) = std::env::var_os("XDG_RUNTIME_DIR") {
        raw.push(PathBuf::from(x));
    }
    let mut out: Vec<Hidden> = Vec::new();
    for p in raw {
        let Ok(real) = std::fs::canonicalize(&p) else {
            continue;
        };
        if real == Path::new("/") || out.iter().any(|h| h.path == real) {
            continue;
        }
        out.push(Hidden {
            is_dir: real.is_dir(),
            path: real,
        });
    }
    out
}

/// The session's display and input sockets, named by the variables the
/// desktop tools read. `get` reads one variable (the process environment in
/// production).
pub fn desktop_sockets(get: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = ["SWAYSOCK", "I3SOCK", "XAUTHORITY", "YDOTOOL_SOCKET"]
        .iter()
        .filter_map(|k| get(k).filter(|v| !v.is_empty()).map(PathBuf::from))
        .collect();
    if let Some(w) = get("WAYLAND_DISPLAY").filter(|v| !v.is_empty()) {
        let w = PathBuf::from(w);
        match get("XDG_RUNTIME_DIR") {
            _ if w.is_absolute() => out.push(w),
            Some(dir) => out.push(PathBuf::from(dir).join(w)),
            None => {}
        }
    }
    // X11 sockets and ydotoold's default; the private /tmp hides them unless
    // a policy root re-binds /tmp.
    out.extend(DESKTOP_FIXED_PATHS.iter().map(PathBuf::from));
    out
}

/// Session paths at fixed locations (X11 sockets, ydotoold's default socket).
pub const DESKTOP_FIXED_PATHS: &[&str] = &["/tmp/.X11-unix", "/tmp/.ydotool_socket"];

/// Landlock ABI of the running kernel, 0 without Landlock. ABI 6 scopes
/// abstract unix sockets (an X server listens on one).
#[cfg(target_os = "linux")]
fn landlock_abi() -> i64 {
    // SAFETY: LANDLOCK_CREATE_RULESET_VERSION with a null attr and size 0
    // only returns the ABI version.
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0usize,
            1u32,
        )
    };
    v.max(0)
}

/// Landlock is Linux only: elsewhere the ABI is 0 and the caller refuses.
#[cfg(not(target_os = "linux"))]
fn landlock_abi() -> i64 {
    0
}

/// Refusal of an agent command while `desktop.allowed_apps` is set.
fn desktop_refusal(why: &str) -> io::Error {
    io::Error::new(
        UNAVAILABLE_KIND,
        format!(
            "desktop.allowed_apps is set and {why}: the command could reach the display, \
             the compositor or an input device and drive a window outside the list. \
             Agent commands are refused until this is fixed (or the list is emptied)."
        ),
    )
}

fn command_exists(command: &str, cwd: &Path) -> bool {
    if command.contains('/') {
        return cwd.join(command).is_file();
    }
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|d| d.join(command).is_file()))
        .unwrap_or(false)
}

fn under(p: &Path, root: &Path) -> bool {
    crate::executor::confine::is_within_roots(p, std::slice::from_ref(&root.to_path_buf()))
}

/// The bwrap options, in mount order. Order is the security property:
/// write roots, the private /tmp (read and write roots under it re-bound),
/// then the hides (which also cover write roots containing them), then write
/// roots that lie INSIDE a hidden path, then the hides made read-only.
pub fn bwrap_args(
    writes: &[PathBuf],
    reads: &[PathBuf],
    hidden: &[Hidden],
    cwd: &Path,
    share_net: bool,
) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |xs: &[&std::ffi::OsStr]| a.extend(xs.iter().map(|x| x.to_os_string()));
    push(&[
        "--die-with-parent".as_ref(),
        "--new-session".as_ref(),
        "--unshare-all".as_ref(),
    ]);
    if share_net {
        push(&["--share-net".as_ref()]);
    }
    push(&["--ro-bind".as_ref(), "/".as_ref(), "/".as_ref()]);
    for w in writes {
        push(&["--bind".as_ref(), w.as_os_str(), w.as_os_str()]);
    }
    push(&[
        "--dev".as_ref(),
        "/dev".as_ref(),
        "--proc".as_ref(),
        "/proc".as_ref(),
        "--tmpfs".as_ref(),
        "/tmp".as_ref(),
    ]);
    // Host /tmp holds other sessions' sockets (X11, ssh-agent): private by
    // default, only the configured roots and the cwd come through.
    let tmp = Path::new("/tmp");
    let cwd_writable = writes.iter().any(|w| under(cwd, w));
    let mut ro_in_tmp: Vec<&PathBuf> = reads.iter().filter(|r| under(r, tmp)).collect();
    let cwd_buf = cwd.to_path_buf();
    if !cwd_writable && under(cwd, tmp) {
        ro_in_tmp.push(&cwd_buf);
    }
    for r in ro_in_tmp {
        push(&["--ro-bind".as_ref(), r.as_os_str(), r.as_os_str()]);
    }
    for w in writes.iter().filter(|w| under(w, tmp)) {
        push(&["--bind".as_ref(), w.as_os_str(), w.as_os_str()]);
    }
    for h in hidden {
        if h.is_dir {
            push(&["--tmpfs".as_ref(), h.path.as_os_str()]);
        } else {
            push(&[
                "--ro-bind".as_ref(),
                "/dev/null".as_ref(),
                h.path.as_os_str(),
            ]);
        }
    }
    for w in writes
        .iter()
        .filter(|w| hidden.iter().any(|h| h.path != **w && under(w, &h.path)))
    {
        push(&["--bind".as_ref(), w.as_os_str(), w.as_os_str()]);
    }
    // Last: a hide that accepted writes would let the agent plant files the
    // next command reads as real config.
    for h in hidden.iter().filter(|h| h.is_dir) {
        push(&["--remount-ro".as_ref(), h.path.as_os_str()]);
    }
    // TMPDIR may point into a hidden or unmounted directory (/run/user/…).
    push(&["--setenv".as_ref(), "TMPDIR".as_ref(), "/tmp".as_ref()]);
    push(&["--chdir".as_ref(), cwd.as_os_str()]);
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    fn strs(v: &[OsString]) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    /// Position of the first `flag path` pair in an argv.
    fn pos(a: &[String], flag: &str, path: &str) -> Option<usize> {
        a.windows(2).position(|w| w[0] == flag && w[1] == path)
    }

    #[test]
    fn a_hide_is_mounted_after_the_write_root_that_contains_it() {
        let home = PathBuf::from("/home/u");
        let hidden = vec![Hidden {
            path: home.join(".ssh"),
            is_dir: true,
        }];
        let a = strs(&bwrap_args(
            std::slice::from_ref(&home),
            &[],
            &hidden,
            &home,
            false,
        ));
        let bind = pos(&a, "--bind", "/home/u").unwrap();
        let hide = pos(&a, "--tmpfs", "/home/u/.ssh").unwrap();
        assert!(bind < hide, "{a:?}");
        assert!(a.contains(&"--unshare-all".to_string()));
        assert!(!a.contains(&"--share-net".to_string()));
    }

    #[test]
    fn a_write_root_inside_a_hidden_dir_is_bound_again_after_the_hide() {
        let hidden = vec![Hidden {
            path: PathBuf::from("/home/u/.x0ne"),
            is_dir: true,
        }];
        let w = PathBuf::from("/home/u/.x0ne/work");
        let a = strs(&bwrap_args(
            std::slice::from_ref(&w),
            &[],
            &hidden,
            &w,
            false,
        ));
        let hide = pos(&a, "--tmpfs", "/home/u/.x0ne").unwrap();
        let last_bind = a
            .windows(2)
            .rposition(|x| x[0] == "--bind" && x[1] == "/home/u/.x0ne/work")
            .unwrap();
        assert!(hide < last_bind, "{a:?}");
    }

    #[test]
    fn hidden_files_get_dev_null_and_network_sharing_is_explicit() {
        let hidden = vec![Hidden {
            path: PathBuf::from("/home/u/.netrc"),
            is_dir: false,
        }];
        let a = strs(&bwrap_args(&[], &[], &hidden, Path::new("/"), true));
        assert!(pos(&a, "--ro-bind", "/dev/null").is_some(), "{a:?}");
        assert!(a.contains(&"--share-net".to_string()));
    }

    #[test]
    fn a_missing_bwrap_is_a_refusal_never_a_host_spawn() {
        let mut cfg = AppConfig::defaults();
        cfg.isolation.bwrap_override = Some(PathBuf::from("/nonexistent/x0ne-test/bwrap"));
        let err = plan(
            &cfg.isolation,
            &cfg.policy,
            &cfg.desktop,
            "true",
            &[],
            None,
            &Options::default(),
        )
        .unwrap_err();
        assert_eq!(err.kind(), UNAVAILABLE_KIND);
        assert!(err.to_string().contains("--isolation none"), "{err}");
    }

    #[test]
    fn the_host_is_reached_only_through_an_explicit_mode() {
        let mut cfg = AppConfig::defaults();
        assert_eq!(cfg.isolation.mode, IsolationMode::Bwrap);
        cfg.isolation.mode = IsolationMode::None;
        let s = plan(
            &cfg.isolation,
            &cfg.policy,
            &cfg.desktop,
            "true",
            &[],
            None,
            &Options::default(),
        )
        .unwrap();
        assert_eq!(s.confinement, Confinement::HostOptOut);
        assert_eq!(s.program, OsString::from("true"));
    }

    /// `--isolation container` on a bare host used to spawn directly.
    #[test]
    fn container_mode_outside_a_container_is_a_refusal() {
        let bare = tempfile::tempdir().unwrap();
        let mut cfg = AppConfig::defaults();
        cfg.isolation.mode = IsolationMode::Container;
        cfg.isolation.container_probe_root = Some(bare.path().to_path_buf());
        let err = plan(
            &cfg.isolation,
            &cfg.policy,
            &cfg.desktop,
            "true",
            &[],
            None,
            &Options::default(),
        )
        .unwrap_err();
        assert_eq!(err.kind(), UNAVAILABLE_KIND);
        assert!(
            err.to_string().contains("no container was detected"),
            "{err}"
        );
    }

    #[test]
    fn container_mode_inside_a_container_spawns_directly() {
        for marker in ["run/.containerenv", ".dockerenv", "dev/incus"] {
            let root = tempfile::tempdir().unwrap();
            let p = root.path().join(marker);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            let mut cfg = AppConfig::defaults();
            cfg.isolation.mode = IsolationMode::Container;
            cfg.isolation.container_probe_root = Some(root.path().to_path_buf());
            let s = plan(
                &cfg.isolation,
                &cfg.policy,
                &cfg.desktop,
                "true",
                &[],
                None,
                &Options::default(),
            )
            .unwrap();
            assert_eq!(s.confinement, Confinement::Container, "{marker}");
        }
    }

    #[test]
    fn pid1_environment_marks_a_container_but_a_lookalike_does_not() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("proc/1")).unwrap();
        let env = root.path().join("proc/1/environ");
        std::fs::write(&env, b"PATH=/bin\0NOTcontainer=lxc\0").unwrap();
        assert_eq!(container_evidence(root.path()), None);
        std::fs::write(&env, b"PATH=/bin\0container=lxc\0").unwrap();
        assert_eq!(
            container_evidence(root.path()),
            Some("proc/1/environ:container=")
        );
    }

    /// The weakening knobs are not TOML keys: a project-supplied runtime file
    /// cannot switch isolation off.
    #[test]
    fn toml_cannot_turn_isolation_off() {
        let cfg: AppConfig = toml::from_str(
            "[isolation]\nmode = \"none\"\nnetwork = \"host\"\nhidden_paths = [\"~/extra\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.isolation.mode, IsolationMode::Bwrap);
        assert_eq!(cfg.isolation.network, IsolationNetwork::Off);
        assert_eq!(cfg.isolation.hidden_paths, vec!["~/extra".to_string()]);
    }

    fn locked() -> AppConfig {
        let mut cfg = AppConfig::defaults();
        cfg.desktop.allowed_apps = vec!["firefox".into()];
        cfg
    }

    fn planned(cfg: &AppConfig) -> io::Result<Spawn> {
        plan(
            &cfg.isolation,
            &cfg.policy,
            &cfg.desktop,
            "true",
            &[],
            None,
            &Options::default(),
        )
    }

    #[test]
    fn the_desktop_lock_refuses_every_unconfined_mode() {
        let mut cfg = locked();
        cfg.isolation.mode = IsolationMode::None;
        let err = planned(&cfg).unwrap_err();
        assert_eq!(err.kind(), UNAVAILABLE_KIND);
        assert!(err.to_string().contains("allowed_apps"), "{err}");

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".dockerenv"), "").unwrap();
        cfg.isolation.mode = IsolationMode::Container;
        cfg.isolation.container_probe_root = Some(root.path().to_path_buf());
        let err = planned(&cfg).unwrap_err();
        assert_eq!(err.kind(), UNAVAILABLE_KIND);
        assert!(err.to_string().contains("allowed_apps"), "{err}");

        cfg.desktop.allowed_apps.clear();
        assert_eq!(planned(&cfg).unwrap().confinement, Confinement::Container);
    }

    #[test]
    fn desktop_sockets_come_from_the_session_variables() {
        let env = |k: &str| -> Option<OsString> {
            match k {
                "XDG_RUNTIME_DIR" => Some("/run/user/7".into()),
                "WAYLAND_DISPLAY" => Some("wayland-1".into()),
                "SWAYSOCK" => Some("/srv/s/sway.sock".into()),
                "I3SOCK" => Some("/srv/s/i3.sock".into()),
                "XAUTHORITY" => Some("/srv/s/.Xauthority".into()),
                "YDOTOOL_SOCKET" => Some("/srv/s/ydotool".into()),
                "DISPLAY" => Some(":3".into()),
                _ => None,
            }
        };
        let got = desktop_sockets(env);
        for want in [
            "/run/user/7/wayland-1",
            "/srv/s/sway.sock",
            "/srv/s/i3.sock",
            "/srv/s/.Xauthority",
            "/srv/s/ydotool",
            "/tmp/.X11-unix",
            "/tmp/.ydotool_socket",
        ] {
            assert!(
                got.contains(&PathBuf::from(want)),
                "{want} missing: {got:?}"
            );
        }
        let abs = |k: &str| (k == "WAYLAND_DISPLAY").then(|| OsString::from("/srv/w/wl-0"));
        assert!(desktop_sockets(abs).contains(&PathBuf::from("/srv/w/wl-0")));
    }
}
