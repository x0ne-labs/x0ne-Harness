//! One systemd journal entry per hook decision, native protocol, no library.
//!
//! Contract read by `x0ne-hardening watch` (x0ne OS):
//! `SYSLOG_IDENTIFIER=x0ne-harness`, `X0NE_EVENT=harness`, `X0NE_OUTCOME`
//! and further `X0NE_*` fields. Protocol: systemd's JOURNAL_NATIVE_PROTOCOL
//! (one datagram, `KEY=value\n`, or `KEY\n<u64 LE length><value>\n` when the
//! value holds a newline).

use std::os::unix::net::UnixDatagram;
use std::path::Path;

pub const DEFAULT_SOCKET: &str = "/run/systemd/journal/socket";
pub const IDENTIFIER: &str = "x0ne-harness";

/// Longest value sent, in bytes: a command line can be a whole script.
const MAX_VALUE: usize = 2048;

fn truncate(v: &str) -> &str {
    if v.len() <= MAX_VALUE {
        return v;
    }
    let mut end = MAX_VALUE;
    while !v.is_char_boundary(end) {
        end -= 1;
    }
    &v[..end]
}

/// The datagram for `fields`.
pub fn encode(fields: &[(&str, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in fields {
        let v = truncate(v);
        out.extend_from_slice(k.as_bytes());
        if v.contains('\n') {
            out.push(b'\n');
            out.extend_from_slice(&(v.len() as u64).to_le_bytes());
            out.extend_from_slice(v.as_bytes());
        } else {
            out.push(b'=');
            out.extend_from_slice(v.as_bytes());
        }
        out.push(b'\n');
    }
    out
}

/// Send one entry. An error is returned, never raised: the decision stands
/// whether or not the journal took it.
pub fn send(socket: &Path, fields: &[(&str, String)]) -> std::io::Result<()> {
    let sock = UnixDatagram::unbound()?;
    sock.send_to(&encode(fields), socket)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_multiline_values() {
        let d = encode(&[("A", "x".into()), ("B", "l1\nl2".into())]);
        let mut want = b"A=x\nB\n".to_vec();
        want.extend_from_slice(&5u64.to_le_bytes());
        want.extend_from_slice(b"l1\nl2\n");
        assert_eq!(d, want);
    }

    #[test]
    fn long_values_are_cut_on_a_char_boundary() {
        let v = "é".repeat(MAX_VALUE);
        let t = truncate(&v);
        assert!(t.len() <= MAX_VALUE && t.chars().all(|c| c == 'é'));
    }
}
