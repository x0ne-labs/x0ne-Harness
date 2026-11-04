use std::io::{self, BufRead};

use crate::action::Action;
use crate::config::AppConfig;
use crate::result::{ActionOutput, ActionResult, ErrorKind};

/// Read the last N lines of a log file.
pub async fn read_logs(action: &Action, config: &AppConfig) -> ActionResult {
    let Action::ReadLogs { path, lines } = action else {
        unreachable!()
    };

    let max_lines = lines.unwrap_or(100).min(config.policy.max_list_entries);

    let confined = match crate::executor::confine::resolve_read(path, &config.policy) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("ReadLogs", e.kind(), e.message(), 0),
    };

    // SECURITY: a line COUNT bounds nothing — measured 2 098 275 bytes returned
    // under a 65 536 policy, because 512 lines of 4 KiB is 2 MiB. Logs are
    // exactly where long lines live (a JSON record, a stack trace, a base64
    // payload), so the byte budget is not a theoretical case here.
    match read_tail(&confined, max_lines, config.policy.max_read_bytes) {
        Ok((tail_lines, total_lines)) => ActionResult::ok(
            "ReadLogs",
            ActionOutput::LogLines {
                path: path.clone(),
                lines: tail_lines,
                total_lines,
            },
            0,
        ),
        Err(e) => {
            let kind = match e.kind() {
                io::ErrorKind::NotFound => ErrorKind::NotFound,
                io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
                _ => ErrorKind::ExecutionFailed,
            };
            ActionResult::err("ReadLogs", kind, e.to_string(), 0)
        }
    }
}

/// Efficiently read the tail of a (potentially large) file.
///
/// Reads all lines into memory using a ring-buffer approach so
/// only the last `n` lines are kept at any given time.
/// `max_bytes` is `policy.max_read_bytes`: the ring is trimmed from the FRONT
/// when it exceeds it, because the tail is what was asked for — dropping the
/// oldest kept line is the answer that stays true to the question.
fn read_tail(
    confined: &crate::executor::confine::ConfinedPath,
    n: usize,
    max_bytes: u64,
) -> io::Result<(Vec<String>, usize)> {
    let file = confined
        .open_read()
        .map_err(|e| io::Error::other(e.message()))?;
    let reader = io::BufReader::new(file);
    let max = max_bytes as usize;

    // Ring buffer bounded twice: by `n` lines and by `max` bytes.
    let mut ring: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut ring_bytes = 0usize;
    let mut total = 0usize;

    for line in reader.lines() {
        let line = line?;
        total += 1;
        // A single line longer than the whole budget is shortened rather than
        // dropped: a log whose last record is one huge JSON blob must still
        // answer something.
        let line = crate::executor::limits::truncate_to(&line, max);
        ring_bytes += line.len() + 1;
        ring.push_back(line);
        while ring.len() > n || (ring_bytes > max && ring.len() > 1) {
            match ring.pop_front() {
                Some(old) => ring_bytes -= old.len() + 1,
                None => break,
            }
        }
    }

    Ok((ring.into_iter().collect(), total))
}
