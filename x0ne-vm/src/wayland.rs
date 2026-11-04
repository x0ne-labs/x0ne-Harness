//! Wayland-backend session — implements [`VmSession`] against a headless
//! wlroots compositor (sway), with capture via `grim` and input via `wlrctl`.
//!
//! ## Why
//!
//! VNC + libvirt is the right tool for cross-OS, snapshotable, deeply
//! isolated guests. But for the 80% case ("the agent needs a Linux desktop
//! to drive a Firefox / shell / editor"), spinning up a full KVM VM is
//! overkill — it costs 256MB RAM and 5-30s of boot per instance.
//!
//! With sway in headless mode + grim + wlrctl we get:
//! - ~50MB RAM per "VM" (sway + apps)
//! - ~200ms cold-spawn
//! - ~5-15ms screencopy latency
//! - native execution speed (no virt tax)
//!
//! Tradeoff: weaker isolation (shared kernel). For untrusted code, stay on
//! [`crate::LibvirtSession`].
//!
//! ## Prerequisites
//!
//! - `sway` ≥ 1.7 with wlroots headless backend (set `WLR_BACKENDS=headless`)
//! - `grim` (wlr-screencopy client)
//! - `wlrctl` (virtual-keyboard + pointer + foreign-toplevel protocols)
//!
//! All three are subprocess deps in v0; a native `smithay-client-toolkit`
//! port can land in v1 to drop the subprocess hop.
//!
//! ## State layout
//!
//! Per-session state lives in `~/.x0ne/wayland/<name>/`:
//! - `pid`        — sway's PID
//! - `socket`     — name of the Wayland socket under `$XDG_RUNTIME_DIR`
//! - `config`     — sway config file passed at spawn
//! - `sway.log`   — stdout+stderr of sway
//! - `size`       — `WxH` of the virtual output (used by grim and click coords)

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::{sleep, timeout, Duration};

use crate::session::{KeyChord, MouseButton, Screenshot, VmInfo, VmSession, VmState};

/// Default virtual-output resolution. Matches what Claude's computer tool
/// declares (1280×800 is a reasonable middle ground — large enough for real
/// apps, small enough to fit in Claude's vision token budget).
pub const DEFAULT_SIZE: (u32, u32) = (1280, 800);

/// Default name when the caller doesn't care.
pub const DEFAULT_SESSION: &str = "default";

#[derive(Debug, Clone)]
pub struct WaylandSession {
    /// Root of per-session state — defaults to `~/.x0ne/wayland/`.
    pub state_root: PathBuf,
    /// `(width, height)` of the virtual output. Defaults to [`DEFAULT_SIZE`].
    pub size: (u32, u32),
    /// If true, run sway as a nested Wayland client (window pops on the host
    /// desktop). If false, use the wlroots headless backend (no visible
    /// window). Defaults to true when a host `WAYLAND_DISPLAY` is detected.
    pub nested: bool,
    /// Optional command launched by sway at startup via `exec_always`.
    /// Use this to auto-spawn a terminal, a browser, etc. — e.g. `Some("foot")`.
    pub startup_cmd: Option<String>,
}

impl WaylandSession {
    pub fn new() -> Self {
        let nested = std::env::var("WAYLAND_DISPLAY").is_ok();
        Self {
            state_root: dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".x0ne")
                .join("wayland"),
            size: DEFAULT_SIZE,
            nested,
            startup_cmd: None,
        }
    }

    pub fn with_size(mut self, w: u32, h: u32) -> Self {
        self.size = (w, h);
        self
    }

    pub fn with_nested(mut self, nested: bool) -> Self {
        self.nested = nested;
        self
    }

    pub fn with_startup_cmd(mut self, cmd: impl Into<String>) -> Self {
        self.startup_cmd = Some(cmd.into());
        self
    }

    fn session_dir(&self, name: &str) -> PathBuf {
        self.state_root.join(name)
    }
    fn pid_file(&self, name: &str) -> PathBuf {
        self.session_dir(name).join("pid")
    }
    fn socket_file(&self, name: &str) -> PathBuf {
        self.session_dir(name).join("socket")
    }
    fn config_file(&self, name: &str) -> PathBuf {
        self.session_dir(name).join("config")
    }
    fn log_file(&self, name: &str) -> PathBuf {
        self.session_dir(name).join("sway.log")
    }
    fn size_file(&self, name: &str) -> PathBuf {
        self.session_dir(name).join("size")
    }

    /// Returns the Wayland display name (e.g. `wayland-x0ne-default`) used
    /// in `WAYLAND_DISPLAY` for child processes (grim / wlrctl).
    fn wayland_display(&self, name: &str) -> String {
        format!("wayland-x0ne-{name}")
    }

    async fn ensure_state_dir(&self, name: &str) -> Result<()> {
        tokio::fs::create_dir_all(self.session_dir(name)).await?;
        Ok(())
    }

    /// True if the recorded PID is still alive (signal 0 → ESRCH if not).
    async fn is_alive(&self, name: &str) -> bool {
        match self.read_pid(name).await {
            // SAFETY: kill(2) takes two integers and no pointer; signal 0 sends
            // nothing, it only asks whether the pid exists.
            Ok(pid) => unsafe { libc::kill(pid, 0) == 0 },
            Err(_) => false,
        }
    }

    async fn read_pid(&self, name: &str) -> Result<i32> {
        let raw = tokio::fs::read_to_string(self.pid_file(name)).await?;
        parse_pid(&raw)
    }

    /// Run a child process with WAYLAND_DISPLAY set to this session's actual
    /// sway socket (read from disk, set at start-time).
    async fn child(&self, name: &str, prog: &str, args: &[&str]) -> Result<std::process::Output> {
        let socket = tokio::fs::read_to_string(self.socket_file(name))
            .await
            .with_context(|| format!("session '{name}' has no recorded socket — is it running?"))?;
        let socket = socket.trim().to_string();
        let mut cmd = Command::new(prog);
        cmd.env("WAYLAND_DISPLAY", &socket)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = cmd
            .output()
            .await
            .with_context(|| format!("spawning `{prog}` failed"))?;
        if !out.status.success() {
            bail!(
                "{} exited {}: {}",
                prog,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out)
    }
}

impl Default for WaylandSession {
    fn default() -> Self {
        Self::new()
    }
}

// ─── helpers shared by tests ────────────────────────────────────────────────

/// Render a minimal sway config.
/// - `width`/`height` set the virtual output mode.
/// - `nested` selects which output name to configure (the wayland backend
///   exposes `WL-1`, the headless backend exposes `HEADLESS-1`).
/// - `startup_cmd` is launched once via `exec_always` (e.g. `foot`, `firefox`).
pub fn render_sway_config(
    width: u32,
    height: u32,
    nested: bool,
    startup_cmd: Option<&str>,
) -> String {
    let output_name = if nested { "WL-1" } else { "HEADLESS-1" };
    let exec_line = match startup_cmd {
        Some(cmd) => format!("exec_always {cmd}"),
        None => "exec_always sleep infinity".to_string(),
    };
    format!(
        r#"# auto-generated by x0ne-vm; safe to delete when sway is stopped
output {output_name} {{
    mode {width}x{height}@60Hz
}}
# Disable bar + decorations — pure framebuffer.
default_border none
default_floating_border none
font monospace 10
{exec_line}
"#
    )
}

/// Map an x0ne `KeyChord` to a wlrctl `keyboard event` invocation.
/// wlrctl uses Linux `KEY_*` codes (see `linux/input-event-codes.h`).
pub fn keychord_to_wlrctl(chord: &KeyChord) -> Option<(Vec<String>, String)> {
    let main = key_to_wlrctl(&chord.key)?;
    let mods: Vec<String> = chord
        .modifiers
        .iter()
        .filter_map(|m| modifier_to_wlrctl(m))
        .map(|s| s.to_string())
        .collect();
    Some((mods, main))
}

fn key_to_wlrctl(key: &str) -> Option<String> {
    let k = key.trim().to_ascii_lowercase();
    Some(match k.as_str() {
        "return" | "enter" => "RETURN".into(),
        "tab" => "TAB".into(),
        "escape" | "esc" => "ESC".into(),
        "backspace" | "bs" => "BACKSPACE".into(),
        "space" | "spc" => "SPACE".into(),
        "left" => "LEFT".into(),
        "right" => "RIGHT".into(),
        "up" => "UP".into(),
        "down" => "DOWN".into(),
        "delete" | "del" => "DELETE".into(),
        "home" => "HOME".into(),
        "end" => "END".into(),
        "pageup" | "pgup" => "PAGEUP".into(),
        "pagedown" | "pgdn" => "PAGEDOWN".into(),
        s if s.starts_with('f') && s[1..].parse::<u32>().is_ok() => {
            format!("F{}", &s[1..])
        }
        s if s.chars().count() == 1 => {
            let c = s.chars().next().unwrap().to_ascii_uppercase();
            match c {
                'A'..='Z' => c.to_string(),
                '0'..='9' => c.to_string(),
                _ => return None,
            }
        }
        _ => return None,
    })
}

fn modifier_to_wlrctl(m: &str) -> Option<&'static str> {
    Some(match m.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" => "ctrl",
        "shift" => "shift",
        "alt" => "alt",
        "super" | "meta" | "win" | "cmd" => "logo",
        _ => return None,
    })
}

#[async_trait]
impl VmSession for WaylandSession {
    /// List every directory under state_root that has a live PID.
    async fn list(&self) -> Result<Vec<VmInfo>> {
        if !self.state_root.exists() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.state_root).await?;
        while let Some(e) = entries.next_entry().await? {
            if !e.metadata().await?.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let state = if self.is_alive(&name).await {
                VmState::Running
            } else {
                VmState::ShutOff
            };
            out.push(VmInfo {
                name,
                uuid: None,
                state,
                vcpu: None,
                max_mem_kib: None,
                used_mem_kib: None,
                vnc_endpoint: None,
            });
        }
        Ok(out)
    }

    async fn info(&self, name: &str) -> Result<VmInfo> {
        let alive = self.is_alive(name).await;
        Ok(VmInfo {
            name: name.to_string(),
            uuid: None,
            state: if alive {
                VmState::Running
            } else {
                VmState::ShutOff
            },
            vcpu: None,
            max_mem_kib: None,
            used_mem_kib: None,
            vnc_endpoint: None,
        })
    }

    /// Spawn sway and wait for its Wayland socket to appear in
    /// `$XDG_RUNTIME_DIR`. Sway picks the socket name itself (wayland-N), so
    /// we snapshot the sockets before spawn and watch for a new one after.
    /// That name is then recorded in `socket` so grim/wlrctl can target it.
    async fn start(&self, name: &str) -> Result<()> {
        if self.is_alive(name).await {
            return Ok(()); // idempotent
        }
        self.ensure_state_dir(name).await?;
        let cfg_path = self.config_file(name);
        let cfg = render_sway_config(
            self.size.0,
            self.size.1,
            self.nested,
            self.startup_cmd.as_deref(),
        );
        tokio::fs::write(&cfg_path, &cfg).await?;
        tokio::fs::write(
            self.size_file(name),
            format!("{}x{}", self.size.0, self.size.1),
        )
        .await?;

        // Snapshot existing wayland-* sockets so we can identify ours later.
        // SAFETY: geteuid(2) takes no argument and cannot fail.
        let xdg = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::geteuid() }));
        let before: std::collections::HashSet<String> = list_wayland_sockets(&xdg);

        let log = std::fs::File::create(self.log_file(name))?;
        let log_err = log.try_clone()?;
        let mut child = Command::new("sway");
        if self.nested {
            // Nested: sway inherits host WAYLAND_DISPLAY to connect as a
            // client. Its own server socket name is picked automatically.
            child.env("SWAYSOCK", self.session_dir(name).join("sway-ipc.sock"));
        } else {
            // Headless: no host display, virtual output.
            child
                .env("WLR_BACKENDS", "headless")
                .env("WAYLAND_DISPLAY", self.wayland_display(name))
                .env_remove("DISPLAY");
        }
        // Force the software (pixman) renderer — wlr-screencopy is more
        // reliable on it, especially under nested sway where the GL context
        // is shared with the host compositor and DMA-BUF capture can fail.
        // (Override at runtime with X0NE_VM_RENDERER=gles2 if needed.)
        let renderer = std::env::var("X0NE_VM_RENDERER").unwrap_or_else(|_| "pixman".to_string());
        child.env("WLR_RENDERER", renderer);
        // Disable Xwayland in our nested sway — the host already owns X0.
        child.env("WLR_NO_X11_USERSPACE", "1");

        child
            .arg("--unsupported-gpu") // tolerates software fallback
            .arg("--config")
            .arg(&cfg_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));

        let handle = child
            .spawn()
            .context("failed to spawn sway — is it installed?")?;
        let pid = handle.id().ok_or_else(|| anyhow!("sway has no PID"))?;
        tokio::fs::write(self.pid_file(name), pid.to_string()).await?;

        // Detach: we won't await the child — sway runs until we kill it.
        std::mem::forget(handle);

        // Wait up to 5s for sway's new socket to appear.
        let socket_name = timeout(Duration::from_secs(5), async {
            loop {
                let now = list_wayland_sockets(&xdg);
                if let Some(diff) = now.difference(&before).next() {
                    return diff.clone();
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| {
            anyhow!(
                "sway socket did not appear within 5s — check {:?}",
                self.log_file(name)
            )
        })?;
        tokio::fs::write(self.socket_file(name), &socket_name).await?;
        Ok(())
    }

    async fn shutdown(&self, name: &str) -> Result<()> {
        if let Ok(pid) = self.read_pid(name).await {
            // SAFETY: kill(2) takes two integers and no pointer.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            // Wait up to 3s for clean exit.
            for _ in 0..30 {
                if !self.is_alive(name).await {
                    return Ok(());
                }
                sleep(Duration::from_millis(100)).await;
            }
        }
        Ok(())
    }

    async fn force_off(&self, name: &str) -> Result<()> {
        if let Ok(pid) = self.read_pid(name).await {
            // SAFETY: kill(2) takes two integers and no pointer.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        Ok(())
    }

    async fn reboot(&self, name: &str) -> Result<()> {
        self.shutdown(name).await?;
        self.start(name).await
    }

    /// Capture via `grim` (writes PNG to stdout with `-`).
    async fn screenshot(&self, name: &str) -> Result<Screenshot> {
        let out = self.child(name, "grim", &["-"]).await?;
        let bytes = out.stdout;
        let (w, h) = parse_png_dims(&bytes).unwrap_or(self.size);
        Ok(Screenshot {
            png: bytes,
            width: w,
            height: h,
        })
    }

    async fn mouse_move(&self, name: &str, x: i32, y: i32) -> Result<()> {
        // wlrctl 0.2+ supports absolute via `pointer move <x> <y>` — but
        // for portability we'll use the explicit `position` subcommand.
        // (Note: not all wlrctl builds expose this — see Roadmap.)
        let _ = (name, x, y);
        bail!("vm_move on Wayland backend: absolute positioning not yet wired in v0 — track wlrctl support")
    }

    async fn mouse_click(
        &self,
        name: &str,
        _x: i32,
        _y: i32,
        button: MouseButton,
        double: bool,
    ) -> Result<()> {
        // wlrctl pointer click currently clicks at the cursor's existing
        // position. v0 ignores (x,y) — v1 will warp via tablet protocol.
        let btn = match button {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
        };
        self.child(name, "wlrctl", &["pointer", "click", btn])
            .await?;
        if double {
            self.child(name, "wlrctl", &["pointer", "click", btn])
                .await?;
        }
        Ok(())
    }

    async fn mouse_drag(
        &self,
        name: &str,
        _x1: i32,
        _y1: i32,
        _x2: i32,
        _y2: i32,
        _button: MouseButton,
    ) -> Result<()> {
        let _ = name;
        bail!("vm_drag on Wayland backend: not yet implemented in v0")
    }

    async fn type_text(&self, name: &str, text: &str) -> Result<()> {
        // wlrctl keyboard type "<string>" handles full Unicode via the
        // virtual-keyboard protocol (no US-layout limitation!).
        self.child(name, "wlrctl", &["keyboard", "type", text])
            .await?;
        Ok(())
    }

    async fn key_chord(&self, name: &str, chord: &KeyChord) -> Result<()> {
        let (mods, main) =
            keychord_to_wlrctl(chord).ok_or_else(|| anyhow!("unknown key: {:?}", chord.key))?;
        let mut args = vec!["keyboard".to_string(), "event".to_string()];
        for m in &mods {
            args.push("-m".to_string());
            args.push(m.clone());
        }
        args.push(main);
        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.child(name, "wlrctl", &args_ref).await?;
        Ok(())
    }

    // ─── snapshot ops — deferred to v2 (overlayfs / btrfs subvolume) ──────

    async fn snapshot_create(&self, _name: &str, _snap: &str) -> Result<()> {
        bail!("vm_snapshot_create on Wayland backend: deferred to v2 (overlayfs)")
    }
    async fn snapshot_restore(&self, _name: &str, _snap: &str) -> Result<()> {
        bail!("vm_snapshot_restore on Wayland backend: deferred to v2 (overlayfs)")
    }
    async fn snapshot_list(&self, _name: &str) -> Result<Vec<String>> {
        Ok(vec![]) // empty list is honest for v0
    }
    async fn snapshot_delete(&self, _name: &str, _snap: &str) -> Result<()> {
        bail!("vm_snapshot_delete on Wayland backend: deferred to v2 (overlayfs)")
    }
}

/// Return the set of `wayland-*` socket basenames currently present in
/// `xdg_runtime_dir`. Used by `start` to identify the new socket spawned
/// by sway via before/after diff.
fn list_wayland_sockets(xdg: &str) -> std::collections::HashSet<String> {
    let mut s = std::collections::HashSet::new();
    if let Ok(rd) = std::fs::read_dir(xdg) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            // Skip .lock files and only keep wayland-* (the actual sockets).
            if n.starts_with("wayland-") && !n.ends_with(".lock") {
                s.insert(n);
            }
        }
    }
    s
}

/// Parse `width` and `height` from the first IHDR chunk of a PNG buffer.
/// Returns `None` if the magic doesn't match.
pub fn parse_png_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    // IHDR starts at byte 8: 4 length + 4 type + 4 width + 4 height + ...
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((w, h))
}

/// A pid of 0 or below names a process group or every process to kill(2).
fn parse_pid(raw: &str) -> Result<i32> {
    let pid: i32 = raw.trim().parse()?;
    anyhow::ensure!(pid > 0, "pid file holds {pid}, not a process id");
    Ok(pid)
}

// ─── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_picks_correct_output_per_backend() {
        let headless = render_sway_config(1280, 800, false, None);
        assert!(headless.contains("HEADLESS-1"));
        assert!(headless.contains("mode 1280x800@60Hz"));

        let nested = render_sway_config(1280, 800, true, None);
        assert!(nested.contains("WL-1"));
        assert!(!nested.contains("HEADLESS-1"));

        let with_cmd = render_sway_config(1280, 800, true, Some("foot"));
        assert!(with_cmd.contains("exec_always foot"));
    }

    #[test]
    fn key_resolves_named_and_single_char() {
        assert_eq!(key_to_wlrctl("return"), Some("RETURN".into()));
        assert_eq!(key_to_wlrctl("ESC"), Some("ESC".into()));
        assert_eq!(key_to_wlrctl("f10"), Some("F10".into()));
        assert_eq!(key_to_wlrctl("a"), Some("A".into()));
        assert_eq!(key_to_wlrctl("Z"), Some("Z".into()));
        assert_eq!(key_to_wlrctl("3"), Some("3".into()));
        assert_eq!(key_to_wlrctl("nonsense"), None);
    }

    #[test]
    fn a_pid_file_must_hold_a_positive_pid() {
        assert_eq!(parse_pid("4242\n").unwrap(), 4242);
        for bad in ["0", "-1", "-4242", "", "x"] {
            assert!(parse_pid(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn modifiers_resolve() {
        assert_eq!(modifier_to_wlrctl("ctrl"), Some("ctrl"));
        assert_eq!(modifier_to_wlrctl("Shift"), Some("shift"));
        assert_eq!(modifier_to_wlrctl("super"), Some("logo"));
        assert_eq!(modifier_to_wlrctl("win"), Some("logo"));
        assert_eq!(modifier_to_wlrctl("hyper"), None);
    }

    #[test]
    fn chord_breaks_down_to_mods_and_main() {
        let chord = KeyChord::parse("ctrl+alt+t").unwrap();
        let (mods, main) = keychord_to_wlrctl(&chord).unwrap();
        assert_eq!(mods, vec!["ctrl", "alt"]);
        assert_eq!(main, "T");
    }

    #[test]
    fn png_dims_parsed_from_minimal_header() {
        // Real PNG magic + IHDR with width=1024, height=768
        let mut p = vec![0u8; 24];
        p[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        // Bytes 8..16 = chunk length + chunk type (ignored)
        p[16..20].copy_from_slice(&1024u32.to_be_bytes());
        p[20..24].copy_from_slice(&768u32.to_be_bytes());
        assert_eq!(parse_png_dims(&p), Some((1024, 768)));

        assert_eq!(parse_png_dims(b"not a png"), None);
        assert_eq!(parse_png_dims(&[0u8; 4]), None);
    }

    #[test]
    fn default_session_layout() {
        let s = WaylandSession::new();
        assert_eq!(s.size, DEFAULT_SIZE);
        assert!(s.state_root.ends_with(".x0ne/wayland"));
        assert_eq!(s.wayland_display("default"), "wayland-x0ne-default");
        assert_eq!(s.wayland_display("agent-42"), "wayland-x0ne-agent-42");
    }
}
