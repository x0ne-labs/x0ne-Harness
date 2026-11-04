//! End-to-end demo tests — verbose, visible with `cargo test -- --nocapture`.
//!
//! These don't need a real VM. They prove the wire-format chain works:
//!
//! - **`screenshot_round_trip_ppm_to_png`** — feed a hand-built PPM (P6) image
//!   into the encoder, get a real PNG back, verify dimensions & PNG magic.
//!
//! - **`vnc_session_records_a_full_agent_loop`** — spin up a fake VNC server
//!   in a tokio task, run an `x0ne_vm::VncInputClient` against it, record every
//!   RFB message bytes-wire, then print a human-readable replay of the
//!   "agent loop": move → click → type "hi" → ctrl+alt+t.
//!
//! Run with:
//! ```
//! cargo test -p x0ne-vm --test integration_demo -- --nocapture
//! ```

use std::io::Cursor;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use x0ne_vm::session::MouseButton;
use x0ne_vm::vnc::VncInputClient;

// ─────────────────────────────────────────────────────────────────────────────
// Test 1 — screenshot encoder: PPM in, PNG out.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn screenshot_round_trip_ppm_to_png() {
    // Build a 4x3 RGB image in PPM P6 format.
    // Pattern: vertical bands red / green / blue / white.
    let mut ppm = Vec::new();
    ppm.extend_from_slice(b"P6\n4 3\n255\n");
    for _y in 0..3 {
        ppm.extend_from_slice(&[255, 0, 0]); // R
        ppm.extend_from_slice(&[0, 255, 0]); // G
        ppm.extend_from_slice(&[0, 0, 255]); // B
        ppm.extend_from_slice(&[255, 255, 255]); // W
    }

    // Decode via the same image crate the runtime uses, then re-encode as PNG.
    let img = image::ImageReader::new(Cursor::new(&ppm))
        .with_guessed_format()
        .unwrap()
        .decode()
        .unwrap();
    assert_eq!(img.width(), 4);
    assert_eq!(img.height(), 3);

    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();

    // PNG magic = 89 50 4E 47 0D 0A 1A 0A
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "missing PNG magic");

    println!(
        "✅ PPM→PNG OK — 4x3 image, {} bytes in, {} bytes out",
        ppm.len(),
        png.len()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2 — full agent loop replay against a fake VNC server.
// ─────────────────────────────────────────────────────────────────────────────

/// Recorded message after the handshake; we humanize it below.
#[derive(Debug)]
enum RecordedMsg {
    Pointer { mask: u8, x: u16, y: u16 },
    Key { down: bool, keysym: u32 },
}

#[tokio::test]
async fn vnc_session_records_a_full_agent_loop() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Channel for the server task to ship every recorded message back.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RecordedMsg>();

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();

        // Handshake (mirror of vnc::tests::handshake_succeeds_with_no_auth).
        s.write_all(b"RFB 003.008\n").await.unwrap();
        let mut buf12 = [0u8; 12];
        s.read_exact(&mut buf12).await.unwrap();
        s.write_all(&[1, 1]).await.unwrap();
        let mut b1 = [0u8; 1];
        s.read_exact(&mut b1).await.unwrap();
        s.write_all(&0u32.to_be_bytes()).await.unwrap();
        s.read_exact(&mut b1).await.unwrap();

        // ServerInit — 1024x768.
        s.write_all(&1024u16.to_be_bytes()).await.unwrap();
        s.write_all(&768u16.to_be_bytes()).await.unwrap();
        s.write_all(&[0u8; 16]).await.unwrap();
        let name = b"x0ne-vm fake VM";
        s.write_all(&(name.len() as u32).to_be_bytes())
            .await
            .unwrap();
        s.write_all(name).await.unwrap();

        // Now read input messages forever, ship to channel, until client drops.
        loop {
            let mut hdr = [0u8; 1];
            if s.read_exact(&mut hdr).await.is_err() {
                break;
            }
            match hdr[0] {
                4 => {
                    let mut rest = [0u8; 7];
                    if s.read_exact(&mut rest).await.is_err() {
                        break;
                    }
                    let down = rest[0] == 1;
                    let keysym = u32::from_be_bytes([rest[3], rest[4], rest[5], rest[6]]);
                    let _ = tx.send(RecordedMsg::Key { down, keysym });
                }
                5 => {
                    let mut rest = [0u8; 5];
                    if s.read_exact(&mut rest).await.is_err() {
                        break;
                    }
                    let mask = rest[0];
                    let x = u16::from_be_bytes([rest[1], rest[2]]);
                    let y = u16::from_be_bytes([rest[3], rest[4]]);
                    let _ = tx.send(RecordedMsg::Pointer { mask, x, y });
                }
                _ => break,
            }
        }
    });

    // ── Drive the client through a realistic agent loop. ──
    let mut client = VncInputClient::connect(&addr.to_string()).await.unwrap();
    assert_eq!(client.framebuffer.width, 1024);
    assert_eq!(client.framebuffer.height, 768);

    // 1. Move cursor.
    client.pointer_move(512, 384).await.unwrap();
    // 2. Left-click.
    client
        .pointer_click(512, 384, MouseButton::Left.mask())
        .await
        .unwrap();
    // 3. Type "hi".
    client.key_tap('h' as u32).await.unwrap();
    client.key_tap('i' as u32).await.unwrap();
    // 4. Ctrl+Alt+T (open terminal).
    client
        .key_chord(&[0xffe3, 0xffe9], 't' as u32)
        .await
        .unwrap();

    // Drop the client to close the TCP connection, then await the server.
    drop(client);
    server.await.unwrap();

    // Collect everything the server recorded.
    let mut events = Vec::new();
    while let Ok(m) = rx.try_recv() {
        events.push(m);
    }

    println!("\n🎬 Agent loop replay against fake VM (1024x768):");
    println!("   --- recorded RFB messages ---");
    for (i, e) in events.iter().enumerate() {
        match e {
            RecordedMsg::Pointer { mask, x, y } => {
                let buttons = describe_buttons(*mask);
                println!("   [{i:02}] mouse  ({x:>4},{y:>4})  buttons={buttons}");
            }
            RecordedMsg::Key { down, keysym } => {
                let label = describe_keysym(*keysym);
                let arrow = if *down { "▼" } else { "▲" };
                println!("   [{i:02}] key    {arrow} {label:<10} (0x{keysym:04x})");
            }
        }
    }
    println!("   --- {} events total ---\n", events.len());

    // Sanity assertions on the timeline.
    // Explicit pointer_move(512,384) → 1
    // pointer_click = move + button-down + button-up → 3 more pointer events
    // h↓ h↑ i↓ i↑ → 4 key events
    // chord ctrl↓ alt↓ t↓ t↑ alt↑ ctrl↑ → 6 key events
    // Total: 4 pointer + 10 keys = 14 events.
    let pointers = events
        .iter()
        .filter(|e| matches!(e, RecordedMsg::Pointer { .. }))
        .count();
    let keys = events
        .iter()
        .filter(|e| matches!(e, RecordedMsg::Key { .. }))
        .count();
    assert_eq!(
        pointers, 4,
        "expected 4 pointer events (explicit move + click's move + down + up)"
    );
    assert_eq!(keys, 10, "expected 10 key events (h↓↑ i↓↑ + chord 6)");

    // First key must be 'h' down, last must be Ctrl up.
    assert!(
        matches!(
            events.iter().find(|e| matches!(e, RecordedMsg::Key { .. })),
            Some(RecordedMsg::Key {
                down: true,
                keysym: 0x68
            })
        ),
        "first key must be 'h' down"
    );
    assert!(
        matches!(
            events.last(),
            Some(RecordedMsg::Key {
                down: false,
                keysym: 0xffe3
            })
        ),
        "last key must be Ctrl_L up"
    );
}

fn describe_buttons(mask: u8) -> String {
    if mask == 0 {
        return "—".into();
    }
    let mut s = String::new();
    if mask & 1 != 0 {
        s.push('L');
    }
    if mask & 2 != 0 {
        s.push('M');
    }
    if mask & 4 != 0 {
        s.push('R');
    }
    s
}

fn describe_keysym(k: u32) -> String {
    match k {
        0xff08 => "BackSpace".into(),
        0xff09 => "Tab".into(),
        0xff0d => "Return".into(),
        0xff1b => "Escape".into(),
        0xffe1 => "Shift_L".into(),
        0xffe3 => "Ctrl_L".into(),
        0xffe9 => "Alt_L".into(),
        0xffeb => "Super_L".into(),
        k if (0x20..=0x7e).contains(&k) => format!("'{}'", char::from_u32(k).unwrap()),
        _ => format!("U+{k:04x}"),
    }
}
