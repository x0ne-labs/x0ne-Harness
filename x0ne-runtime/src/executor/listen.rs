//! `Listen` — a long-running command whose interesting lines become turns.
//!
//! The cousin of `watch_react`, and the difference is the whole point: that
//! one watches a FILE and runs a FIXED action, so the model is never woken
//! and never comments. This one watches a STREAM and hands what it saw back,
//! which is the only shape in which "listen, and tell me what matters" can
//! exist.
//!
//! Returns a handle immediately. Batches land in the handle's event buffer,
//! where `CheckHandle` finds them — the same road every streamed process
//! already takes, so nothing new has to be plumbed through the bridge.
//!
//! What this module does NOT do, deliberately: wake the agent. Pushing a
//! batch into the conversation is the orchestrator's business — it owns the
//! turn loop and the human gate. The runtime is an executor; it applies
//! lists, it does not decide when the operator gets interrupted.
//!
//! ## Shape of the wiring, and why
//!
//! One task OWNS the child and drives the batcher; two pump tasks read the
//! pipes into a bounded channel. The first version (2026-09-10, morning) did
//! none of this and the code review said exactly what it cost: the `Child`
//! was dropped at the end of `listen()` without `kill_on_drop`, `set_pid` or
//! `set_task`, so `StopHandle` flipped the status to `Cancelled` while the
//! process and its reader **kept running for the life of the runtime**;
//! stderr was read only after stdout hit EOF, so a child that filled its
//! stderr pipe blocked forever; and the reader polled every 200 ms whether
//! or not anything was pending. `process.rs:830-960` had the right shape all
//! along — this file now follows it.

use std::sync::Arc;

use chrono::Utc;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

use crate::action::Action;
use crate::handles::{gen_id, HandleStatus, HandleType, SharedHandles};
use crate::result::{ActionOutput, ActionResult, ErrorKind};

use super::listen_batch::{BatchPolicy, Batcher, Verdict};

/// Ceilings on what the model may ask for. Visible clamps, like
/// `process.rs::clamp_timeout`: the values actually applied travel in the
/// `started` event, so a reader who asked for more knows it did not get it.
///
/// SECURITY: without these, `Listen { max_lines: 10_000_000, window_ms:
/// u64::MAX }` holds every line in memory forever and `CheckHandle` never
/// returns a batch. The review that found it (2026-09-10) costed it at
/// ~11 GiB after an hour on a 49 lines/s writer — just under the flood
/// threshold, so nothing tripped.
pub const MAX_WINDOW_MS: u64 = 10_000;
pub const MAX_BATCH_LINES: usize = 200;
/// Bytes kept per line. A line longer than this is cut and marked; the rest
/// is read and discarded, never buffered. Bounds one batch at
/// `MAX_BATCH_LINES × MAX_LINE_BYTES` = 800 KiB, whatever the child prints.
pub const MAX_LINE_BYTES: usize = 4096;
/// Lines in flight between the pumps and the batcher. Small on purpose: a
/// full channel makes the pump wait, which makes the child block on its
/// pipe, which is the correct back-pressure for a writer nobody keeps up
/// with. Memory stays bounded; the child slows down.
const PUMP_CAPACITY: usize = 256;

/// Compile the filter once, before spawning anything.
///
/// A regex the model wrote can be malformed, and finding that out after the
/// process is running would leave an orphan to reap. It can also be
/// pathological — `regex` has no catastrophic backtracking, which is exactly
/// why this crate is the right one to hand a model-authored pattern.
fn compile_filter(pattern: Option<&String>) -> Result<Option<regex::Regex>, String> {
    match pattern {
        None => Ok(None),
        Some(p) => regex::Regex::new(p)
            .map(Some)
            .map_err(|e| format!("filter is not a valid regex: {e}")),
    }
}

/// Clamp the model's numbers to the ceilings and say which ones were cut.
fn clamp_policy(window_ms: Option<u64>, max_lines: Option<usize>) -> (BatchPolicy, Vec<String>) {
    let mut clamped = Vec::new();
    let asked_window = window_ms.unwrap_or(400);
    let asked_lines = max_lines.unwrap_or(20);
    if asked_window > MAX_WINDOW_MS {
        clamped.push(format!("window_ms {asked_window} → {MAX_WINDOW_MS}"));
    }
    if asked_lines > MAX_BATCH_LINES {
        clamped.push(format!("max_lines {asked_lines} → {MAX_BATCH_LINES}"));
    }
    (
        BatchPolicy {
            window_ms: asked_window.min(MAX_WINDOW_MS),
            // A window of 0 would emit every line as its own batch; 1 ms is
            // the smallest that still means "group".
            max_lines: asked_lines.clamp(1, MAX_BATCH_LINES),
            ..BatchPolicy::default()
        },
        clamped,
    )
}

/// Read one line, keeping at most `cap` bytes of it.
///
/// `tokio`'s `lines()` has no per-line ceiling, and a child that prints a
/// gigabyte with no newline would be buffered whole. This reads through
/// `fill_buf`/`consume`, so the bytes past the cap are consumed and dropped
/// without ever being copied. Returns `(line, was_cut)`, or `None` at EOF.
async fn read_capped_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    cap: usize,
) -> std::io::Result<Option<(String, bool)>> {
    let mut kept: Vec<u8> = Vec::new();
    let mut cut = false;
    let mut saw_any = false;
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            // EOF. A partial last line still counts.
            return Ok(saw_any.then(|| (String::from_utf8_lossy(&kept).into_owned(), cut)));
        }
        saw_any = true;
        let (chunk, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (&buf[..i], true),
            None => (buf, false),
        };
        let room = cap.saturating_sub(kept.len());
        if chunk.len() > room {
            kept.extend_from_slice(&chunk[..room]);
            cut = true;
        } else {
            kept.extend_from_slice(chunk);
        }
        let used = chunk.len() + usize::from(done);
        reader.consume(used);
        if done {
            return Ok(Some((String::from_utf8_lossy(&kept).into_owned(), cut)));
        }
    }
}

/// One pipe → the channel, filtered. Ends at EOF or when the receiver is
/// gone (the owning task was aborted, or stopped on a flood).
fn spawn_pump<R: AsyncRead + Unpin + Send + 'static>(
    pipe: Option<R>,
    filter: Arc<Option<regex::Regex>>,
    tx: tokio::sync::mpsc::Sender<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(pipe) = pipe else { return };
        let mut reader = BufReader::new(pipe);
        while let Ok(Some((mut line, cut))) = read_capped_line(&mut reader, MAX_LINE_BYTES).await {
            if cut {
                line.push_str(" […cut at 4 KiB]");
            }
            if filter.as_ref().as_ref().is_some_and(|r| !r.is_match(&line)) {
                continue;
            }
            if tx.send(line).await.is_err() {
                return;
            }
        }
    })
}

pub async fn listen(
    action: &Action,
    handles: SharedHandles,
    config: &crate::config::AppConfig,
) -> ActionResult {
    let Action::Listen {
        command,
        args,
        filter,
        window_ms,
        max_lines,
        persistent,
    } = action
    else {
        return ActionResult::err("Listen", ErrorKind::InvalidInput, "not a Listen action", 0);
    };

    // `persistent` is accepted and NOT honoured yet. Said out loud rather
    // than silently ignored: a flag that reads as supported and does nothing
    // is this repository's signature defect, and it has twenty-one entries.
    if *persistent {
        return ActionResult::err(
            "Listen",
            ErrorKind::NotImplemented,
            "persistent listeners are not built yet — the listener would die with \
             this process, so accepting the flag would be a promise the runtime \
             cannot keep. Run without it.",
            0,
        );
    }

    let re = match compile_filter(filter.as_ref()) {
        Ok(r) => Arc::new(r),
        Err(e) => return ActionResult::err("Listen", ErrorKind::InvalidInput, e, 0),
    };

    let (policy, clamped) = clamp_policy(*window_ms, *max_lines);

    // SECURITY: `harden` before `spawn`, never the other way round. Without
    // it the process inherits the operator's environment — every secret in
    // `~/.x0ne/.env` — which is exactly the 2026-08-14 leak. The test
    // `every_spawn_in_the_crate_goes_through_the_env_gate` reads this
    // crate's SOURCE to demand it, and it caught a bare `Command::new` here
    // on 2026-09-10.
    //
    // `kill_on_drop`: the task below owns the child, so aborting the task
    // (`HandleManager::cancel`, reached by `StopHandle`) drops it, and the
    // drop kills. Without this line a stopped listener was a status change
    // and nothing else.
    //
    // SECURITY: `stdin(null)`. In `--mode stdio` fd 0 is the bridge pipe the
    // orchestrator talks through; a child that inherits it can eat the next
    // action line or set `O_NONBLOCK` on the shared description — the
    // 2026-08-14 EAGAIN death, `tests/bridge_stdio_survives_children.rs`.
    // A listener has nothing to read from anyway.
    // SECURITY: confined like RunCommand; refused if it cannot be.
    let spawn = match crate::executor::isolation::plan(
        &config.isolation,
        &config.policy,
        &config.desktop,
        command,
        args,
        None,
        &Default::default(),
    ) {
        Ok(s) => s,
        Err(e) if e.kind() == crate::executor::isolation::UNAVAILABLE_KIND => {
            return ActionResult::err("Listen", ErrorKind::MissingDependency, e.to_string(), 0)
        }
        Err(e) => {
            return ActionResult::err(
                "Listen",
                ErrorKind::ExecutionFailed,
                format!("{command}: {e}"),
                0,
            )
        }
    };
    let mut cmd = Command::new(&spawn.program);
    cmd.args(&spawn.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    crate::child_env::harden(&mut cmd);

    let id = gen_id("listen");
    // Register FIRST, spawn second. The concurrent-handle ceiling is refused
    // here, before anything runs: the first version spawned three lines
    // above this comment while the comment claimed the opposite, so a
    // refused 21st listener was a running, unregistered, unkillable process.
    if let Err(e) = handles
        .lock()
        .await
        .register(id.clone(), HandleType::Process)
    {
        return ActionResult::err("Listen", ErrorKind::ExecutionFailed, e, 0);
    }
    let started = std::time::Instant::now();

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            handles.lock().await.set_status(
                &id,
                HandleStatus::Failed {
                    at: Utc::now().to_rfc3339(),
                    error: format!("{command}: {e}"),
                },
            );
            return ActionResult::err(
                "Listen",
                ErrorKind::ExecutionFailed,
                format!("{command}: {e}"),
                0,
            );
        }
    };
    let pid = child.id();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(PUMP_CAPACITY);
    let out_pump = spawn_pump(child.stdout.take(), re.clone(), tx.clone());
    let err_pump = spawn_pump(child.stderr.take(), re, tx);

    let sink = handles.clone();
    let hid = id.clone();
    let task = tokio::spawn(async move {
        let mut batcher = Batcher::new(policy);
        let now = || started.elapsed().as_millis() as u64;
        let window = std::time::Duration::from_millis(policy.window_ms);
        loop {
            // Sleep until a line arrives — or, if a batch is open, until its
            // window closes, so a quiet stream costs no wakeups at all and a
            // batch that stopped growing is released on time rather than at
            // the next poll boundary.
            let deadline = batcher
                .open_since_ms()
                .map(|first| started + std::time::Duration::from_millis(first) + window);
            let next = match deadline {
                Some(at) => tokio::time::timeout_at(at.into(), rx.recv()).await,
                None => Ok(rx.recv().await),
            };
            match next {
                Ok(Some(line)) => match batcher.push(line, now()) {
                    Verdict::Hold => {}
                    Verdict::Emit(b) => emit(&sink, &hid, b).await,
                    Verdict::StopFlooding {
                        lines_seen,
                        over_ms,
                        rate_per_s,
                    } => {
                        // The child is killed, not just ignored: a process
                        // that talks with no listener has no reason left to
                        // run (same rule as the stream budget in process.rs).
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        let last = batcher.flush(now());
                        let mut mgr = sink.lock().await;
                        if let Some(b) = last {
                            mgr.push_event(&hid, batch_event(b));
                        }
                        mgr.push_event(
                            &hid,
                            json!({
                                "listen": "stopped",
                                "why": "flooding",
                                "lines_seen": lines_seen,
                                "sustained_ms": over_ms,
                                "rate_per_s": rate_per_s,
                            }),
                        );
                        mgr.set_status(
                            &hid,
                            HandleStatus::Cancelled {
                                at: Utc::now().to_rfc3339(),
                            },
                        );
                        return;
                    }
                },
                // Both pumps are gone: the pipes hit EOF.
                Ok(None) => break,
                // The window closed with no new line.
                Err(_) => {
                    if let Verdict::Emit(b) = batcher.tick(now()) {
                        emit(&sink, &hid, b).await;
                    }
                }
            }
        }
        let _ = out_pump.await;
        let _ = err_pump.await;
        let last = batcher.flush(now());
        let exit = child.wait().await;
        let mut mgr = sink.lock().await;
        if let Some(b) = last {
            mgr.push_event(&hid, batch_event(b));
        }
        match exit {
            Ok(status) => {
                mgr.push_event(
                    &hid,
                    json!({ "listen": "exit", "exit_code": status.code().unwrap_or(-1) }),
                );
                mgr.set_status(
                    &hid,
                    HandleStatus::Completed {
                        at: Utc::now().to_rfc3339(),
                    },
                );
            }
            Err(e) => mgr.set_status(
                &hid,
                HandleStatus::Failed {
                    at: Utc::now().to_rfc3339(),
                    error: e.to_string(),
                },
            ),
        }
    });

    {
        let mut mgr = handles.lock().await;
        mgr.set_task(&id, task);
        mgr.set_pid(&id, pid);
        // The handle, and the settings actually in force. A caller who does
        // not see which ceilings apply cannot read a truncated batch — and
        // one who asked for more than the ceiling must see that it was cut.
        mgr.push_event(
            &id,
            json!({
                "listen": "started",
                "command": command,
                "filter": filter,
                "window_ms": policy.window_ms,
                "max_lines": policy.max_lines,
                "clamped": clamped,
            }),
        );
    }

    ActionResult::ok(
        "Listen",
        ActionOutput::HandleCreated {
            handle: id,
            handle_type: "listen".into(),
            streaming: true,
            pid,
        },
        started.elapsed().as_millis() as u64,
    )
}

/// One batch, one event. `dropped` travels with it — a reader that saw only
/// the lines would believe it saw everything, which is the exact failure the
/// batching policy exists to prevent. Takes the batch by value so the lines
/// move into the event instead of being copied.
fn batch_event(b: super::listen_batch::Batch) -> serde_json::Value {
    let complete = b.is_complete();
    json!({
        "listen": "batch",
        "lines": b.lines,
        "dropped": b.dropped,
        "complete": complete,
        "first_ms": b.first_ms,
        "last_ms": b.last_ms,
    })
}

async fn emit(handles: &SharedHandles, id: &str, b: super::listen_batch::Batch) {
    handles.lock().await.push_event(id, batch_event(b));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ceilings are applied and named. A silent clamp is worse than
    /// none — the model would read a 200-line batch as the 10 000 it asked
    /// for.
    #[test]
    fn model_supplied_limits_are_clamped_and_the_clamp_is_named() {
        let (p, clamped) = clamp_policy(Some(u64::MAX), Some(10_000_000));
        assert_eq!(p.window_ms, MAX_WINDOW_MS);
        assert_eq!(p.max_lines, MAX_BATCH_LINES);
        assert_eq!(clamped.len(), 2, "{clamped:?}");
        let (p, clamped) = clamp_policy(None, None);
        assert_eq!((p.window_ms, p.max_lines), (400, 20));
        assert!(clamped.is_empty());
        let (p, _) = clamp_policy(Some(0), Some(0));
        assert_eq!(p.max_lines, 1, "zero lines per batch would emit nothing");
    }

    /// A line past the cap is cut, the remainder is consumed and never
    /// buffered, and the next line is still read correctly.
    #[tokio::test]
    async fn an_oversized_line_is_cut_and_the_stream_continues() {
        let long = "x".repeat(10_000);
        let input = format!("{long}\nshort\n");
        let mut reader = BufReader::new(std::io::Cursor::new(input.into_bytes()));
        let (l1, cut1) = read_capped_line(&mut reader, 100).await.unwrap().unwrap();
        assert_eq!(l1.len(), 100);
        assert!(cut1);
        let (l2, cut2) = read_capped_line(&mut reader, 100).await.unwrap().unwrap();
        assert_eq!(l2, "short");
        assert!(!cut2);
        assert!(read_capped_line(&mut reader, 100).await.unwrap().is_none());
    }

    /// A last line without a trailing newline is not lost.
    #[tokio::test]
    async fn a_partial_last_line_is_delivered_at_eof() {
        let mut reader = BufReader::new(std::io::Cursor::new(b"a\nb".to_vec()));
        assert_eq!(
            read_capped_line(&mut reader, 10).await.unwrap().unwrap().0,
            "a"
        );
        assert_eq!(
            read_capped_line(&mut reader, 10).await.unwrap().unwrap().0,
            "b"
        );
        assert!(read_capped_line(&mut reader, 10).await.unwrap().is_none());
    }
}
