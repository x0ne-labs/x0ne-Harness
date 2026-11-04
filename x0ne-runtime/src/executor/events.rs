use std::net::TcpStream;
use std::time::Duration;

use chrono::Utc;
use serde_json::json;

use crate::action::EventType;
use crate::config::AppConfig;
use crate::handles::{gen_id, HandleStatus, HandleType, SharedHandles};
use crate::result::{ActionOutput, ActionResult, ErrorKind};

pub async fn subscribe(
    event_type: &EventType,
    config: &AppConfig,
    handles: SharedHandles,
) -> ActionResult {
    match event_type {
        EventType::FileChanged { path, recursive } => {
            subscribe_file_changed(path, *recursive, config, handles).await
        }
        EventType::FileCreated { dir, pattern } => {
            subscribe_file_created(dir, pattern.clone(), config, handles).await
        }
        EventType::ProcessExited { pid } => subscribe_process_exited(*pid, handles).await,
        EventType::PortOpen { host, port } => {
            subscribe_port_open(host, *port, config, handles).await
        }
    }
}

// ── FileChanged ───────────────────────────────────────────────────────────────

async fn subscribe_file_changed(
    path: &str,
    recursive: bool,
    config: &AppConfig,
    handles: SharedHandles,
) -> ActionResult {
    use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};

    let handle_id = gen_id("watch");
    {
        let mut mgr = handles.lock().await;
        if let Err(e) = mgr.register(handle_id.clone(), HandleType::FileWatch) {
            return ActionResult::err("Subscribe", ErrorKind::Internal, e, 0);
        }
    }

    let debounce = Duration::from_millis(config.events.debounce_ms);
    let path_str = path.to_string();
    let handles_clone = handles.clone();
    let hid = handle_id.clone();

    let task = tokio::spawn(async move {
        use std::sync::mpsc;
        let (tx, rx) = mpsc::channel();
        let mut watcher = match RecommendedWatcher::new(tx, NotifyConfig::default()) {
            Ok(w) => w,
            Err(e) => {
                let mut mgr = handles_clone.lock().await;
                mgr.set_status(
                    &hid,
                    HandleStatus::Failed {
                        at: Utc::now().to_rfc3339(),
                        error: e.to_string(),
                    },
                );
                return;
            }
        };
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        if let Err(e) = watcher.watch(std::path::Path::new(&path_str), mode) {
            let mut mgr = handles_clone.lock().await;
            mgr.set_status(
                &hid,
                HandleStatus::Failed {
                    at: Utc::now().to_rfc3339(),
                    error: e.to_string(),
                },
            );
            return;
        }

        let mut last_event = std::time::Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(event)) => {
                    if last_event.elapsed() < debounce {
                        continue;
                    }
                    last_event = std::time::Instant::now();
                    let kind = match event.kind {
                        notify::EventKind::Create(_) => "created",
                        notify::EventKind::Remove(_) => "deleted",
                        _ => "modified",
                    };
                    for p in event.paths {
                        let ev = json!({
                            "event": kind,
                            "path": p.to_string_lossy(),
                            "timestamp": Utc::now().to_rfc3339(),
                        });
                        handles_clone.lock().await.push_event(&hid, ev);
                    }
                }
                Ok(Err(_)) | Err(_) => {
                    // Check if cancelled
                    let mgr = handles_clone.lock().await;
                    if mgr
                        .get_status(&hid)
                        .map(|s| s.is_terminal())
                        .unwrap_or(true)
                    {
                        break;
                    }
                }
            }
        }
    });

    handles.lock().await.set_task(&handle_id, task);

    ActionResult::ok(
        "Subscribe",
        ActionOutput::SubscribeCreated {
            handle: handle_id,
            event_type: "file_changed".into(),
            subscribed: true,
        },
        0,
    )
}

// ── FileCreated ────────────────────────────────────────────────────────────────

async fn subscribe_file_created(
    dir: &str,
    pattern: Option<String>,
    config: &AppConfig,
    handles: SharedHandles,
) -> ActionResult {
    use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};

    let handle_id = gen_id("watch");
    {
        let mut mgr = handles.lock().await;
        if let Err(e) = mgr.register(handle_id.clone(), HandleType::FileWatch) {
            return ActionResult::err("Subscribe", ErrorKind::Internal, e, 0);
        }
    }

    let dir_str = dir.to_string();
    let debounce = Duration::from_millis(config.events.debounce_ms);
    let handles_clone = handles.clone();
    let hid = handle_id.clone();

    let task = tokio::spawn(async move {
        use std::sync::mpsc;
        let (tx, rx) = mpsc::channel();
        let mut watcher = match RecommendedWatcher::new(tx, NotifyConfig::default()) {
            Ok(w) => w,
            Err(e) => {
                let mut mgr = handles_clone.lock().await;
                mgr.set_status(
                    &hid,
                    HandleStatus::Failed {
                        at: Utc::now().to_rfc3339(),
                        error: e.to_string(),
                    },
                );
                return;
            }
        };
        if let Err(e) = watcher.watch(std::path::Path::new(&dir_str), RecursiveMode::NonRecursive) {
            let mut mgr = handles_clone.lock().await;
            mgr.set_status(
                &hid,
                HandleStatus::Failed {
                    at: Utc::now().to_rfc3339(),
                    error: e.to_string(),
                },
            );
            return;
        }
        let mut last_event = std::time::Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(event)) if matches!(event.kind, notify::EventKind::Create(_)) => {
                    if last_event.elapsed() < debounce {
                        continue;
                    }
                    last_event = std::time::Instant::now();
                    for p in event.paths {
                        let name = p
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        // Pattern filter
                        if let Some(ref pat) = pattern {
                            if !glob::Pattern::new(pat)
                                .map(|g| g.matches(&name))
                                .unwrap_or(false)
                            {
                                continue;
                            }
                        }
                        let ev = json!({ "event": "created", "path": p.to_string_lossy(), "timestamp": Utc::now().to_rfc3339() });
                        handles_clone.lock().await.push_event(&hid, ev);
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    let mgr = handles_clone.lock().await;
                    if mgr
                        .get_status(&hid)
                        .map(|s| s.is_terminal())
                        .unwrap_or(true)
                    {
                        break;
                    }
                }
            }
        }
    });

    handles.lock().await.set_task(&handle_id, task);
    ActionResult::ok(
        "Subscribe",
        ActionOutput::SubscribeCreated {
            handle: handle_id,
            event_type: "file_created".into(),
            subscribed: true,
        },
        0,
    )
}

// ── ProcessExited ─────────────────────────────────────────────────────────────

async fn subscribe_process_exited(pid: u32, handles: SharedHandles) -> ActionResult {
    let handle_id = gen_id("procwatch");
    {
        let mut mgr = handles.lock().await;
        if let Err(e) = mgr.register(handle_id.clone(), HandleType::ProcessWatch) {
            return ActionResult::err("Subscribe", ErrorKind::Internal, e, 0);
        }
    }
    let handles_clone = handles.clone();
    let hid = handle_id.clone();

    let task = tokio::spawn(async move {
        loop {
            // Check if /proc/{pid} still exists (Linux)
            let proc_exists = std::path::Path::new(&format!("/proc/{}", pid)).exists();
            if !proc_exists {
                let ev =
                    json!({ "event": "exited", "pid": pid, "timestamp": Utc::now().to_rfc3339() });
                let mut mgr = handles_clone.lock().await;
                mgr.push_event(&hid, ev);
                mgr.set_status(
                    &hid,
                    HandleStatus::Completed {
                        at: Utc::now().to_rfc3339(),
                    },
                );
                break;
            }
            // Check if cancelled
            {
                let mgr = handles_clone.lock().await;
                if mgr
                    .get_status(&hid)
                    .map(|s| s.is_terminal())
                    .unwrap_or(true)
                {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    handles.lock().await.set_task(&handle_id, task);
    ActionResult::ok(
        "Subscribe",
        ActionOutput::SubscribeCreated {
            handle: handle_id,
            event_type: "process_exited".into(),
            subscribed: true,
        },
        0,
    )
}

// ── PortOpen ──────────────────────────────────────────────────────────────────

async fn subscribe_port_open(
    host: &str,
    port: u16,
    config: &AppConfig,
    handles: SharedHandles,
) -> ActionResult {
    // Classified before the watcher is registered, with the same rules as any
    // other outbound target. A port watch is a port check on a timer, so
    // leaving it unguarded reinstated the scanner this closes for `PortCheck`.
    let pinned = match crate::executor::system::check_host(host, port, false) {
        Ok(a) => a,
        Err(reason) => {
            return ActionResult::err("Subscribe", ErrorKind::PolicyDenied, reason, 0);
        }
    };
    let handle_id = gen_id("portwatch");
    {
        let mut mgr = handles.lock().await;
        if let Err(e) = mgr.register(handle_id.clone(), HandleType::PortWatch) {
            return ActionResult::err("Subscribe", ErrorKind::Internal, e, 0);
        }
    }
    let host_str = host.to_string();
    let interval = Duration::from_secs(config.events.port_check_interval_secs);
    let timeout = Duration::from_secs(config.events.port_check_timeout_secs);
    let handles_clone = handles.clone();
    let hid = handle_id.clone();

    let task = tokio::spawn(async move {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            // Connect to the address classified above. The previous code
            // re-parsed a string each tick and silently fell back to
            // `127.0.0.1:0` when parsing failed.
            let open = tokio::task::spawn_blocking(move || {
                TcpStream::connect_timeout(&pinned, Duration::from_secs(2)).is_ok()
            })
            .await
            .unwrap_or(false);

            if open {
                let ev = json!({ "event": "port_open", "host": host_str, "port": port, "timestamp": Utc::now().to_rfc3339() });
                let mut mgr = handles_clone.lock().await;
                mgr.push_event(&hid, ev);
                mgr.set_status(
                    &hid,
                    HandleStatus::Completed {
                        at: Utc::now().to_rfc3339(),
                    },
                );
                break;
            }

            if std::time::Instant::now() >= deadline {
                let mut mgr = handles_clone.lock().await;
                mgr.set_status(
                    &hid,
                    HandleStatus::Failed {
                        at: Utc::now().to_rfc3339(),
                        error: format!("Timed out waiting for {}:{}", host_str, port),
                    },
                );
                break;
            }

            {
                let mgr = handles_clone.lock().await;
                if mgr
                    .get_status(&hid)
                    .map(|s| s.is_terminal())
                    .unwrap_or(true)
                {
                    break;
                }
            }
            tokio::time::sleep(interval).await;
        }
    });

    handles.lock().await.set_task(&handle_id, task);
    ActionResult::ok(
        "Subscribe",
        ActionOutput::SubscribeCreated {
            handle: handle_id,
            event_type: "port_open".into(),
            subscribed: true,
        },
        0,
    )
}

// ── Handle action handlers ────────────────────────────────────────────────────

pub async fn check_handle(handle_id: &str, handles: SharedHandles) -> ActionResult {
    let mut mgr = handles.lock().await;
    if !mgr.exists(handle_id) {
        return ActionResult::err(
            "CheckHandle",
            ErrorKind::HandleNotFound,
            format!("Handle '{}' not found.", handle_id),
            0,
        );
    }
    let events = mgr.drain_events(handle_id).unwrap_or_default();
    let dropped = mgr.events_dropped(handle_id);
    let status = mgr.get_status(handle_id).unwrap();
    let status_json = serde_json::to_value(&status).unwrap_or(serde_json::Value::Null);
    ActionResult::ok(
        "CheckHandle",
        ActionOutput::HandleEvents {
            handle: handle_id.to_string(),
            status: status_json,
            events,
            events_dropped: dropped,
        },
        0,
    )
}

/// How long a process gets to leave on SIGTERM before it is made to.
pub const STOP_GRACE: std::time::Duration = std::time::Duration::from_millis(2000);

/// Stop a handle. A process handle is first ASKED to stop — SIGTERM, then up
/// to [`STOP_GRACE`] for it to exit on its own — and only then made to
/// (`cancel`, which aborts the task and drops the child, `kill_on_drop` =
/// SIGKILL). Before 2026-08-17 it went straight to the abort: a server that
/// wrote its state on SIGTERM never got the signal (playground 19,
/// `stopped.txt` never written, `final_status: cancelled`). Non-process
/// handles (watches, subscriptions) are cancelled as before.
///
/// `final_status`: `"exited"` when the process left on its own within the
/// grace, `"killed"` when it had to be made to, `"cancelled"` for a handle
/// that is not a process.
pub async fn stop_handle(handle_id: &str, handles: SharedHandles) -> ActionResult {
    let pid = {
        let mgr = handles.lock().await;
        if !mgr.exists(handle_id) {
            return ActionResult::err(
                "StopHandle",
                ErrorKind::HandleNotFound,
                format!("Handle '{}' not found.", handle_id),
                0,
            );
        }
        mgr.pid_of(handle_id)
    };
    let started = std::time::Instant::now();
    let final_status = match pid {
        Some(pid) => {
            #[cfg(unix)]
            {
                // SAFETY: kill(2) with a pid this process spawned and still
                // owns a handle for; a stale pid can only fail (ESRCH), it
                // cannot reach a process we did not start, because the child
                // is not reaped until the handle's task drops it.
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
            }
            let mut exited = false;
            while started.elapsed() < STOP_GRACE {
                if handles
                    .lock()
                    .await
                    .get_status(handle_id)
                    .map(|s| s.is_terminal())
                    .unwrap_or(true)
                {
                    exited = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            let mut mgr = handles.lock().await;
            if exited {
                // Left on its own: keep the status the task recorded.
                "exited"
            } else {
                mgr.cancel(handle_id);
                "killed"
            }
        }
        None => {
            handles.lock().await.cancel(handle_id);
            "cancelled"
        }
    };
    ActionResult::ok(
        "StopHandle",
        ActionOutput::HandleStopped {
            handle: handle_id.to_string(),
            stopped: true,
            final_status: final_status.into(),
        },
        started.elapsed().as_millis() as u64,
    )
}

pub async fn list_handles(handles: SharedHandles) -> ActionResult {
    let mgr = handles.lock().await;
    let summaries: Vec<serde_json::Value> = mgr
        .summaries()
        .into_iter()
        .filter_map(|s| serde_json::to_value(s).ok())
        .collect();
    ActionResult::ok(
        "ListHandles",
        ActionOutput::HandleList { handles: summaries },
        0,
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::handles;

    fn make_handles() -> SharedHandles {
        handles::new_shared(1000, 300, 20)
    }

    #[tokio::test]
    async fn check_handle_drains_buffer() {
        let h = make_handles();
        let id = {
            let mut mgr = h.lock().await;
            let id = mgr.register("proc_test123", HandleType::Process).unwrap();
            mgr.push_event(&id, json!({"line": "hello"}));
            mgr.push_event(&id, json!({"line": "world"}));
            id
        };
        let result = check_handle(&id, h.clone()).await;
        assert!(result.success);
        if let Some(ActionOutput::HandleEvents { events, .. }) = result.output {
            assert_eq!(events.len(), 2);
        }
        // Second check — buffer cleared
        let result2 = check_handle(&id, h.clone()).await;
        if let Some(ActionOutput::HandleEvents { events, .. }) = result2.output {
            assert_eq!(events.len(), 0);
        }
    }

    #[tokio::test]
    async fn stop_handle_cancels() {
        let h = make_handles();
        let id = {
            let mut mgr = h.lock().await;
            mgr.register("watch_stopcancels", HandleType::FileWatch)
                .unwrap()
        };
        let result = stop_handle(&id, h.clone()).await;
        assert!(result.success);
        let status = h.lock().await.get_status(&id);
        assert!(matches!(status, Some(HandleStatus::Cancelled { .. })));
    }

    /// A process handle is asked to leave first: a child that traps SIGTERM
    /// and writes a marker gets to write it, and the status says `exited`;
    /// a child that ignores SIGTERM is killed after the grace, `killed`.
    /// A failure would mean StopHandle went straight to SIGKILL again
    /// (playground 19, 2026-08-17: `stopped.txt` never written).
    #[cfg(unix)]
    #[tokio::test]
    async fn stop_handle_terms_before_it_kills() {
        use std::process::Stdio;
        let h = make_handles();
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("stopped.txt");
        let ready = dir.path().join("ready");
        // Polite child: writes the marker on TERM, then exits. It says when
        // its trap is armed — a TERM before that would just kill it.
        let mut polite = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "trap 'echo bye > {}; exit 0' TERM; : > {}; while :; do sleep 0.05; done",
                marker.display(),
                ready.display()
            ))
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let polite_pid = polite.id();
        while !ready.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let id = {
            let mut mgr = h.lock().await;
            let id = mgr.register("proc_polite", HandleType::Process).unwrap();
            mgr.set_pid(&id, polite_pid);
            id
        };
        // The task that would normally wait the child and record the status.
        let h2 = h.clone();
        let id2 = id.clone();
        let task = tokio::spawn(async move {
            let _ = polite.wait().await;
            h2.lock().await.set_status(
                &id2,
                HandleStatus::Completed {
                    at: chrono::Utc::now().to_rfc3339(),
                },
            );
        });
        h.lock().await.set_task(&id, task);
        let result = stop_handle(&id, h.clone()).await;
        assert!(result.success);
        match result.output {
            Some(ActionOutput::HandleStopped { final_status, .. }) => {
                assert_eq!(final_status, "exited", "left on SIGTERM")
            }
            other => panic!("{other:?}"),
        }
        assert!(marker.exists(), "the SIGTERM handler ran");

        // Stubborn child: ignores TERM; killed after the grace.
        let ready2 = dir.path().join("ready2");
        let mut stubborn = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "trap '' TERM; : > {}; while :; do sleep 0.05; done",
                ready2.display()
            ))
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = stubborn.id();
        while !ready2.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let id = {
            let mut mgr = h.lock().await;
            let id = mgr.register("proc_stubborn", HandleType::Process).unwrap();
            mgr.set_pid(&id, pid);
            id
        };
        let task = tokio::spawn(async move {
            let _ = stubborn.wait().await;
        });
        h.lock().await.set_task(&id, task);
        let t0 = std::time::Instant::now();
        let result = stop_handle(&id, h.clone()).await;
        assert!(
            t0.elapsed() >= STOP_GRACE,
            "waited the grace before killing"
        );
        match result.output {
            Some(ActionOutput::HandleStopped { final_status, .. }) => {
                assert_eq!(final_status, "killed")
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn list_handles_returns_all() {
        let h = make_handles();
        {
            let mut mgr = h.lock().await;
            mgr.register("p1_listtest", HandleType::Process).unwrap();
            mgr.register("p2_listtest", HandleType::FileWatch).unwrap();
        }
        let result = list_handles(h.clone()).await;
        assert!(result.success);
        if let Some(ActionOutput::HandleList { handles: hlist }) = result.output {
            assert!(hlist.len() >= 2);
        }
    }

    #[tokio::test]
    async fn check_handle_not_found_errors() {
        let h = make_handles();
        let result = check_handle("nonexistent_handle_xyz", h).await;
        assert!(!result.success);
        assert!(matches!(
            result.error.unwrap().kind,
            ErrorKind::HandleNotFound
        ));
    }

    /// BEHAVIOUR CHANGE: a port watch is a port check on a timer, so it is
    /// classified like any other outbound target. Loopback is refused unless
    /// the deployment opts in — the sandbox config sets `X0NE_ALLOW_LOOPBACK`,
    /// because there the container is the isolation boundary. This test
    /// previously bound a loopback listener and asserted detection, which is
    /// exactly the scanning capability being closed.
    #[tokio::test]
    async fn subscribe_port_open_refuses_loopback_by_default() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg = AppConfig::defaults();
        let handles = crate::handles::new_shared(100, 60, 10);
        let result = subscribe_port_open("127.0.0.1", port, &cfg, handles).await;
        assert!(
            !result.success,
            "a loopback port watch must be classified, not armed"
        );
        drop(listener);
    }
}
