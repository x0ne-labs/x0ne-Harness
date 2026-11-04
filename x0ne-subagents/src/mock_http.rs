//! Minimal HTTP/1.1 server on 127.0.0.1 for tests and the offline demo.
//! One request per connection, `Connection: close`. Records every request
//! (also the absolute-form target a client sends to a forward proxy).

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    /// Request target as sent: `/v1/messages`, or `http://host/v1/messages` via a proxy.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }
}

type Handler = dyn Fn(&Recorded) -> (u16, String) + Send + Sync;

pub struct MockServer {
    pub url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    task: JoinHandle<()>,
}

impl MockServer {
    pub async fn start<F>(handler: F) -> std::io::Result<MockServer>
    where
        F: Fn(&Recorded) -> (u16, String) + Send + Sync + 'static,
    {
        Self::start_on("127.0.0.1:0", handler).await
    }

    pub async fn start_on<F>(addr: &str, handler: F) -> std::io::Result<MockServer>
    where
        F: Fn(&Recorded) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind(addr).await?;
        let url = format!("http://{}", listener.local_addr()?);
        let requests: Arc<Mutex<Vec<Recorded>>> = Arc::default();
        let handler: Arc<Handler> = Arc::new(handler);
        let reqs = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let h = Arc::clone(&handler);
                let r = Arc::clone(&reqs);
                tokio::spawn(async move {
                    let _ = serve(sock, h, r).await;
                });
            }
        });
        Ok(MockServer {
            url,
            requests,
            task,
        })
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("mock lock").clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(
    mut sock: tokio::net::TcpStream,
    handler: Arc<Handler>,
    reqs: Arc<Mutex<Vec<Recorded>>>,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if buf.len() > 1 << 20 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let method = first.next().unwrap_or("").to_string();
    let target = first.next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let rec = Recorded {
        method,
        target,
        headers,
        body,
    };
    let (status, out) = handler(&rec);
    reqs.lock().expect("mock lock").push(rec);
    let resp = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}",
        out.len()
    );
    sock.write_all(resp.as_bytes()).await?;
    sock.shutdown().await
}
