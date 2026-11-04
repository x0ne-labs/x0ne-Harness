//! Every request the page makes passes the allowlist, not only the tab's
//! URL (audit of 2026-10-30, H4-1). Before the fix, the allowlist knew the
//! address bar: an allowed page reached any host through an image, a
//! frame, a script, `fetch`, a beacon, `window.open` and a `target=_blank`
//! link, with `--disable-popup-blocking` on the command line.
//!
//! Needs a Chromium: `CHROME=<binary>` or one on the PATH. Without one the
//! test prints `SKIP` and passes, unless `X0NE_REQUIRE_CHROMIUM=1`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use x0ne_browser::chromium::ChromiumDriver;
use x0ne_browser::session::BrowserConfig;

type Hits = Arc<Mutex<Vec<String>>>;

fn chromium_present() -> bool {
    if let Ok(p) = std::env::var("CHROME") {
        return std::path::Path::new(&p).is_file();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|dir| {
        ["chromium", "chromium-browser", "google-chrome", "chrome"]
            .iter()
            .any(|name| dir.join(name).is_file())
    })
}

/// A one-connection-at-a-time HTTP/1.1 server that records every path it
/// is asked for and answers `respond(path)`.
async fn serve(
    hits: Hits,
    respond: fn(&str, u16) -> (&'static str, String),
    other_port: u16,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let hits = hits.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                let mut n = 0;
                loop {
                    let Ok(r) = sock.read(&mut buf[n..]).await else {
                        return;
                    };
                    if r == 0 {
                        return;
                    }
                    n += r;
                    if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") || n == buf.len() {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                hits.lock().unwrap().push(path.clone());
                let (ctype, body) = respond(&path, other_port);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    addr
}

fn allowed_page(path: &str, outside: u16) -> (&'static str, String) {
    match path {
        "/own.json" => ("application/json", "own-ok".into()),
        "/own.js" => (
            "text/javascript",
            "document.getElementById('own-js').textContent='own-js-ok';".into(),
        ),
        _ => (
            "text/html; charset=utf-8",
            format!(
                r#"<!doctype html><html><body>
<img src="http://127.0.0.1:{outside}/img">
<iframe src="http://127.0.0.1:{outside}/frame"></iframe>
<script src="http://127.0.0.1:{outside}/script.js"></script>
<a id="pop" href="http://127.0.0.1:{outside}/popup-click" target="_blank"
   style="display:block;width:300px;height:60px;background:#ccc">open</a>
<span id="own-status">pending</span> <span id="own-js">pending</span>
<script>
  fetch("http://127.0.0.1:{outside}/fetch", {{mode: "no-cors"}}).catch(() => {{}});
  navigator.sendBeacon("http://127.0.0.1:{outside}/beacon", "x");
  window.open("http://127.0.0.1:{outside}/popup-script");
  fetch("/own.json").then(r => r.text())
    .then(t => {{ document.getElementById("own-status").textContent = t; }})
    .catch(() => {{ document.getElementById("own-status").textContent = "failed"; }});
</script>
<script src="/own.js"></script>
</body></html>"#
            ),
        ),
    }
}

fn outside_page(path: &str, _: u16) -> (&'static str, String) {
    match path {
        "/script.js" => ("text/javascript", "window.outsideScriptRan = true;".into()),
        "/img" => ("image/png", "not really a png".into()),
        _ => ("text/html; charset=utf-8", "<h1>outside</h1>".into()),
    }
}

/// A driver call with a ceiling: a page whose popup took the foreground
/// answered nothing for the whole chromiumoxide command timeout, three
/// times over, before the fix. `None` is "no answer".
async fn step(
    name: &str,
    fut: impl std::future::Future<Output = x0ne_browser::error::BrowserResult<serde_json::Value>>,
) -> Option<x0ne_browser::error::BrowserResult<serde_json::Value>> {
    let t0 = std::time::Instant::now();
    match tokio::time::timeout(Duration::from_secs(25), fut).await {
        Ok(Ok(v)) => {
            eprintln!("{name}: ok in {:?}: {v}", t0.elapsed());
            Some(Ok(v))
        }
        Ok(Err(e)) => {
            eprintln!(
                "{name}: {} in {:?}: {}",
                e.code.as_str(),
                t0.elapsed(),
                e.message
            );
            Some(Err(e))
        }
        Err(_) => {
            eprintln!("{name}: no answer in {:?}", t0.elapsed());
            None
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_allowed_page_reaches_nothing_outside_the_allowlist() {
    if !chromium_present() {
        assert!(
            std::env::var("X0NE_REQUIRE_CHROMIUM").is_err(),
            "X0NE_REQUIRE_CHROMIUM is set and no Chromium was found"
        );
        eprintln!("SKIP: no Chromium (set CHROME=<binary>)");
        return;
    }
    tokio::time::timeout(Duration::from_secs(150), run())
        .await
        .expect("the test hung");
}

async fn run() {
    let outside_hits: Hits = Arc::default();
    let allowed_hits: Hits = Arc::default();
    let outside = serve(outside_hits.clone(), outside_page, 0).await;
    let allowed = serve(allowed_hits.clone(), allowed_page, outside.port()).await;

    let scratch = std::env::temp_dir().join(format!(
        "x0ne-browser-h41-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&scratch).unwrap();
    let config = BrowserConfig {
        mode: "headless".into(),
        profile_root: Some(scratch.join("profile")),
        allowlist: vec![
            format!("http://127.0.0.1:{}", allowed.port()),
            "about:*".into(),
        ],
        kill_switch_path: Some(scratch.join("kill")),
        default_timeout_ms: 20_000,
        ..BrowserConfig::default()
    };
    let driver = ChromiumDriver::new(config);
    let page_url = format!("http://127.0.0.1:{}/", allowed.port());

    let landed = step("navigate", driver.navigate(&page_url))
        .await
        .expect("navigate answered")
        .expect("the allowed page loads");
    assert_eq!(landed["url"], page_url);

    // Let the subresources, the fetches and the script popup settle.
    tokio::time::sleep(Duration::from_millis(2000)).await;

    // A click is a user gesture: the popup blocker lets `target=_blank` through.
    let clicked = step("click", driver.click("#pop")).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let hits = outside_hits.lock().unwrap().clone();
    let own = step("read own-status", driver.read_text("#own-status")).await;
    let own_js = step("read own-js", driver.read_text("#own-js")).await;
    let _ = step("close", driver.close()).await;
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        hits.is_empty(),
        "an allowed page reached a host outside the allowlist: {hits:?}"
    );

    // The page's own traffic flows and the page answers: the allowlisted
    // origin is not blocked, and nothing took the foreground from it.
    assert!(
        matches!(clicked, Some(Ok(_))),
        "the click on the link did not answer ok"
    );
    let own = own.expect("own-status answered").expect("own-status read");
    assert_eq!(own["text"], "own-ok", "same-origin fetch must still work");
    let own_js = own_js.expect("own-js answered").expect("own-js read");
    assert_eq!(
        own_js["text"], "own-js-ok",
        "same-origin script must still run"
    );
    let a = allowed_hits.lock().unwrap();
    assert!(
        a.iter().any(|p| p == "/own.json"),
        "allowed server hits: {a:?}"
    );
    assert!(
        a.iter().any(|p| p == "/own.js"),
        "allowed server hits: {a:?}"
    );
}
