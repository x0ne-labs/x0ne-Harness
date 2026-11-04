//! The runtime's artifact server (`artifact_serve`, default port 7900) answers
//! a browser on this machine and the orchestrator's router, nothing else.
//!
//! Audit of 2026-10-30, H3-1: the router on 7800 got its guard; the server
//! behind it had none. Reached directly (the URLs it prints on
//! `/_api/artifacts` name its own port), it served an artifact under any
//! `Host`, so a page whose name resolves to 127.0.0.1 read it, and relayed
//! any method and body to a service artifact.
//!
//! Through the real server on an ephemeral port (`X0NE_RUNTIME_SERVE_PORT=0`)
//! and raw HTTP/1.1, so `Host` and the browser headers are exactly what each
//! request says.

use std::net::SocketAddr;
use std::time::Duration;

/// One request on a fresh connection: `(status, lower-cased headers, body)`.
async fn raw(addr: SocketAddr, lines: &str) -> (u16, String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("{lines}Connection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf))
        .await
        .expect("the server answers")
        .unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    (status, head.to_ascii_lowercase(), body.to_string())
}

#[tokio::test]
async fn the_server_answers_this_machine_only() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("X0NE_HOME", home.path());
    std::env::set_var("X0NE_RUNTIME_SERVE_PORT", "0");
    let root = home.path().join("guarded");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("index.html"), b"<h1>guarded</h1>").unwrap();
    let now = chrono::Utc::now();
    x0ne_artifact::with_registry(|reg| {
        reg.artifacts.insert(
            "cv_guard".into(),
            x0ne_artifact::Artifact {
                id: "cv_guard".into(),
                name: None,
                kind: x0ne_artifact::ArtifactKind::Static,
                state: x0ne_artifact::ArtifactState::Created,
                persistent: false,
                root_path: root.clone(),
                session_id: None,
                created_at: now,
                updated_at: now,
                service: None,
            },
        );
    })
    .unwrap();
    let addr = x0ne_runtime::artifact_serve::spawn().await.unwrap();
    let port = addr.port();

    // DNS rebinding: the browser sends the attacker's Host.
    let (st, _, body) = raw(addr, "GET /a/cv_guard/ HTTP/1.1\r\nHost: evil.example\r\n").await;
    assert_eq!(st, 403, "{body}");
    assert!(
        !body.contains("guarded"),
        "served under a foreign Host: {body}"
    );
    let (st, _, body) = raw(
        addr,
        &format!("GET /a/cv_guard/ HTTP/1.1\r\nHost: evil.example:{port}\r\n"),
    )
    .await;
    assert_eq!(st, 403, "{body}");
    // A fetch from a remote page: Host right, Origin remote.
    let (st, _, body) = raw(
        addr,
        &format!("GET /a/cv_guard/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example\r\n"),
    )
    .await;
    assert_eq!(st, 403, "{body}");
    // A remote page embedding the artifact: no Origin, cross-site, not a navigation.
    let (st, _, body) = raw(
        addr,
        &format!("GET /a/cv_guard/ HTTP/1.1\r\nHost: localhost:{port}\r\nSec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: no-cors\r\nSec-Fetch-Dest: image\r\n"),
    )
    .await;
    assert_eq!(st, 403, "{body}");
    // Methods beyond a read.
    for method in ["POST", "PUT", "DELETE", "OPTIONS"] {
        let (st, _, body) = raw(
            addr,
            &format!("{method} /a/cv_guard/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 2\r\n\r\nhi"),
        )
        .await;
        assert_eq!(st, 405, "{method}: {body}");
    }
    // The inventory is not for whoever asks: the ids are the URLs.
    let (st, _, body) = raw(
        addr,
        &format!("GET /_api/artifacts HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    assert!(
        !body.contains("cv_guard"),
        "the listing names the artifacts: {body}"
    );

    // This machine's browser: typed URL, a link followed, the page's own
    // subresources; and the orchestrator's router across a container bridge,
    // which addresses the server by IP. Served, with the protective headers.
    for (host, extra) in [
        (format!("localhost:{port}"), ""),
        (
            format!("127.0.0.1:{port}"),
            "Sec-Fetch-Site: none\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\n",
        ),
        (
            format!("localhost:{port}"),
            "Sec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\n",
        ),
        (
            format!("localhost:{port}"),
            "Sec-Fetch-Site: same-origin\r\nSec-Fetch-Mode: cors\r\nSec-Fetch-Dest: script\r\n",
        ),
        (format!("198.51.100.5:{port}"), ""),
        (format!("[fd00::5]:{port}"), ""),
    ] {
        let (st, head, body) = raw(
            addr,
            &format!("GET /a/cv_guard/ HTTP/1.1\r\nHost: {host}\r\n{extra}"),
        )
        .await;
        assert_eq!(st, 200, "{host} {extra:?}: {body}");
        assert!(body.contains("guarded"), "{host} {extra:?}: {body}");
        for h in [
            "x-content-type-options: nosniff",
            "referrer-policy: no-referrer",
            "cross-origin-resource-policy: same-origin",
            "content-security-policy: frame-ancestors",
            "cache-control: no-store",
        ] {
            assert!(head.contains(h), "{host} {extra:?}: missing {h} in {head}");
        }
    }
    // An IP with another port is another server.
    let (st, _, body) = raw(
        addr,
        "GET /a/cv_guard/ HTTP/1.1\r\nHost: 198.51.100.5:1\r\n",
    )
    .await;
    assert_eq!(st, 403, "{body}");
    let (st, _, body) = raw(
        addr,
        &format!("HEAD /a/cv_guard/ HTTP/1.1\r\nHost: [::1]:{port}\r\n"),
    )
    .await;
    assert_eq!(st, 200, "{body}");
}
