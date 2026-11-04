//! X0NE Artifact — generate, serve, save and supervise web artifacts.
//!
//! A *artifact* is a self-contained directory of web assets (HTML/CSS/JS, plus
//! optional backend code) that X0NE can:
//!
//! * **Create** — allocate a fresh directory under `~/.x0ne/artifacts/…`
//! * **Write into** via the normal `WriteFile`/`AppendFile` actions
//! * **Serve** — start an HTTP service (long-running process) inside it
//! * **Expose** to the browser via a single reverse proxy (default
//!   `127.0.0.1:7800`, URL `http://localhost:7800/a/<id>/` — override host for
//!   links with `X0NE_ARTIFACT_PUBLIC_HOST`, health-probe address with
//!   `X0NE_ARTIFACT_PROXY_CHECK_HOST`)
//! * **Save** — promote an ephemeral artifact to a persistent one that survives
//!   session termination
//! * **List / Stop / Destroy**
//!
//! This crate owns the on-disk **registry** (`~/.x0ne/artifacts/registry.json`)
//! and the storage layout. The `x0ne-artifact-proxy` binary (in this crate)
//! reads the same registry to route requests. `x0ne-runtime` depends on this
//! crate to manipulate artifacts during action execution.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

// ── Constants ────────────────────────────────────────────────────────────────

/// Loopback port on which `x0ne-artifact-proxy` listens by default.
pub const DEFAULT_PROXY_PORT: u16 = 7800;

/// Range of internal ports allocated to artifact services.
pub const SERVICE_PORT_MIN: u16 = 7810;
pub const SERVICE_PORT_MAX: u16 = 7899;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// Pure static files (HTML/CSS/JS), served directly from disk by the proxy.
    Static,
    /// A user-supplied long-running service (a Node/Python/binary process)
    /// that the proxy forwards requests to.
    Service,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactState {
    /// Just created; for `Service` artifacts this means no process attached yet.
    Created,
    /// `Service` artifact with a healthy backing process.
    Running,
    /// Service was started then stopped (gracefully or via crash).
    Stopped,
    /// Last attempt to start the service failed.
    Failed,
}

/// What command runs the service for a `Service` artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceSpec {
    /// Command + args, joined by spaces — interpreted via `sh -c`.
    pub command: String,
    /// Internal TCP port the service binds (the proxy forwards `/a/<id>/` here).
    pub port: u16,
    /// PID of the supervisor child (None when not running).
    #[serde(default)]
    pub pid: Option<u32>,
    /// Extra environment variables exported to the service.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub kind: ArtifactKind,
    pub state: ArtifactState,
    /// `false` = ephemeral (auto-purged), `true` = saved (kept forever).
    pub persistent: bool,
    /// Absolute path on the host where the files live.
    pub root_path: PathBuf,
    /// X0NE session that created the artifact (None for restored saved ones).
    #[serde(default)]
    pub session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Service definition (only for kind=Service).
    #[serde(default)]
    pub service: Option<ServiceSpec>,
}

/// Hostname used in **user-facing** URLs (`http://HOST:port/a/…`).
/// Default `localhost` so links work in the operator's browser on the same
/// machine as the proxy. In split setups (proxy on host, agent in Incus) set
/// `X0NE_ARTIFACT_PUBLIC_HOST` to the host's LAN IP or DNS name the browser can reach.
pub fn public_url_host() -> String {
    match std::env::var("X0NE_ARTIFACT_PUBLIC_HOST") {
        Ok(s) => {
            let t = s.trim().trim_matches('/').to_string();
            if t.is_empty() {
                "localhost".into()
            } else {
                t
            }
        }
        Err(_) => "localhost".into(),
    }
}

/// Address used to **probe** whether the proxy is up (`GET /_health`).
/// Default `127.0.0.1` — the loopback of the process running x0ne-runtime.
/// When the executor runs *inside* a container but the proxy listens on the
/// **host**, set this to the host-side bridge IP (e.g. the Incus/LXC default
/// gateway from `ip route`) so `ArtifactList` reports `proxy_running` correctly.
pub fn proxy_check_host() -> String {
    match std::env::var("X0NE_ARTIFACT_PROXY_CHECK_HOST") {
        Ok(s) => {
            let t = s.trim().to_string();
            if t.is_empty() {
                "127.0.0.1".into()
            } else {
                t
            }
        }
        Err(_) => "127.0.0.1".into(),
    }
}

/// True when the runtime is executing inside an Incus / Podman sandbox.
///
/// `x0ne-code`'s `autowrap.rs` exports `X0NE_ALLOW_LOOPBACK=1` when it spawns
/// the runtime via `incus exec` / `podman exec` (also set by the Makefile's
/// `run-incus*` / `run-podman*` targets). This is the canonical "I'm boxed in"
/// signal across the codebase — we reuse it here rather than introduce a new
/// env var.
pub fn running_in_sandbox() -> bool {
    matches!(
        std::env::var("X0NE_ALLOW_LOOPBACK").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes")
    )
}

/// TCP connect + minimal HTTP GET to `/_health`. Returns true on `HTTP/1.x 200`.
pub fn proxy_health_probe(port: u16) -> bool {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;
    let host = proxy_check_host();
    let addr = format!("{}:{}", host, port);
    let Ok(mut addrs) = addr.to_socket_addrs() else {
        return false;
    };
    let Some(socket) = addrs.next() else {
        return false;
    };
    let Ok(mut s) = TcpStream::connect_timeout(&socket, Duration::from_millis(200)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(300)));
    let _ = s.write_all(b"GET /_health HTTP/1.0\r\n\r\n");
    let mut buf = [0u8; 48];
    match s.read(&mut buf) {
        Ok(n) if n > 0 => buf[..n]
            .windows(12)
            .any(|w| w == b"HTTP/1.0 200" || w == b"HTTP/1.1 200"),
        _ => false,
    }
}

impl Artifact {
    /// Browser-facing URL through the artifact proxy.
    pub fn url(&self, proxy_port: u16) -> String {
        let h = public_url_host();
        format!("http://{}:{}/a/{}/", h, proxy_port, self.id)
    }

    /// On-disk size in bytes (sum of all files under root, walks the tree).
    pub fn size_bytes(&self) -> u64 {
        fn walk(p: &Path, acc: &mut u64) {
            let Ok(rd) = fs::read_dir(p) else { return };
            for entry in rd.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_file() {
                    *acc += meta.len();
                } else if meta.is_dir() {
                    walk(&entry.path(), acc);
                }
            }
        }
        let mut acc = 0;
        walk(&self.root_path, &mut acc);
        acc
    }
}

// ── Registry ─────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ArtifactRegistry {
    #[serde(default)]
    pub artifacts: HashMap<String, Artifact>,
    /// Port the running proxy listens on (so the CLI/agent can compute URLs).
    #[serde(default)]
    pub proxy_port: Option<u16>,
}

impl ArtifactRegistry {
    pub fn list_sorted(&self) -> Vec<&Artifact> {
        let mut v: Vec<&Artifact> = self.artifacts.values().collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
        v
    }
}

// ── Storage layout ───────────────────────────────────────────────────────────

/// Root directory: `~/.x0ne/artifacts/` (or `$X0NE_HOME/artifacts/`).
pub fn root_dir() -> PathBuf {
    if let Ok(s) = std::env::var("X0NE_HOME") {
        return PathBuf::from(s).join("artifacts");
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".x0ne").join("artifacts")
}

pub fn registry_path() -> PathBuf {
    root_dir().join("registry.json")
}

pub fn ephemeral_dir(session_id: &str) -> PathBuf {
    root_dir().join("ephemeral").join(session_id)
}

pub fn saved_dir() -> PathBuf {
    root_dir().join("saved")
}

/// Path on host where a artifact's files live (depending on persistence).
pub fn artifact_path(id: &str, persistent: bool, session_id: Option<&str>) -> PathBuf {
    if persistent {
        saved_dir().join(id)
    } else {
        ephemeral_dir(session_id.unwrap_or("orphan")).join(id)
    }
}

// ── Locking + load/save ──────────────────────────────────────────────────────
//
// We use a tiny advisory lock file (`registry.lock`) to serialise concurrent
// updates from the runtime (writing) and the proxy daemon (reading).

fn lock_path() -> PathBuf {
    root_dir().join("registry.lock")
}

/// Tiny RAII wrapper around a flock-style busy-wait lock file.
pub struct RegistryLock {
    _file: fs::File,
    path: PathBuf,
}

impl RegistryLock {
    pub fn acquire() -> std::io::Result<Self> {
        let dir = root_dir();
        fs::create_dir_all(&dir)?;
        let path = lock_path();
        // Best-effort spin: most contention is sub-millisecond.
        for _ in 0..200 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Self { _file: f, path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        }
        // Stale lock — break it.
        let _ = fs::remove_file(&path);
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let _ = writeln!(f, "{}", std::process::id());
        Ok(Self { _file: f, path })
    }
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn load_registry() -> std::io::Result<ArtifactRegistry> {
    let path = registry_path();
    if !path.exists() {
        return Ok(ArtifactRegistry::default());
    }
    let mut s = String::new();
    fs::File::open(&path)?.read_to_string(&mut s)?;
    if s.trim().is_empty() {
        return Ok(ArtifactRegistry::default());
    }
    serde_json::from_str::<ArtifactRegistry>(&s).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("registry parse: {}", e),
        )
    })
}

pub fn save_registry(reg: &ArtifactRegistry) -> std::io::Result<()> {
    fs::create_dir_all(root_dir())?;
    let path = registry_path();
    let tmp = path.with_extension("json.tmp");
    let s = serde_json::to_string_pretty(reg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    fs::write(&tmp, s)?;
    fs::rename(tmp, path)?;
    Ok(())
}

/// `with_registry(|reg| { … })` — atomically read, mutate, write back.
pub fn with_registry<R>(f: impl FnOnce(&mut ArtifactRegistry) -> R) -> std::io::Result<R> {
    let _lock = RegistryLock::acquire()?;
    let mut reg = load_registry()?;
    let out = f(&mut reg);
    save_registry(&reg)?;
    Ok(out)
}

// ── ID + port allocation ─────────────────────────────────────────────────────

/// A fresh id: `cv_` and 128 random bits (a v4 UUID), nothing of the clock
/// or the pid in it. The id is the artifact's URL; it was a hash of the
/// clock's nanoseconds and the pid, which anyone on the machine enumerates
/// (audit of 2026-10-30, H3-1 follow-up).
pub fn new_artifact_id() -> String {
    format!("cv_{}", uuid::Uuid::new_v4().simple())
}

/// Find a free TCP port in `[SERVICE_PORT_MIN, SERVICE_PORT_MAX]`, skipping
/// any port already claimed by another artifact in the registry.
pub fn allocate_service_port(reg: &ArtifactRegistry) -> Option<u16> {
    let claimed: std::collections::HashSet<u16> = reg
        .artifacts
        .values()
        .filter_map(|c| c.service.as_ref().map(|s| s.port))
        .collect();
    for port in SERVICE_PORT_MIN..=SERVICE_PORT_MAX {
        if claimed.contains(&port) {
            continue;
        }
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Some(port);
        }
    }
    None
}

// ── Process supervision helpers ──────────────────────────────────────────────

/// Returns true if a PID is alive (POSIX kill -0 semantics).
pub fn pid_alive(pid: u32) -> bool {
    // Linux: /proc/<pid> existence is the cheapest correct test.
    Path::new(&format!("/proc/{}", pid)).exists()
}

/// SIGTERM the whole process group then SIGKILL it if still alive after
/// `grace_ms`. Artifact services are spawned with `setsid` so the PID is the
/// process-group leader; killing the group reaps any forked children
/// (e.g. `sh -c "python3 …"` → python).
///
/// kill(2), not a `kill` program: none was on PATH in the rust:1.98.1 image
/// (2026-10-16), its error was dropped, and the service outlived the stop.
pub fn terminate_pid(pid: u32, grace_ms: u64) {
    let Some(group) = process_group(pid) else {
        return;
    };
    if !pid_alive(pid) {
        return;
    }
    signal_group(group, false);
    let step = 50u64;
    let mut waited = 0u64;
    while waited < grace_ms && pid_alive(pid) {
        std::thread::sleep(std::time::Duration::from_millis(step));
        waited += step;
    }
    if pid_alive(pid) {
        signal_group(group, true);
    }
}

/// The negative pid kill(2) takes for the group led by `pid`. `None` for 0
/// and 1 (kill(0) is our own group, kill(-1) every process we may signal),
/// for our own pid, and for a pid that does not fit a `pid_t`.
fn process_group(pid: u32) -> Option<i32> {
    if pid <= 1 || pid == std::process::id() {
        return None;
    }
    i32::try_from(pid).ok().map(|p| -p)
}

#[cfg(unix)]
fn signal_group(group: i32, force: bool) {
    let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
    // SAFETY: kill(2) takes no pointer; a negative pid names a process group.
    unsafe {
        libc::kill(group, sig);
    }
}

#[cfg(not(unix))]
fn signal_group(_group: i32, _force: bool) {}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_distinct_in_quick_succession() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            seen.insert(new_artifact_id());
        }
        assert_eq!(seen.len(), 50, "ids repeated: {seen:?}");
    }

    /// Audit of 2026-10-30 (H3-1 follow-up). The id was a hash of the
    /// clock's nanoseconds and the pid: anyone on the machine who reads the
    /// pid and the clock enumerates it, as this test does from the clock
    /// before and after the call. An id is a URL: it must not be guessable.
    #[test]
    fn an_id_is_not_predicted_from_the_clock_and_the_pid() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        };
        let pid = std::process::id();
        let before = nanos();
        let id = new_artifact_id();
        let after = nanos();
        let hi = if after >= before {
            after
        } else {
            after + 1_000_000_000
        };
        let predicted = (before..=hi).any(|n| {
            let n = n % 1_000_000_000;
            id == format!(
                "cv_{:08x}",
                n.wrapping_mul(2_654_435_761) ^ pid.wrapping_mul(40_503)
            )
        });
        assert!(!predicted, "{id} follows from the clock and the pid");
        let hex = id.strip_prefix("cv_").unwrap_or("");
        assert!(
            hex.len() >= 32 && hex.chars().all(|c| c.is_ascii_hexdigit()),
            "{id}: fewer than 128 random bits"
        );
    }

    #[test]
    fn registry_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("X0NE_HOME", tmp.path());
        let cv = Artifact {
            id: "cv_test".into(),
            name: Some("demo".into()),
            kind: ArtifactKind::Static,
            state: ArtifactState::Created,
            persistent: false,
            root_path: tmp.path().join("x"),
            session_id: Some("s1".into()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            service: None,
        };
        with_registry(|r| {
            r.artifacts.insert(cv.id.clone(), cv.clone());
            r.proxy_port = Some(7800);
        })
        .unwrap();
        let back = load_registry().unwrap();
        assert_eq!(back.proxy_port, Some(7800));
        assert!(back.artifacts.contains_key("cv_test"));
        std::env::remove_var("X0NE_HOME");
    }

    #[test]
    fn url_uses_proxy_port() {
        let cv = Artifact {
            id: "cv_abcd".into(),
            name: None,
            kind: ArtifactKind::Static,
            state: ArtifactState::Created,
            persistent: true,
            root_path: PathBuf::from("/tmp"),
            session_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            service: None,
        };
        assert_eq!(cv.url(8000), "http://localhost:8000/a/cv_abcd/");
    }

    /// A registry pid of 0 or 1, or our own, never reaches kill(2): with
    /// kill(-1), one edited registry.json would end every process we may signal.
    #[test]
    fn a_pid_that_names_no_single_group_is_never_signalled() {
        for pid in [0, 1, std::process::id(), u32::MAX, 1u32 << 31] {
            assert_eq!(process_group(pid), None, "pid {pid}");
        }
        assert_eq!(process_group(4242), Some(-4242));
    }

    /// SIGTERM reaches the service's group. The rust:1.98.1 image has no
    /// `kill` on PATH: the stop signalled nothing there and the service
    /// outlived ArtifactStop (2026-10-16).
    #[cfg(unix)]
    #[test]
    fn terminate_pid_delivers_sigterm_to_the_group() {
        use std::os::unix::process::CommandExt;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let mark = dir.path().join("term");
        let ready = dir.path().join("ready");
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(r#"trap 'echo TERM > "$1"; exit 0' TERM; : > "$2"; while :; do sleep 0.05; done"#)
            .arg("sh")
            .arg(&mark)
            .arg(&ready)
            .process_group(0)
            .spawn()
            .expect("spawn sh");
        let t0 = Instant::now();
        while !ready.exists() && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(ready.exists(), "the TERM trap was never installed");

        terminate_pid(child.id(), 1000);

        let t0 = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait().expect("try_wait") {
                break Some(s);
            }
            if t0.elapsed() > Duration::from_secs(5) {
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if status.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(status.is_some(), "the group outlived terminate_pid");
        assert_eq!(
            fs::read_to_string(&mark).unwrap_or_default().trim(),
            "TERM",
            "the group was not sent SIGTERM"
        );
    }
}
