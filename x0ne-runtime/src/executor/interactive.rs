use tokio::io::AsyncWriteExt;

use crate::action::Action;
use crate::handles::SharedHandles;
use crate::result::{ActionOutput, ActionResult, ErrorKind};

/// Send text input to a running streaming process's stdin pipe.
///
/// The handle must be:
/// - a `Process` handle created via `RunCommand { stream: true }`
/// - still in `Running` status
/// - have a captured stdin pipe (all streaming commands now do)
pub async fn send_input(action: &Action, handles: SharedHandles) -> ActionResult {
    let Action::SendInput { handle, input } = action else {
        unreachable!()
    };

    // 1. Check the handle exists and is still running
    let mgr = handles.lock().await;
    let status = match mgr.get_status(handle) {
        Some(s) => s,
        None => {
            return ActionResult::err(
                "SendInput",
                ErrorKind::NotFound,
                format!(
                    "Handle '{}' not found. Use ListHandles to see active handles.",
                    handle
                ),
                0,
            );
        }
    };

    if status.is_terminal() {
        return ActionResult::err(
            "SendInput",
            ErrorKind::InvalidInput,
            format!(
                "Handle '{}' is no longer running (status: {:?}). \
                 Cannot send input to a completed/failed/cancelled process.",
                handle, status
            ),
            0,
        );
    }

    // 2. Get the stdin pipe
    let stdin_pipe = match mgr.get_stdin(handle) {
        Some(s) => s,
        None => {
            return ActionResult::err(
                "SendInput",
                ErrorKind::InvalidInput,
                format!(
                    "Handle '{}' has no stdin pipe. Only streaming processes \
                     (RunCommand with stream: true) support SendInput.",
                    handle
                ),
                0,
            );
        }
    };

    // Drop the manager lock before writing (avoid holding it during I/O)
    drop(mgr);

    // 3. Write to stdin
    let bytes = input.as_bytes();
    let mut stdin_guard = stdin_pipe.lock().await;
    match stdin_guard.write_all(bytes).await {
        Ok(()) => {
            if let Err(e) = stdin_guard.flush().await {
                return ActionResult::err(
                    "SendInput",
                    ErrorKind::ExecutionFailed,
                    format!("Failed to flush stdin: {}", e),
                    0,
                );
            }
            ActionResult::ok(
                "SendInput",
                ActionOutput::InputSent {
                    handle: handle.clone(),
                    bytes_written: bytes.len(),
                },
                0,
            )
        }
        Err(e) => ActionResult::err(
            "SendInput",
            ErrorKind::ExecutionFailed,
            format!(
                "Failed to write to stdin of handle '{}': {}. \
                 The process may have closed its stdin or exited.",
                handle, e
            ),
            0,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handles;

    fn make_handles() -> SharedHandles {
        handles::new_shared(1000, 300, 20)
    }

    #[tokio::test]
    async fn send_input_handle_not_found() {
        let h = make_handles();
        let action = Action::SendInput {
            handle: "nonexistent_handle".into(),
            input: "hello\n".into(),
        };
        let result = send_input(&action, h).await;
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().message.contains("not found"));
    }

    #[tokio::test]
    async fn send_input_to_completed_process_fails() {
        let h = make_handles();
        {
            let mut mgr = h.lock().await;
            mgr.register("proc_test123", handles::HandleType::Process)
                .unwrap();
            mgr.set_status(
                "proc_test123",
                handles::HandleStatus::Completed {
                    at: "2026-01-01T00:00:00Z".into(),
                },
            );
        }
        let action = Action::SendInput {
            handle: "proc_test123".into(),
            input: "hello\n".into(),
        };
        let result = send_input(&action, h).await;
        assert!(!result.success);
        assert!(result
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("no longer running"));
    }

    #[tokio::test]
    async fn send_input_no_stdin_pipe() {
        let h = make_handles();
        {
            let mut mgr = h.lock().await;
            mgr.register("proc_nostdin", handles::HandleType::Process)
                .unwrap();
            // No stdin attached — simulates a handle without piped stdin
        }
        let action = Action::SendInput {
            handle: "proc_nostdin".into(),
            input: "hello\n".into(),
        };
        let result = send_input(&action, h).await;
        assert!(!result.success);
        assert!(result
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("no stdin pipe"));
    }

    #[tokio::test]
    async fn send_input_to_running_process() {
        use std::process::Stdio;
        use tokio::process::Command;

        let h = make_handles();
        // Spawn a real cat process that echoes stdin to stdout
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn cat");

        let child_stdin = child.stdin.take().unwrap();
        let shared_stdin = std::sync::Arc::new(tokio::sync::Mutex::new(child_stdin));

        {
            let mut mgr = h.lock().await;
            mgr.register("proc_cat", handles::HandleType::Process)
                .unwrap();
            mgr.set_stdin("proc_cat", shared_stdin);
        }

        let action = Action::SendInput {
            handle: "proc_cat".into(),
            input: "hello world\n".into(),
        };
        let result = send_input(&action, h).await;
        assert!(result.success, "send_input failed: {:?}", result.error);
        if let Some(ActionOutput::InputSent { bytes_written, .. }) = result.output {
            assert_eq!(bytes_written, 12); // "hello world\n"
        } else {
            panic!("expected InputSent output");
        }
    }
}
