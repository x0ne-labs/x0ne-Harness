//! `Listen` on a real process: the filter holds, the batches admit.
//!
//! `listen_batch` proves the POLICY without spawning anything. This file
//! proves the wiring: that a real pipe, a real filter and a real batcher do
//! produce batches in the handle's buffer, where `ReadLogs` will fetch them.
//!
//! Both are needed, and that is the lesson of 2026-09-10: that day a banner
//! was "verified" three times by commands that redirected the output, hence
//! on a path the binary did not take. A pure policy test says nothing about
//! the wiring.

use x0ne_runtime::action::Action;
use x0ne_runtime::executor::listen::listen;
use x0ne_runtime::handles::new_shared;

fn cfg() -> x0ne_runtime::config::AppConfig {
    let mut c = x0ne_runtime::config::AppConfig::defaults();
    c.isolation.confine_helper_override = Some(env!("CARGO_BIN_EXE_x0ne-runtime").into());
    c
}

/// The filter lets through what it must, and nothing else; and nothing is
/// lost silently — what is not shown is counted.
#[tokio::test]
async fn a_real_stream_becomes_batches_and_the_filter_is_obeyed() {
    let handles = new_shared(256, 60, 8);
    let action = Action::Listen {
        command: "sh".into(),
        args: vec![
            "-c".into(),
            // Five lines to keep, five to drop, interleaved.
            "for i in 1 2 3 4 5; do echo \"[KEEP] ligne $i\"; echo bruit; done".into(),
        ],
        filter: Some(r"^\[KEEP\]".into()),
        window_ms: Some(50),
        max_lines: Some(3),
        persistent: false,
    };

    let result = listen(&action, handles.clone(), &cfg()).await;
    assert!(result.success, "listen failed: {result:?}");
    let handle = match result.output {
        Some(x0ne_runtime::result::ActionOutput::HandleCreated { handle, .. }) => handle,
        other => panic!("expected a handle, got {other:?}"),
    };

    // The process is short; give it time to finish and give the batcher
    // time to flush its last batch.
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;

    let events = handles
        .lock()
        .await
        .drain_events(&handle)
        .expect("the handle exists");
    let batches: Vec<_> = events
        .iter()
        .filter(|e| e["listen"] == "batch")
        .cloned()
        .collect();
    assert!(!batches.is_empty(), "no batch produced: {events:?}");

    let shown: Vec<String> = batches
        .iter()
        .flat_map(|b| b["lines"].as_array().cloned().unwrap_or_default())
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert!(
        shown.iter().all(|l| l.starts_with("[KEEP]")),
        "the filter let noise through: {shown:?}"
    );

    let dropped: u64 = batches.iter().filter_map(|b| b["dropped"].as_u64()).sum();
    assert_eq!(
        shown.len() as u64 + dropped,
        5,
        "five kept lines, shown or counted — never lost. \
         shown {shown:?}, counted {dropped}"
    );
}

/// A malformed filter is refused BEFORE anything runs.
///
/// A failure here means a process was spawned only to discover afterwards
/// that the regex does not compile — and it would then have to be reaped.
/// The regex comes from the model; it will be malformed one day.
#[tokio::test]
async fn a_malformed_filter_is_refused_before_anything_is_spawned() {
    let handles = new_shared(256, 60, 8);
    let action = Action::Listen {
        command: "sh".into(),
        args: vec!["-c".into(), "echo never".into()],
        filter: Some("(unclosed".into()),
        window_ms: None,
        max_lines: None,
        persistent: false,
    };
    let result = listen(&action, handles.clone(), &cfg()).await;
    assert!(!result.success, "an invalid regex must be refused");
    assert!(
        handles.lock().await.summaries().is_empty(),
        "no handle must have been created"
    );
}

/// `persistent` is refused as long as it is not built, and the refusal says
/// why.
///
/// Accepting it silently would be the promise this repository records twenty
/// times: a flag that reads as supported and changes nothing.
#[tokio::test]
async fn the_persistent_flag_is_refused_rather_than_quietly_ignored() {
    let handles = new_shared(256, 60, 8);
    let action = Action::Listen {
        command: "sh".into(),
        args: vec!["-c".into(), "echo never".into()],
        filter: None,
        window_ms: None,
        max_lines: None,
        persistent: true,
    };
    let result = listen(&action, handles.clone(), &cfg()).await;
    assert!(!result.success);
    let msg = format!("{:?}", result.error);
    assert!(
        msg.contains("persistent"),
        "the refusal must name what is not built: {msg}"
    );
    assert!(
        handles.lock().await.summaries().is_empty(),
        "nothing must have been started"
    );
}

/// `StopHandle` on a `Listen` kills the process. Not just the status.
///
/// The first version (morning of 2026-09-10) dropped the `Child` without
/// `kill_on_drop`, without `set_pid`, without `set_task`: `StopHandle` moved
/// the handle to `Cancelled` and the process, its reader task and its batches
/// went on for the whole life of the runtime. Code review caught it; this
/// test keeps it from coming back. A failure here: a `tail -f` the operator
/// believes stopped still holds its file.
#[tokio::test]
async fn stopping_a_listener_kills_its_process() {
    let handles = new_shared(256, 60, 8);
    let action = Action::Listen {
        command: "sh".into(),
        args: vec![
            "-c".into(),
            "while true; do echo tick; sleep 0.05; done".into(),
        ],
        filter: None,
        window_ms: Some(50),
        max_lines: None,
        persistent: false,
    };
    let result = listen(&action, handles.clone(), &cfg()).await;
    let (handle, pid) = match result.output {
        Some(x0ne_runtime::result::ActionOutput::HandleCreated { handle, pid, .. }) => {
            (handle, pid.expect("a process has a pid"))
        }
        other => panic!("expected a handle, got {other:?}"),
    };
    assert!(
        handles.lock().await.pid_of(&handle).is_some(),
        "the pid must be recorded on the handle, otherwise StopHandle cannot signal anything"
    );

    // It is running: at least one batch arrives.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(alive(pid), "precondition: the process is running");

    let stopped = x0ne_runtime::executor::events::stop_handle(&handle, handles.clone()).await;
    assert!(stopped.success, "{stopped:?}");

    // Dead or a zombie awaiting reaping, but no longer alive; allow it up to
    // two seconds, the signal is asynchronous.
    let mut waited = 0;
    while alive(pid) && waited < 2000 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        waited += 50;
    }
    assert!(
        !alive(pid),
        "process {pid} is still running after StopHandle"
    );
    let status = handles
        .lock()
        .await
        .get_status(&handle)
        .expect("the handle exists");
    assert!(
        status.is_terminal(),
        "the status must be terminal after the stop: {status:?}"
    );
}

/// "Alive" in the sense that matters here: present in /proc and not a zombie.
fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(s) => !s
            .lines()
            .any(|l| l.starts_with("State:") && l.contains('Z')),
        Err(_) => false,
    }
}

/// Both pipes are read at the same time. A child that fills stderr before
/// writing to stdout does not block listening.
///
/// The old loop read stdout until EOF, THEN stderr. A process writing 64 KiB
/// to stderr before its first stdout line blocked on the full pipe, stdout
/// never reached EOF, and nobody read anything anymore: a hung service, a
/// sleeping task. Here, 20,000 lines on stderr precede the only line kept;
/// bounded to five seconds to fail rather than hang.
#[tokio::test]
async fn stderr_is_drained_while_stdout_is_silent() {
    let handles = new_shared(256, 60, 8);
    let action = Action::Listen {
        command: "sh".into(),
        args: vec![
            "-c".into(),
            "i=0; while [ $i -lt 20000 ]; do echo \"stderr noise $i\" >&2; i=$((i+1)); done; echo DONE".into(),
        ],
        filter: Some("^DONE".into()),
        window_ms: Some(50),
        max_lines: None,
        persistent: false,
    };
    let result = listen(&action, handles.clone(), &cfg()).await;
    let handle = match result.output {
        Some(x0ne_runtime::result::ActionOutput::HandleCreated { handle, .. }) => handle,
        other => panic!("expected a handle, got {other:?}"),
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let st = handles.lock().await.get_status(&handle).expect("exists");
        if st.is_terminal() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "listen did not finish in 5 s: stderr is not read while stdout is silent"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let events = handles.lock().await.drain_events(&handle).expect("exists");
    let shown: Vec<String> = events
        .iter()
        .filter(|e| e["listen"] == "batch")
        .flat_map(|b| b["lines"].as_array().cloned().unwrap_or_default())
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert_eq!(
        shown,
        ["DONE"],
        "the kept line is there, and nothing else: {events:?}"
    );
}
