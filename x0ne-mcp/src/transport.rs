use super::jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use anyhow::{anyhow, Result};
use std::io::{self, BufRead, BufReader, Write};

/// Line-delimited JSON-RPC over stdin/stdout — the wire format expected by
/// MCP clients launched as subprocesses.
///
/// Blocking I/O on the OS handle is fine for the single-threaded MCP server
/// loop: there is exactly one reader and one writer per process.
pub struct StdioTransport {
    reader: BufReader<io::Stdin>,
    writer: io::Stdout,
}

impl StdioTransport {
    pub fn new() -> Self {
        Self {
            reader: BufReader::new(io::stdin()),
            writer: io::stdout(),
        }
    }

    /// One request, at most [`MAX_MESSAGE_BYTES`] long.
    pub async fn read_message(&mut self) -> Result<JsonRpcRequest> {
        read_message_from(&mut self.reader, MAX_MESSAGE_BYTES)
    }

    /// One raw line, `None` at EOF. For a server that answers malformed
    /// input itself instead of losing it in [`Self::read_message`].
    pub async fn read_line(&mut self) -> Result<Option<String>> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        Ok((n > 0).then_some(line))
    }

    /// [`Self::read_line`] that never buffers more than `max` bytes of a line.
    pub async fn read_line_bounded(&mut self, max: usize) -> Result<BoundedLine> {
        Ok(read_bounded_line(&mut self.reader, max)?)
    }

    pub async fn write_message(&mut self, msg: &JsonRpcResponse) -> Result<()> {
        let mut line = serde_json::to_string(msg)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }
}

impl Default for StdioTransport {
    fn default() -> Self {
        Self::new()
    }
}

/// Cap on one JSON-RPC line. An unbounded `read_line` let a client grow the
/// server's memory without limit.
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// Read and parse one request under `max` bytes. `EOF` is its own error text.
pub fn read_message_from(reader: &mut impl BufRead, max: usize) -> Result<JsonRpcRequest> {
    match read_bounded_line(reader, max)? {
        BoundedLine::Eof => Err(anyhow!("EOF")),
        BoundedLine::TooLong { bytes } => {
            Err(anyhow!("message of {bytes} bytes refused (limit {max})"))
        }
        BoundedLine::Line(line) => Ok(serde_json::from_str(&line)?),
    }
}

/// One line read under a byte cap.
#[derive(Debug, PartialEq, Eq)]
pub enum BoundedLine {
    Eof,
    Line(String),
    /// Longer than the cap: discarded up to its newline, `bytes` long in all.
    TooLong {
        bytes: usize,
    },
}

/// Read one `\n`-terminated line, keeping at most `max` bytes (newline
/// included). A longer line is consumed to its end without being kept.
/// Invalid UTF-8 is an `InvalidData` error once the line is consumed.
pub fn read_bounded_line(reader: &mut impl BufRead, max: usize) -> io::Result<BoundedLine> {
    let mut kept: Vec<u8> = Vec::new();
    let mut total = 0usize;
    loop {
        let buf = match reader.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if buf.is_empty() {
            break;
        }
        let (chunk, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (&buf[..=i], true),
            None => (buf, false),
        };
        let n = chunk.len();
        total += n;
        if total <= max {
            kept.extend_from_slice(chunk);
        } else {
            kept = Vec::new();
        }
        reader.consume(n);
        if done {
            break;
        }
    }
    if total == 0 {
        return Ok(BoundedLine::Eof);
    }
    if total > max {
        return Ok(BoundedLine::TooLong { bytes: total });
    }
    String::from_utf8(kept)
        .map(BoundedLine::Line)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 9 MiB line is refused and the next request still parses.
    #[test]
    fn an_oversized_message_is_refused_and_the_stream_goes_on() {
        let mut data = vec![b'x'; 9 * 1024 * 1024];
        data.push(b'\n');
        data.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n");
        let mut r = io::Cursor::new(data);
        let err = read_message_from(&mut r, MAX_MESSAGE_BYTES).unwrap_err();
        assert!(err.to_string().contains("refused"), "{err}");
        assert_eq!(
            read_message_from(&mut r, MAX_MESSAGE_BYTES).unwrap().method,
            "tools/list"
        );
        assert_eq!(
            read_message_from(&mut r, MAX_MESSAGE_BYTES)
                .unwrap_err()
                .to_string(),
            "EOF"
        );
    }
    use std::io::Cursor;

    #[test]
    fn a_line_over_the_cap_is_skipped_and_the_next_one_is_read() {
        let big = "x".repeat(100);
        let input = format!("{big}\n{{\"a\":1}}\n");
        let mut r = BufReader::with_capacity(8, Cursor::new(input.into_bytes()));
        assert_eq!(
            read_bounded_line(&mut r, 16).unwrap(),
            BoundedLine::TooLong { bytes: 101 }
        );
        assert_eq!(
            read_bounded_line(&mut r, 16).unwrap(),
            BoundedLine::Line("{\"a\":1}\n".into())
        );
        assert_eq!(read_bounded_line(&mut r, 16).unwrap(), BoundedLine::Eof);
    }

    #[test]
    fn a_line_at_the_cap_is_read_whole() {
        let mut r = Cursor::new(b"0123456789\nlast".to_vec());
        assert_eq!(
            read_bounded_line(&mut r, 11).unwrap(),
            BoundedLine::Line("0123456789\n".into())
        );
        // A final line without newline is still a line.
        assert_eq!(
            read_bounded_line(&mut r, 11).unwrap(),
            BoundedLine::Line("last".into())
        );
    }

    #[test]
    fn an_unterminated_oversized_tail_is_too_long_not_buffered() {
        let mut r = Cursor::new(vec![b'x'; 1000]);
        assert_eq!(
            read_bounded_line(&mut r, 10).unwrap(),
            BoundedLine::TooLong { bytes: 1000 }
        );
        assert_eq!(read_bounded_line(&mut r, 10).unwrap(), BoundedLine::Eof);
    }

    #[test]
    fn invalid_utf8_is_an_error_after_the_line_is_consumed() {
        let mut r = Cursor::new(b"\xff\xfe\nok\n".to_vec());
        assert!(read_bounded_line(&mut r, 64).is_err());
        assert_eq!(
            read_bounded_line(&mut r, 64).unwrap(),
            BoundedLine::Line("ok\n".into())
        );
    }
}
