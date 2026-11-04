//! x0ne-vmd — the pool daemon.
//!
//! Long-running process that maintains a warm pool of cloned VMs and serves
//! acquire/release requests over a Unix socket. Designed for agents that
//! need bodies fast — request a clone, get a name back in ~50ms (assuming
//! the pool is pre-warmed and idle).
//!
//! Wire protocol (line-delimited JSON over UNIX socket):
//!   → {"cmd":"status"}
//!   ← {"target":3,"free":2,"acquired":1,"slots":[...]}
//!
//!   → {"cmd":"acquire","by":"agent-A"}
//!   ← {"ok":true,"vm":"pool-1","vnc":"127.0.0.1:5901"}
//!
//!   → {"cmd":"release","vm":"pool-1"}
//!   ← {"ok":true,"vm":"pool-1","state":"recycling"}
//!
//!   → {"cmd":"shutdown"}
//!   ← {"ok":true}     # daemon exits, destroys all pool members
//!
//! Defaults:
//!   socket    ~/.x0ne/vm/vmd.sock
//!   config    ~/.x0ne/vm/vmd-config.json (created on first run)
//!   state     ~/.x0ne/vm/vmd-state.json  (persisted between restarts)

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use x0ne_vm::pool::{PoolConfig, PoolState, SlotState};
use x0ne_vm::session::VmSession;
use x0ne_vm::LibvirtSession;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn state_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".x0ne")
        .join("vm")
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    init_tracing();

    let dir = state_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let sock_path = dir.join("vmd.sock");
    let cfg_path = dir.join("vmd-config.json");
    let state_path = dir.join("vmd-state.json");

    // ─── load or create config ─────────────────────────────────────────────
    let config: PoolConfig = match tokio::fs::read_to_string(&cfg_path).await {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {:?}", cfg_path))?,
        Err(_) => {
            let c = PoolConfig::default();
            tokio::fs::write(&cfg_path, serde_json::to_string_pretty(&c)?).await?;
            tracing::info!(
                "created default config at {:?} — edit + restart to change",
                cfg_path
            );
            c
        }
    };

    // ─── load or initialize state ──────────────────────────────────────────
    let mut state: PoolState = match tokio::fs::read_to_string(&state_path).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| PoolState::new(config.clone())),
        Err(_) => PoolState::new(config.clone()),
    };
    state.config = config.clone(); // always honor on-disk config

    let session: Arc<dyn VmSession> = Arc::new(LibvirtSession::new());
    let state = Arc::new(Mutex::new(state));

    // ─── ensure the golden base is SHUT OFF so its qcow2 lock is released ─
    // Without this, qemu refuses to start clones with "Failed to get shared
    // write lock" on the backing file.
    let base_name = { state.lock().await.config.base.clone() };
    if let Ok(info) = session.info(&base_name).await {
        if matches!(info.state, x0ne_vm::session::VmState::Running) {
            tracing::info!(
                "base '{}' is running; shutting it down so clones can use its disk",
                base_name
            );
            // Force off — saves time vs ACPI shutdown, and the base is immutable so no data loss.
            let _ = session.force_off(&base_name).await;
        }
    }

    // ─── ensure pool is full at startup ────────────────────────────────────
    refill_pool(&session, &state, &state_path).await;

    // ─── remove stale socket + listen ──────────────────────────────────────
    let _ = std::fs::remove_file(&sock_path);
    let listener =
        UnixListener::bind(&sock_path).with_context(|| format!("bind {:?}", sock_path))?;
    tracing::info!(
        "x0ne-vmd listening on {:?} — pool: {:?}",
        sock_path,
        state.lock().await.config
    );

    // ─── accept loop ───────────────────────────────────────────────────────
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("accept: {e}");
                continue;
            }
        };
        let session = Arc::clone(&session);
        let state = Arc::clone(&state);
        let state_path = state_path.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, session, state, state_path).await {
                tracing::warn!("client: {e}");
            }
        });
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("x0ne_vmd=info,info")),
        )
        .with_writer(std::io::stderr)
        .init();
}

async fn handle_client(
    stream: UnixStream,
    session: Arc<dyn VmSession>,
    state: Arc<Mutex<PoolState>>,
    state_path: PathBuf,
) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let resp = process_cmd(&line, &session, &state, &state_path).await;
        let mut bytes = serde_json::to_vec(&resp)?;
        bytes.push(b'\n');
        if wr.write_all(&bytes).await.is_err() {
            break;
        }
    }
    Ok(())
}

async fn process_cmd(
    raw: &str,
    session: &Arc<dyn VmSession>,
    state: &Arc<Mutex<PoolState>>,
    state_path: &PathBuf,
) -> Value {
    let Ok(req): std::result::Result<Value, _> = serde_json::from_str(raw) else {
        return json!({"ok": false, "error": "invalid json"});
    };
    let cmd = req.get("cmd").and_then(|v| v.as_str()).unwrap_or("");

    match cmd {
        "status" => {
            let s = state.lock().await;
            json!({
                "ok": true,
                "config": s.config,
                "free": s.free(),
                "acquired": s.acquired(),
                "slots": s.slots.values().collect::<Vec<_>>(),
            })
        }
        "acquire" => {
            let by = req
                .get("by")
                .and_then(|v| v.as_str())
                .unwrap_or("anon")
                .to_string();
            // 1. Try to find a Free slot
            let picked = {
                let mut s = state.lock().await;
                s.acquire(&by, now_ms()).map(|slot| slot.name.clone())
            };
            let Some(vm_name) = picked else {
                // Trigger a refill in background, return busy now
                let session = Arc::clone(session);
                let state_arc = Arc::clone(state);
                let sp = state_path.clone();
                tokio::spawn(async move { refill_pool(&session, &state_arc, &sp).await });
                return json!({"ok": false, "error": "no free slot — pool is empty or all acquired"});
            };
            // 2. Ensure the slot's VM is started
            if let Err(e) = session.start(&vm_name).await {
                tracing::warn!("start {vm_name} failed: {e}");
            }
            // 3. Trigger pool refill if below target
            let session_arc = Arc::clone(session);
            let state_arc = Arc::clone(state);
            let sp = state_path.clone();
            tokio::spawn(async move { refill_pool(&session_arc, &state_arc, &sp).await });
            // 4. Probe info to return VNC endpoint
            let info = session.info(&vm_name).await.ok();
            let vnc = info.as_ref().and_then(|i| i.vnc_endpoint.clone());
            persist_state(state, state_path).await;
            json!({"ok": true, "vm": vm_name, "vnc": vnc, "acquired_by": by})
        }
        "release" => {
            let Some(vm) = req
                .get("vm")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
            else {
                return json!({"ok": false, "error": "missing 'vm' field"});
            };
            // mark Recycling
            {
                let mut s = state.lock().await;
                if let Err(e) = s.start_recycle(&vm) {
                    return json!({"ok": false, "error": e.to_string()});
                }
            }
            // Detach the actual revert + restart so the caller gets a fast ack.
            let session_arc = Arc::clone(session);
            let state_arc = Arc::clone(state);
            let sp = state_path.clone();
            let vm_for_task = vm.clone();
            tokio::spawn(async move {
                // Revert via base golden snapshot if it exists. For v1 we just
                // destroy + recreate the clone — simpler and avoids snapshot
                // chain ownership questions across linked clones.
                let _ = session_arc.destroy_clone(&vm_for_task).await;
                state_arc.lock().await.reap(&vm_for_task);
                persist_state(&state_arc, &sp).await;
                refill_pool(&session_arc, &state_arc, &sp).await;
            });
            json!({"ok": true, "vm": vm, "state": "recycling"})
        }
        "shutdown" => {
            let names: Vec<String> = {
                let s = state.lock().await;
                s.slots.keys().cloned().collect()
            };
            for n in names {
                let _ = session.destroy_clone(&n).await;
            }
            // Exit after this response is flushed.
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                std::process::exit(0);
            });
            json!({"ok": true, "shutdown": true})
        }
        _ => json!({"ok": false, "error": format!("unknown cmd: {cmd}")}),
    }
}

/// Spawn fresh clones to top the pool up to `config.target` Free slots.
/// Idempotent — safe to call repeatedly.
async fn refill_pool(
    session: &Arc<dyn VmSession>,
    state: &Arc<Mutex<PoolState>>,
    state_path: &PathBuf,
) {
    // Bail out after N consecutive failures to avoid infinite spinning when
    // the system is fundamentally broken (e.g. libvirt down, no disk space).
    let mut consecutive_fail = 0;
    const MAX_FAIL: u32 = 3;
    while consecutive_fail < MAX_FAIL {
        let (base, name_to_create) = {
            let s = state.lock().await;
            if s.deficit() <= 0 {
                break;
            }
            let Some(n) = s.next_name() else {
                break;
            };
            (s.config.base.clone(), n)
        };

        // If a stale overlay disk from a previous run is squatting on this
        // name, remove it so clone_vm can proceed. Safe because the name
        // would only come from `next_name()` which already checks libvirt
        // — if libvirt doesn't know about this name, the disk is orphan.
        let overlay = dirs::home_dir()
            .map(|h| h.join(format!(".x0ne/vm/disks/{name_to_create}.qcow2")))
            .filter(|p| p.exists());
        if let Some(o) = overlay {
            tracing::warn!("pool: removing orphan overlay {:?}", o);
            let _ = std::fs::remove_file(&o);
        }

        match session.clone_vm(&base, &name_to_create).await {
            Ok(_) => {
                tracing::info!("pool: cloned {name_to_create} from {base}");
                // Boot it right away so it has a live framebuffer (otherwise
                // virsh screenshot returns nothing and the dashboard tile is
                // blank). Alpine + 256MB stays near 0% CPU at idle login.
                if let Err(e) = session.start(&name_to_create).await {
                    tracing::warn!("pool: failed to start {name_to_create}: {e}");
                }
                let mut s = state.lock().await;
                s.add_free(&name_to_create, now_ms());
                drop(s);
                persist_state(state, state_path).await;
                consecutive_fail = 0;
            }
            Err(e) => {
                tracing::warn!("pool refill failed for {name_to_create}: {e} — continuing");
                consecutive_fail += 1;
            }
        }
    }
    if consecutive_fail >= MAX_FAIL {
        tracing::error!("pool refill: {MAX_FAIL} consecutive failures — giving up");
    }
}

async fn persist_state(state: &Arc<Mutex<PoolState>>, path: &PathBuf) {
    let s = state.lock().await;
    if let Ok(json) = serde_json::to_string_pretty(&*s) {
        let _ = tokio::fs::write(path, json).await;
    }
}

// keep clippy quiet about unused variants
#[allow(dead_code)]
const _: SlotState = SlotState::Free;

#[derive(Serialize)]
#[allow(dead_code)]
struct _Marker {
    _placeholder: bool,
}
