//! The copy of the history a model call carries.
//!
//! The history itself is left untouched (display, resume, journal read it);
//! only the request is slimmed. Replay of 2026-10-06
//! (`driver::tests::token_replay_of_the_mini_nmap_task`): action echoes and
//! tool results were 51% of the input, resent on every call.

use crate::canonical_json::canonical;
use crate::conversation::{Message, Role};
use serde_json::Value;

/// Action frames (echo + result) kept whole, counted from the end.
pub const RECENT_FRAMES: usize = 3;
/// An older frame keeps this many leading chars, then an elision marker.
pub const AGED_HEAD_CHARS: usize = 400;
/// And this many trailing chars (a compiler's last error, `</result>`).
pub const AGED_TAIL_CHARS: usize = 200;

const RESULT_OPEN: &str = "<result action=\"";
const RESULT_CLOSE: &str = "</result>";
const ACTION_MARK: &str = "[ACTION]\n";

/// Envelope fields the model never needs: bookkeeping, or a repeat of the
/// action it just sent.
const NOISE: &[&str] = &["id", "timestamp", "duration_ms", "action_type"];

/// The messages to send: JSON compacted, envelope noise dropped, frames older
/// than [`RECENT_FRAMES`] cut to head and tail.
pub fn for_model(messages: &[Message]) -> Vec<Message> {
    let frames: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| is_frame(m))
        .map(|(i, _)| i)
        .collect();
    // Two messages (echo, result) per frame.
    let keep_from = frames
        .len()
        .checked_sub(RECENT_FRAMES * 2)
        .map(|k| frames[k])
        .unwrap_or(0);
    messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if !is_frame(m) {
                return m.clone();
            }
            let mut content = compact_frame(&m.content);
            if i < keep_from {
                content = age(&content);
            }
            Message {
                role: m.role.clone(),
                content,
                images: m.images.clone(),
            }
        })
        .collect()
}

fn is_frame(m: &Message) -> bool {
    match m.role {
        Role::Assistant => m.content.contains(ACTION_MARK),
        Role::User => m.content.contains(RESULT_OPEN),
    }
}

/// Re-serialize the JSON body of an echo or a result without indentation.
fn compact_frame(content: &str) -> String {
    if let Some(at) = content.find(ACTION_MARK) {
        let body = &content[at + ACTION_MARK.len()..];
        return match serde_json::from_str::<Value>(body) {
            Ok(v) => format!("{}{}{}", &content[..at], ACTION_MARK, canonical(&v)),
            Err(_) => content.to_string(),
        };
    }
    let Some(open) = content.find(RESULT_OPEN) else {
        return content.to_string();
    };
    let Some(head_end) = content[open..].find(">\n").map(|e| open + e + 2) else {
        return content.to_string();
    };
    let Some(close) = content.rfind(RESULT_CLOSE) else {
        return content.to_string();
    };
    if close < head_end {
        return content.to_string();
    }
    match serde_json::from_str::<Value>(content[head_end..close].trim_end()) {
        Ok(mut v) => {
            strip_noise(&mut v);
            format!(
                "{}{}\n{}",
                &content[..head_end],
                canonical(&v),
                &content[close..]
            )
        }
        // A Pipeline summary is prose, not JSON: left as is.
        Err(_) => content.to_string(),
    }
}

fn strip_noise(v: &mut Value) {
    let Some(obj) = v.as_object_mut() else {
        return;
    };
    for k in NOISE {
        obj.remove(*k);
    }
    if let Some(Value::Object(out)) = obj.get_mut("output") {
        if out.get("kind").and_then(|k| k.as_str()) == Some("command_output") {
            out.remove("command");
            out.remove("args");
        }
        out.retain(|_, v| !matches!(v, Value::Null | Value::Bool(false)));
        out.retain(|_, v| !matches!(v, Value::String(s) if s.is_empty()));
    }
}

/// Head and tail of an older frame, with the number of chars elided.
fn age(content: &str) -> String {
    let n = content.chars().count();
    if n <= AGED_HEAD_CHARS + AGED_TAIL_CHARS + 80 {
        return content.to_string();
    }
    let head: String = content.chars().take(AGED_HEAD_CHARS).collect();
    let tail: String = content.chars().skip(n - AGED_TAIL_CHARS).collect();
    format!(
        "{head}\n[… {} chars of an older step elided; ReadFile or re-run to see them]\n{tail}",
        n - AGED_HEAD_CHARS - AGED_TAIL_CHARS
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_string(),
            images: vec![],
        }
    }

    fn frame(i: usize, body_len: usize) -> [Message; 2] {
        let action = json!({"type": "WriteFile", "path": format!("/work/f{i}"), "content": "x".repeat(body_len)});
        let result = json!({"id": "u", "success": true, "action_type": "WriteFile",
            "timestamp": "t", "duration_ms": 1, "output": {"kind": "write", "bytes": body_len}});
        [
            msg(
                Role::Assistant,
                &format!(
                    "[ACTION]\n{}",
                    serde_json::to_string_pretty(&action).unwrap()
                ),
            ),
            msg(
                Role::User,
                &format!(
                    "<result action=\"WriteFile\">\n{}\n</result>",
                    serde_json::to_string_pretty(&result).unwrap()
                ),
            ),
        ]
    }

    #[test]
    fn noise_is_dropped_and_json_compacted() {
        let r = json!({"id": "u", "success": true, "action_type": "RunCommand", "timestamp": "t",
            "duration_ms": 3, "output": {"kind": "command_output", "command": "cargo",
            "args": ["test"], "exit_code": 0, "stdout": "ok", "stderr": "", "timed_out": false}});
        let m = msg(
            Role::User,
            &format!(
                "<result action=\"RunCommand\">\n{}\n</result>",
                serde_json::to_string_pretty(&r).unwrap()
            ),
        );
        let out = for_model(&[m]);
        assert_eq!(
            out[0].content,
            "<result action=\"RunCommand\">\n{\"output\":{\"exit_code\":0,\"kind\":\"command_output\",\"stdout\":\"ok\"},\"success\":true}\n</result>"
        );
    }

    #[test]
    fn a_failure_keeps_its_error_and_exit_code() {
        let r = json!({"success": false, "error": {"class": "validation_error", "message": "missing field `path`"},
            "output": {"kind": "command_output", "exit_code": 101, "stderr": "error[E0425]"}});
        let m = msg(
            Role::User,
            &format!("<result action=\"RunCommand\">\n{}\n</result>", r),
        );
        let c = &for_model(&[m])[0].content;
        assert!(
            c.contains("missing field `path`") && c.contains("101") && c.contains("E0425"),
            "{c}"
        );
        assert!(c.contains("\"success\":false"), "{c}");
    }

    #[test]
    fn recent_frames_stay_whole_and_older_ones_are_cut() {
        let mut msgs = vec![msg(Role::User, "task")];
        for i in 0..5 {
            msgs.extend(frame(i, 3000));
        }
        let out = for_model(&msgs);
        assert_eq!(out[0].content, "task");
        // frames 0 and 1 aged, 2..5 whole.
        for (k, m) in out.iter().enumerate().skip(1) {
            let frame_no = (k - 1) / 2;
            let aged = m.content.contains("chars of an older step elided");
            if m.role == Role::Assistant {
                assert_eq!(aged, frame_no < 2, "message {k}");
            } else {
                assert!(!aged, "short results are never cut");
            }
        }
        assert!(out[1].content.len() < 800);
        assert!(out[out.len() - 2].content.contains(&"x".repeat(3000)));
    }

    #[test]
    fn prose_and_unparsable_frames_are_left_alone() {
        let msgs = vec![
            msg(Role::User, "[System Context]\ncatalog"),
            msg(Role::Assistant, "plain answer"),
            msg(
                Role::User,
                "<result action=\"Pipeline\">\nstep 1: ok\n</result>",
            ),
        ];
        let out = for_model(&msgs);
        for (a, b) in msgs.iter().zip(&out) {
            assert_eq!(a.content, b.content);
        }
    }

    #[test]
    fn the_history_is_not_modified() {
        let msgs: Vec<Message> = frame(0, 5000).into_iter().collect();
        let before: Vec<String> = msgs.iter().map(|m| m.content.clone()).collect();
        let _ = for_model(&msgs);
        let after: Vec<String> = msgs.iter().map(|m| m.content.clone()).collect();
        assert_eq!(before, after);
    }
}
