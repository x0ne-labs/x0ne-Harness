//! Minimal RFB (VNC) protocol client — **input only**.
//!
//! We need just enough of the RFB protocol to:
//! 1. Complete the v3.8 handshake with `None` (1) authentication.
//! 2. Receive `ServerInit` so we know the framebuffer size.
//! 3. Send `KeyEvent` (msg=4) and `PointerEvent` (msg=5) messages.
//!
//! We do **not** decode framebuffer updates — screenshot capture goes through
//! `virsh screenshot`, which is simpler than parsing CopyRect/Hextile/etc.
//!
//! Reference: RFC 6143 — The Remote Framebuffer Protocol.

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MSG_KEY_EVENT: u8 = 4;
const MSG_POINTER_EVENT: u8 = 5;

#[derive(Debug, Clone, Copy)]
pub struct Framebuffer {
    pub width: u16,
    pub height: u16,
}

pub struct VncInputClient {
    stream: TcpStream,
    pub framebuffer: Framebuffer,
    /// Current button state (sticky between events, RFB style).
    button_mask: u8,
    /// Last (x, y) we sent — we never receive cursor updates back over RFB
    /// without subscribing to the cursor pseudo-encoding, so client-side
    /// tracking is the cheapest read-back.
    last_x: i32,
    last_y: i32,
}

impl VncInputClient {
    /// Connect, handshake, and return a client ready to send input events.
    /// Endpoint format: `host:port` (e.g. `127.0.0.1:5901`).
    pub async fn connect(endpoint: &str) -> Result<Self> {
        let stream = timeout(HANDSHAKE_TIMEOUT, TcpStream::connect(endpoint))
            .await
            .with_context(|| format!("VNC connect timed out: {endpoint}"))?
            .with_context(|| format!("VNC connect failed: {endpoint}"))?;
        let mut s = stream;

        timeout(HANDSHAKE_TIMEOUT, handshake(&mut s))
            .await
            .map_err(|_| anyhow!("VNC handshake timed out"))??;

        let fb = timeout(HANDSHAKE_TIMEOUT, read_server_init(&mut s))
            .await
            .map_err(|_| anyhow!("VNC ServerInit timed out"))??;

        Ok(Self {
            stream: s,
            framebuffer: fb,
            button_mask: 0,
            last_x: 0,
            last_y: 0,
        })
    }

    /// Last (x, y) sent. Used by `LibvirtSession::cursor_position` so the
    /// agent can sanity-check what it just did.
    pub fn last_position(&self) -> (i32, i32) {
        (self.last_x, self.last_y)
    }

    pub async fn pointer_move(&mut self, x: i32, y: i32) -> Result<()> {
        self.send_pointer(x, y, self.button_mask).await
    }

    pub async fn pointer_button_down(&mut self, x: i32, y: i32, mask: u8) -> Result<()> {
        self.button_mask |= mask;
        self.send_pointer(x, y, self.button_mask).await
    }

    pub async fn pointer_button_up(&mut self, x: i32, y: i32, mask: u8) -> Result<()> {
        self.button_mask &= !mask;
        self.send_pointer(x, y, self.button_mask).await
    }

    /// Press + release the given mouse button at (x, y).
    pub async fn pointer_click(&mut self, x: i32, y: i32, mask: u8) -> Result<()> {
        self.pointer_move(x, y).await?;
        self.pointer_button_down(x, y, mask).await?;
        self.pointer_button_up(x, y, mask).await?;
        Ok(())
    }

    /// Send one scroll "click" (detent): press the scroll-direction button,
    /// release it, at the current cursor position. RFB doesn't have a
    /// dedicated scroll message — scroll wheel ticks are encoded as
    /// down+up on bits 4-7 of the PointerEvent button mask.
    pub async fn pointer_scroll(&mut self, mask: u8, clicks: u32) -> Result<()> {
        for _ in 0..clicks {
            self.send_pointer(self.last_x, self.last_y, self.button_mask | mask)
                .await?;
            self.send_pointer(self.last_x, self.last_y, self.button_mask)
                .await?;
        }
        Ok(())
    }

    pub async fn pointer_drag(
        &mut self,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        mask: u8,
    ) -> Result<()> {
        self.pointer_move(x1, y1).await?;
        self.pointer_button_down(x1, y1, mask).await?;
        // Interpolate a few intermediate points so guest sees a real drag.
        for step in 1..=4 {
            let t = step as f32 / 5.0;
            let xi = (x1 as f32 + (x2 - x1) as f32 * t) as i32;
            let yi = (y1 as f32 + (y2 - y1) as f32 * t) as i32;
            self.send_pointer(xi, yi, self.button_mask).await?;
        }
        self.send_pointer(x2, y2, self.button_mask).await?;
        self.pointer_button_up(x2, y2, mask).await?;
        Ok(())
    }

    /// Press + release a single keysym.
    pub async fn key_tap(&mut self, keysym: u32) -> Result<()> {
        self.send_key(true, keysym).await?;
        self.send_key(false, keysym).await?;
        Ok(())
    }

    /// Send a key chord: hold modifiers, tap main key, release modifiers.
    ///
    /// **Invariant**: modifiers are always released, even if any intermediate
    /// send fails. Without this, a transient network blip during a long
    /// `vm_type` would leave Shift_L (or Ctrl/Alt) stuck pressed in the
    /// guest's view, garbling every subsequent keystroke (`base64` → `BASE^$`).
    pub async fn key_chord(&mut self, modifiers: &[u32], main: u32) -> Result<()> {
        // Press modifiers, then main down/up — collect any error to propagate
        // AFTER we've cleanly released the modifiers.
        let mut held = Vec::with_capacity(modifiers.len());
        let mut err: Option<anyhow::Error> = None;
        for &m in modifiers {
            if let Err(e) = self.send_key(true, m).await {
                err = Some(e);
                break;
            }
            held.push(m);
        }
        if err.is_none() {
            if let Err(e) = self.send_key(true, main).await {
                err = Some(e);
            }
        }
        if err.is_none() {
            if let Err(e) = self.send_key(false, main).await {
                err = Some(e);
            }
        }
        // Best-effort release of every modifier we actually pressed, even if
        // we hit an error. Errors during release are swallowed (best we can do).
        for m in held.into_iter().rev() {
            let _ = self.send_key(false, m).await;
        }
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    async fn send_pointer(&mut self, x: i32, y: i32, mask: u8) -> Result<()> {
        let xc = clamp_u16(x);
        let yc = clamp_u16(y);
        let mut buf = [0u8; 6];
        buf[0] = MSG_POINTER_EVENT;
        buf[1] = mask;
        buf[2..4].copy_from_slice(&xc.to_be_bytes());
        buf[4..6].copy_from_slice(&yc.to_be_bytes());
        self.stream.write_all(&buf).await?;
        self.stream.flush().await?;
        // Track for cursor_position() readback.
        self.last_x = xc as i32;
        self.last_y = yc as i32;
        Ok(())
    }

    async fn send_key(&mut self, down: bool, keysym: u32) -> Result<()> {
        let mut buf = [0u8; 8];
        buf[0] = MSG_KEY_EVENT;
        buf[1] = if down { 1 } else { 0 };
        // bytes 2..4 are padding (zero)
        buf[4..8].copy_from_slice(&keysym.to_be_bytes());
        self.stream.write_all(&buf).await?;
        self.stream.flush().await?;
        Ok(())
    }
}

async fn handshake(s: &mut TcpStream) -> Result<()> {
    // 1. ProtocolVersion: server sends 12 bytes "RFB 003.xxx\n".
    let mut ver = [0u8; 12];
    s.read_exact(&mut ver)
        .await
        .context("read ProtocolVersion")?;
    if !ver.starts_with(b"RFB 003.") {
        bail!(
            "not an RFB server (got {:?})",
            String::from_utf8_lossy(&ver)
        );
    }
    // We unconditionally reply with v3.8 — every server post-2010 supports it.
    s.write_all(b"RFB 003.008\n")
        .await
        .context("write ProtocolVersion")?;
    s.flush().await?;

    // 2. Security types: 1 byte count + count bytes of types. 0 means failure
    //    followed by a reason string.
    let count = s.read_u8().await.context("read sec count")?;
    if count == 0 {
        let reason_len = s.read_u32().await.context("read sec failure len")?;
        let mut reason = vec![0u8; reason_len as usize];
        s.read_exact(&mut reason).await.ok();
        bail!("VNC security failure: {}", String::from_utf8_lossy(&reason));
    }
    let mut types = vec![0u8; count as usize];
    s.read_exact(&mut types).await.context("read sec types")?;
    if !types.contains(&1) {
        bail!(
            "VNC server requires authentication (types: {:?}) — only 'None' supported in v0",
            types
        );
    }
    s.write_u8(1).await.context("write sec choice")?; // 1 = None
    s.flush().await?;

    // 3. SecurityResult: u32, 0 = OK, non-zero = failure.
    let result = s.read_u32().await.context("read SecurityResult")?;
    if result != 0 {
        bail!("VNC authentication rejected (SecurityResult={result})");
    }

    // 4. ClientInit: send shared-flag = 1 (don't kick other clients).
    s.write_u8(1).await.context("write ClientInit")?;
    s.flush().await?;
    Ok(())
}

async fn read_server_init(s: &mut TcpStream) -> Result<Framebuffer> {
    let width = s.read_u16().await.context("read width")?;
    let height = s.read_u16().await.context("read height")?;
    // Skip 16-byte PIXEL_FORMAT + variable-length desktop name.
    let mut pf = [0u8; 16];
    s.read_exact(&mut pf).await.context("read pixel format")?;
    let name_len = s.read_u32().await.context("read name length")?;
    if name_len > 8192 {
        bail!("VNC desktop name absurdly long: {name_len} bytes");
    }
    let mut name = vec![0u8; name_len as usize];
    s.read_exact(&mut name).await.context("read name")?;
    Ok(Framebuffer { width, height })
}

fn clamp_u16(v: i32) -> u16 {
    v.clamp(0, u16::MAX as i32) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_u16_handles_extremes() {
        assert_eq!(clamp_u16(-1), 0);
        assert_eq!(clamp_u16(0), 0);
        assert_eq!(clamp_u16(100), 100);
        assert_eq!(clamp_u16(70_000), u16::MAX);
    }

    /// Round-trip the handshake against a fake server in another tokio task —
    /// proves our wire-format reads/writes are byte-accurate.
    #[tokio::test]
    async fn handshake_succeeds_with_no_auth() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            // 1. send protocol version
            s.write_all(b"RFB 003.008\n").await.unwrap();
            // read client version
            let mut buf = [0u8; 12];
            s.read_exact(&mut buf).await.unwrap();
            // 2. security: offer None(1)
            s.write_all(&[1, 1]).await.unwrap();
            // read choice
            let mut c = [0u8; 1];
            s.read_exact(&mut c).await.unwrap();
            // 3. SecurityResult OK
            s.write_all(&0u32.to_be_bytes()).await.unwrap();
            // 4. read ClientInit
            s.read_exact(&mut c).await.unwrap();
            // 5. ServerInit: 800x600, 16 bytes pixel-format, name "test"
            s.write_all(&800u16.to_be_bytes()).await.unwrap();
            s.write_all(&600u16.to_be_bytes()).await.unwrap();
            s.write_all(&[0u8; 16]).await.unwrap();
            s.write_all(&4u32.to_be_bytes()).await.unwrap();
            s.write_all(b"test").await.unwrap();
            // Eat one PointerEvent to confirm we can send input
            let mut evt = [0u8; 6];
            s.read_exact(&mut evt).await.unwrap();
            assert_eq!(evt[0], MSG_POINTER_EVENT);
        });

        let mut client = VncInputClient::connect(&addr.to_string()).await.unwrap();
        assert_eq!(client.framebuffer.width, 800);
        assert_eq!(client.framebuffer.height, 600);
        client.pointer_move(10, 20).await.unwrap();
        server.await.unwrap();
    }
}
