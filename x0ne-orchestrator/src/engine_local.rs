//! Provider `x0ne-engine`: the loopback relay of `x0ne-engine cloud up`.
//! Before every call: key from its file, port held by this user, status verified
//! and serving. Tools never offered; no fallback to port 8477 or elsewhere.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::api::{ApiResponse, LlmBackend, OpenAICompatClient, StreamChunk};
use crate::config::X0neEngineConfig;
use crate::conversation::Message;

/// Where `x0ne-engine cloud up` serves its relay.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8478/v1";
/// Refused even on loopback: the default port of a server without attestation.
pub const UNATTESTED_PORT: u16 = 8477;
/// What every refusal tells the user to do.
pub const START_HINT: &str = "Start a session: x0ne-engine cloud up";
/// The relay's own route: state, verdict, end of the session.
pub const STATUS_PATH: &str = "/x0ne/v1/status";
const MAX_KEY_BYTES: u64 = 4096;
const MAX_STATUS_BYTES: usize = 64 * 1024;

/// `provider` names this backend. Both spellings, so that neither reaches
/// the Anthropic arm of `api::create_backend`.
pub fn is_engine_provider(provider: &str) -> bool {
    matches!(provider, "x0ne-engine" | "x0ne_engine")
}

fn refusal(reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("x0ne-engine: {reason}. {START_HINT}. No prompt was sent.")
}

/// The relay's answer to `GET /x0ne/v1/status`. Unknown fields are ignored;
/// a missing `state` or `verified` refuses.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RelayStatus {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub verified: Option<bool>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub expires_at_unix: Option<i64>,
    /// Read only when `expires_at_unix` is absent.
    #[serde(default)]
    pub seconds_left: Option<i64>,
}

impl RelayStatus {
    fn serving(&self) -> bool {
        self.verified == Some(true) && self.state.as_deref() == Some("serving")
    }

    /// `Ok` for a verified relay that serves, before the session's end.
    pub fn require_serving(&self, now_unix: i64) -> Result<()> {
        if !self.serving() {
            return Err(refusal(format_args!(
                "the relay is not verified and serving (state: {}, verified: {})",
                self.state.as_deref().unwrap_or("missing"),
                self.verified
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "missing".into())
            )));
        }
        match self.expires_at_unix {
            Some(end) if now_unix >= end => Err(refusal(format_args!(
                "the session ended at {} UTC",
                hhmm(end)
            ))),
            _ => Ok(()),
        }
    }

    /// `verified · 52 min left (until 22:05 UTC)`.
    pub fn summary(&self, now_unix: i64) -> String {
        let verdict = if self.serving() {
            "verified".to_string()
        } else {
            format!(
                "NOT verified (state {})",
                self.state.as_deref().unwrap_or("missing")
            )
        };
        match self.expires_at_unix {
            Some(end) if end > now_unix => format!(
                "{verdict} · {} min left (until {} UTC)",
                (end - now_unix) / 60,
                hhmm(end)
            ),
            Some(end) => format!("{verdict} · ended at {} UTC", hhmm(end)),
            None => format!("{verdict} · end time not given"),
        }
    }
}

fn hhmm(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|t| t.format("%H:%M").to_string())
        .unwrap_or_else(|| "?".into())
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

fn current_euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// `http://127.0.0.1:<port>` or `http://[::1]:<port>`, with no path or `/v1`.
/// A name (`localhost`) is refused: it is resolved, an address is not.
pub fn parse_base_url(url: &str) -> Result<(String, IpAddr, u16)> {
    let shown = crate::redact::url_for_display(url);
    let bad = |why: &str| anyhow!("[llm.x0ne_engine] base_url {shown:?}: {why}");
    let u = reqwest::Url::parse(url.trim()).map_err(|e| bad(&e.to_string()))?;
    if u.scheme() != "http" {
        return Err(bad("the relay is plain http:// on loopback"));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(bad("no user or password in the URL"));
    }
    if u.query().is_some() || u.fragment().is_some() {
        return Err(bad("no query or fragment"));
    }
    let host = u.host_str().unwrap_or_default();
    let ip: IpAddr = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .map_err(|_| bad("the host must be 127.0.0.1 or [::1]"))?;
    if ip != IpAddr::V4(Ipv4Addr::LOCALHOST) && ip != IpAddr::V6(Ipv6Addr::LOCALHOST) {
        return Err(bad("the host must be 127.0.0.1 or [::1]"));
    }
    let port = u.port_or_known_default().unwrap_or(80);
    if port == UNATTESTED_PORT {
        return Err(bad(
            "port 8477 is the default of a server without attestation, which is not \
             confidential; the relay of `x0ne-engine cloud up` listens on 8478",
        ));
    }
    if !matches!(u.path(), "" | "/" | "/v1" | "/v1/") {
        return Err(bad("the path must be /v1"));
    }
    let root = match ip {
        IpAddr::V4(a) => format!("http://{a}:{port}"),
        IpAddr::V6(a) => format!("http://[{a}]:{port}"),
    };
    Ok((root, ip, port))
}

fn runtime_dir(xdg: Option<&str>, euid: u32) -> PathBuf {
    xdg.filter(|d| d.starts_with('/'))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{euid}")))
}

/// `key_file` as configured, or `$XDG_RUNTIME_DIR/x0ne-engine/local.key`
/// (`/run/user/<euid>` when the variable is unset). Only the PATH of the key
/// comes from the environment, as the relay writes it there.
pub fn resolve_key_file(
    configured: Option<&str>,
    xdg: Option<&str>,
    home: Option<&Path>,
    euid: u32,
) -> Result<PathBuf> {
    let Some(p) = configured.map(str::trim).filter(|p| !p.is_empty()) else {
        return Ok(runtime_dir(xdg, euid).join("x0ne-engine").join("local.key"));
    };
    if let Some(rest) = p.strip_prefix("$XDG_RUNTIME_DIR/") {
        return Ok(runtime_dir(xdg, euid).join(rest));
    }
    if let Some(rest) = p.strip_prefix("~/") {
        let home = home.ok_or_else(|| anyhow!("[llm.x0ne_engine] key_file {p:?}: no home"))?;
        return Ok(home.join(rest));
    }
    let path = PathBuf::from(p);
    if !path.is_absolute() {
        bail!("[llm.x0ne_engine] key_file {p:?} must be an absolute path");
    }
    Ok(path)
}

/// The Bearer key, re-read at every call: a regular file this user owns and
/// nobody else can read or write, one printable token. Never logged.
pub fn read_key(path: &Path, euid: u32) -> Result<String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let shown = path.display();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| match (e.kind(), e.raw_os_error()) {
            (std::io::ErrorKind::NotFound, _) => refusal(format_args!("no session key at {shown}")),
            (_, Some(libc::ELOOP)) => refusal(format_args!(
                "the key {shown} is a symbolic link; the relay writes a regular file"
            )),
            _ => refusal(format_args!("cannot open the key {shown}: {e}")),
        })?;
    let meta = file
        .metadata()
        .map_err(|e| refusal(format_args!("cannot stat the key {shown}: {e}")))?;
    if !meta.is_file() {
        return Err(refusal(format_args!(
            "the key {shown} is not a regular file"
        )));
    }
    if meta.uid() != euid {
        return Err(refusal(format_args!(
            "the key {shown} belongs to uid {}, not to you (uid {euid})",
            meta.uid()
        )));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(refusal(format_args!(
            "the key {shown} is readable or writable by others (mode {:o}); the relay writes it 0600",
            meta.mode() & 0o7777
        )));
    }
    let mut text = String::new();
    file.take(MAX_KEY_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| refusal(format_args!("the key {shown} is not text")))?;
    let key = text.trim();
    let token = |b: u8| (0x21..=0x7e).contains(&b);
    if text.len() as u64 > MAX_KEY_BYTES || key.is_empty() || !key.bytes().all(token) {
        return Err(refusal(format_args!("{shown} does not hold one key")));
    }
    Ok(key.to_string())
}

/// `/proc/net/tcp` writes an IPv4 address as the `%08X` of its in-memory u32.
fn proc_hex4(a: Ipv4Addr) -> String {
    format!("{:08X}", u32::from_ne_bytes(a.octets()))
}

/// `/proc/net/tcp6`: four in-memory u32, each `%08X`.
fn proc_hex6(a: Ipv6Addr) -> String {
    a.octets()
        .chunks(4)
        .map(|w| format!("{:08X}", u32::from_ne_bytes([w[0], w[1], w[2], w[3]])))
        .collect()
}

/// Uids of the LISTEN sockets that could take a connection to `ip:port`:
/// the address itself, the wildcard, and for IPv4 the v4-mapped forms.
fn listeners(ip: IpAddr, port: u16, tcp: &str, tcp6: &str) -> Vec<u32> {
    let any4 = proc_hex4(Ipv4Addr::UNSPECIFIED);
    let any6 = proc_hex6(Ipv6Addr::UNSPECIFIED);
    let (v4, v6): (Vec<String>, Vec<String>) = match ip {
        IpAddr::V4(a) => (
            vec![proc_hex4(a), any4],
            vec![proc_hex6(a.to_ipv6_mapped()), any6],
        ),
        IpAddr::V6(a) => (vec![], vec![proc_hex6(a), any6]),
    };
    let port = format!("{port:04X}");
    let scan = |table: &str, addrs: &[String]| -> Vec<u32> {
        table
            .lines()
            .skip(1)
            .filter_map(|line| {
                let f: Vec<&str> = line.split_whitespace().collect();
                let (addr, p) = f.get(1)?.split_once(':')?;
                let listening = f.get(3) == Some(&"0A");
                (listening && p == port && addrs.iter().any(|a| a == addr))
                    .then(|| f.get(7)?.parse().ok())?
            })
            .collect()
    };
    let mut uids = scan(tcp, &v4);
    uids.extend(scan(tcp6, &v6));
    uids
}

/// Another local user must not be able to squat the port and receive the key
/// and the prompts: every listener that could answer must be this user's.
fn check_listener_owner(ip: IpAddr, port: u16, euid: u32) -> Result<()> {
    let tcp = std::fs::read_to_string("/proc/net/tcp");
    let tcp6 = std::fs::read_to_string("/proc/net/tcp6");
    if tcp.is_err() && tcp6.is_err() {
        return Err(refusal(format_args!(
            "cannot tell who listens on port {port}: /proc/net/tcp is unreadable"
        )));
    }
    let uids = listeners(
        ip,
        port,
        tcp.as_deref().unwrap_or(""),
        tcp6.as_deref().unwrap_or(""),
    );
    if uids.is_empty() {
        return Err(refusal(format_args!("nothing listens on {ip} port {port}")));
    }
    if let Some(other) = uids.iter().find(|u| **u != euid) {
        return Err(refusal(format_args!(
            "port {port} is held by uid {other}, not by you (uid {euid})"
        )));
    }
    Ok(())
}

/// The `x0ne-engine` backend. See the module documentation.
pub struct EngineBackend {
    root: String,
    ip: IpAddr,
    port: u16,
    key_file: PathBuf,
    model_pin: Option<String>,
    max_tokens: u32,
    euid: u32,
    /// No proxy, no redirect: a request for the loopback relay goes nowhere else.
    client: reqwest::Client,
    last: Mutex<Option<RelayStatus>>,
}

impl EngineBackend {
    /// Validates the config; touches neither the network nor the key.
    pub fn from_config(cfg: &X0neEngineConfig) -> Result<Self> {
        if !cfg.require_verified {
            bail!(
                "[llm.x0ne_engine] require_verified = false is refused: this provider \
                 only talks to a verified relay. For an unverified local server, use \
                 provider \"openai-compat\"."
            );
        }
        let (root, ip, port) = parse_base_url(&cfg.base_url)?;
        let euid = current_euid();
        let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
        let key_file = resolve_key_file(
            cfg.key_file.as_deref(),
            xdg.as_deref(),
            dirs::home_dir().as_deref(),
            euid,
        )?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(300))
            .build()
            .context("cannot build the x0ne-engine HTTP client")?;
        let model_pin = Some(cfg.model.trim())
            .filter(|m| !m.is_empty())
            .map(String::from);
        Ok(Self {
            root,
            ip,
            port,
            key_file,
            model_pin,
            max_tokens: cfg.max_tokens,
            euid,
            client,
            last: Mutex::new(None),
        })
    }

    /// The gate without a prompt, for startup. Returns the status summary.
    pub async fn preflight(&self) -> Result<String> {
        let (_key, status) = self.gate().await?;
        Ok(status.summary(now_unix()))
    }

    /// The model the next call asks for: the pin, else the relay's.
    pub fn model(&self) -> Option<String> {
        self.model_pin.clone().or_else(|| {
            self.last
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .and_then(|s| s.model.clone())
        })
    }

    /// Before any request that carries a prompt.
    async fn gate(&self) -> Result<(String, RelayStatus)> {
        let key = read_key(&self.key_file, self.euid)?;
        check_listener_owner(self.ip, self.port, self.euid)?;
        let status = self.fetch_status(&key).await?;
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(status.clone());
        status.require_serving(now_unix())?;
        Ok((key, status))
    }

    async fn fetch_status(&self, key: &str) -> Result<RelayStatus> {
        let url = format!("{}{STATUS_PATH}", self.root);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(key)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| refusal(format_args!("the relay does not answer at {url}: {e}")))?;
        let code = resp.status();
        if resp.content_length().unwrap_or(0) > MAX_STATUS_BYTES as u64 {
            return Err(refusal("the relay's status is too large"));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| refusal(format_args!("the relay's status could not be read: {e}")))?;
        if body.len() > MAX_STATUS_BYTES {
            return Err(refusal("the relay's status is too large"));
        }
        if !code.is_success() {
            let key_hint = if matches!(code.as_u16(), 401 | 403) {
                format!(" (the key in {} was refused)", self.key_file.display())
            } else {
                String::new()
            };
            return Err(refusal(format_args!(
                "the relay answered {code} to {STATUS_PATH}{key_hint}"
            )));
        }
        let mut status: RelayStatus = serde_json::from_slice(&body).map_err(|e| {
            refusal(format_args!(
                "the relay's status is not the expected JSON: {e}"
            ))
        })?;
        if status.expires_at_unix.is_none() {
            status.expires_at_unix = status.seconds_left.map(|s| now_unix().saturating_add(s));
        }
        Ok(status)
    }

    fn completions(&self, key: String, status: &RelayStatus) -> Result<OpenAICompatClient> {
        let model = self
            .model_pin
            .clone()
            .or_else(|| status.model.clone().filter(|m| !m.is_empty()))
            .ok_or_else(|| {
                refusal("the relay's status names no model; set [llm.x0ne_engine] model")
            })?;
        Ok(OpenAICompatClient::text_only(
            &self.root,
            model,
            self.max_tokens,
            Some(key),
            self.client.clone(),
        ))
    }
}

#[async_trait]
impl LlmBackend for EngineBackend {
    async fn send(&self, system: &str, messages: &[Message]) -> Result<ApiResponse> {
        let (key, status) = self.gate().await?;
        self.completions(key, &status)?
            .send(system, messages)
            .await
            .with_context(|| format!("x0ne-engine relay {}", self.root))
    }

    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tx: tokio::sync::mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<ApiResponse> {
        let (key, status) = self.gate().await?;
        self.completions(key, &status)?
            .stream(system, messages, tx)
            .await
            .with_context(|| format!("x0ne-engine relay {}", self.root))
    }

    /// Tools are never offered: the relay answers 400 to them.
    async fn stream_with_tools(
        &self,
        system: &str,
        messages: &[Message],
        _tools: &[Value],
        tx: tokio::sync::mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<ApiResponse> {
        self.stream(system, messages, tx).await
    }

    /// `x0ne-engine/<model> · <verdict> · <time left> @ <root>`: the verdict
    /// before ` @ `, so that `terminal::format_provider_model` keeps it.
    fn display_name(&self) -> String {
        let last = self.last.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let model = self.model().unwrap_or_else(|| "relay model".into());
        let verdict = last
            .map(|s| s.summary(now_unix()))
            .unwrap_or_else(|| "verdict checked before each call".into());
        format!("x0ne-engine/{model} · {verdict} @ {}", self.root)
    }
}

/// A loopback stand-in for the relay, shared with the quickstart tests.
#[cfg(test)]
pub(crate) mod test_relay {
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Debug, Clone)]
    pub struct Seen {
        pub path: String,
        pub auth: Option<String>,
        pub body: Value,
    }

    pub type Route = Arc<dyn Fn(&Seen) -> (u16, String) + Send + Sync>;

    pub struct Relay {
        pub root: String,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl Relay {
        pub fn all(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
        pub fn completions(&self) -> Vec<Seen> {
            self.all()
                .into_iter()
                .filter(|s| s.path == "/v1/chat/completions")
                .collect()
        }
    }

    pub async fn start(route: Route) -> Relay {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (log, route) = (log.clone(), route.clone());
                tokio::spawn(async move {
                    let Some(req) = read_request(&mut sock).await else {
                        return;
                    };
                    let (status, body) = route(&req);
                    log.lock().unwrap().push(req);
                    // A 3xx route's body is its Location.
                    let (location, body) = if (300..400).contains(&status) {
                        (format!("location: {body}\r\n"), String::new())
                    } else {
                        (String::new(), body)
                    };
                    let head = format!(
                        "HTTP/1.1 {status} X\r\n{location}content-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        Relay { root, seen }
    }

    async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<Seen> {
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            let n = sock.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..h]).to_string();
            let mut lines = head.lines();
            let path = lines.next()?.split_whitespace().nth(1)?.to_string();
            let (mut auth, mut len) = (None, 0usize);
            for line in lines {
                let (k, v) = line.split_once(':')?;
                match k.trim().to_ascii_lowercase().as_str() {
                    "authorization" => auth = Some(v.trim().to_string()),
                    "content-length" => len = v.trim().parse().ok()?,
                    _ => {}
                }
            }
            if buf.len() < h + 4 + len {
                continue;
            }
            let body = serde_json::from_slice(&buf[h + 4..h + 4 + len]).unwrap_or(Value::Null);
            return Some(Seen { path, auth, body });
        }
    }

    pub fn completion(text: &str) -> String {
        json!({"choices":[{"index":0,"finish_reason":"stop",
            "message":{"role":"assistant","content":text}}],
            "usage":{"prompt_tokens":3,"completion_tokens":5}})
        .to_string()
    }

    /// `status` on the status route; completions answer `reply`, or 400
    /// naming tools when the request carries `tools`, as the relay does.
    pub fn route(status: Value, reply: &'static str) -> Route {
        Arc::new(move |s: &Seen| match s.path.as_str() {
            super::STATUS_PATH => (200, status.to_string()),
            "/v1/chat/completions" if s.body.get("tools").is_some() => (
                400,
                json!({"error":{"message":"tools are not supported by this relay"}}).to_string(),
            ),
            "/v1/chat/completions" => (200, completion(reply)),
            _ => (404, json!({"error":{"message":"not found"}}).to_string()),
        })
    }

    pub fn verified(model: &str) -> Value {
        json!({"v":1,"state":"serving","verified":true,"model":model,
               "expires_at_unix": chrono::Utc::now().timestamp() + 3150})
    }

    /// `dir/local.key` holding `key` and a newline, mode 0600.
    pub fn key_file(dir: &Path, key: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("local.key");
        std::fs::write(&path, format!("{key}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }
}

#[cfg(test)]
mod tests {
    use super::test_relay::{self, Route};
    use super::*;
    use crate::conversation::Role;
    use serde_json::json;
    use std::sync::Arc;

    const KEY: &str = "k3y-from-the-file-0123456789abcdef";

    fn engine(root: &str, key_file: &Path) -> EngineBackend {
        EngineBackend::from_config(&X0neEngineConfig {
            base_url: format!("{root}/v1"),
            key_file: Some(key_file.display().to_string()),
            ..Default::default()
        })
        .unwrap()
    }

    fn hi() -> Vec<Message> {
        vec![Message {
            role: Role::User,
            content: "hi".into(),
            images: vec![],
        }]
    }

    #[tokio::test]
    async fn the_key_comes_from_the_file_and_every_call_is_gated() {
        let dir = tempfile::tempdir().unwrap();
        let key = test_relay::key_file(dir.path(), KEY);
        let relay = test_relay::start(test_relay::route(
            test_relay::verified("qwen3:0.6b"),
            "Hello.",
        ))
        .await;
        let e = engine(&relay.root, &key);
        for _ in 0..2 {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let tools = crate::native_tools::action_tools();
            let r = e.stream_with_tools("sys", &hi(), tools, tx).await.unwrap();
            assert_eq!(r.content, "Hello.");
        }
        assert_eq!(e.send("sys", &hi()).await.unwrap().content, "Hello.");

        let seen = relay.all();
        let paths: Vec<&str> = seen.iter().map(|s| s.path.as_str()).collect();
        let call = [STATUS_PATH, "/v1/chat/completions"];
        assert_eq!(
            paths,
            [call, call, call].concat(),
            "a status check before each call"
        );
        for s in &seen {
            assert_eq!(
                s.auth.as_deref(),
                Some(format!("Bearer {KEY}").as_str()),
                "{}",
                s.path
            );
        }
        for c in relay.completions() {
            assert!(c.body.get("tools").is_none(), "tools offered: {}", c.body);
            assert!(c.body.get("tool_choice").is_none(), "{}", c.body);
            assert_eq!(c.body["model"], "qwen3:0.6b", "the relay's model");
        }
        let shown = crate::terminal::format_provider_model(&e.display_name());
        assert!(
            shown.contains("qwen3:0.6b · verified · 52 min left (until "),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn a_relay_that_is_not_verified_and_serving_never_receives_a_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let key = test_relay::key_file(dir.path(), KEY);
        let past = chrono::Utc::now().timestamp() - 10;
        let cases: Vec<(u16, Value)> = vec![
            (200, json!({"state":"serving","verified":false,"model":"m"})),
            (
                200,
                json!({"state":"verifying","verified":true,"model":"m"}),
            ),
            (200, json!({"verified":true,"model":"m"})),
            (200, json!({"state":"serving","model":"m"})),
            (
                200,
                json!({"state":"serving","verified":"true","model":"m"}),
            ),
            (
                200,
                json!({"state":"serving","verified":true,"model":"m","expires_at_unix":past}),
            ),
            (200, json!("verified")),
            (503, json!({"error":{"message":"the VM is gone"}})),
            (401, json!({"error":{"message":"bad key"}})),
        ];
        for (code, status) in cases {
            let body = status.to_string();
            let route: Route = Arc::new(move |s| match s.path.as_str() {
                STATUS_PATH => (code, body.clone()),
                _ => (200, test_relay::completion("leaked")),
            });
            let relay = test_relay::start(route).await;
            let e = engine(&relay.root, &key);
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let streamed = e.stream("sys", &hi(), tx).await.map(|r| r.content);
            let sent = e.send("sys", &hi()).await.map(|r| r.content);
            for r in [streamed, sent] {
                let msg = format!("{:#}", r.expect_err("must be refused"));
                assert!(
                    msg.contains("x0ne-engine cloud up"),
                    "{code} {status}: {msg}"
                );
                assert!(msg.contains("No prompt was sent"), "{code} {status}: {msg}");
            }
            assert!(
                relay.completions().is_empty(),
                "{code} {status}: a prompt left"
            );
            assert!(
                e.display_name().contains("x0ne-engine/"),
                "{}",
                e.display_name()
            );
        }
    }

    #[tokio::test]
    async fn an_unreachable_relay_never_receives_a_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let key = test_relay::key_file(dir.path(), KEY);
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let e = engine(&format!("http://127.0.0.1:{port}"), &key);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let msg = format!("{:#}", e.stream("sys", &hi(), tx).await.unwrap_err());
        assert!(msg.contains("nothing listens"), "{msg}");
        assert!(msg.contains("x0ne-engine cloud up"), "{msg}");
        assert!(e.preflight().await.is_err());
    }

    #[tokio::test]
    async fn a_redirect_from_the_relay_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let key = test_relay::key_file(dir.path(), KEY);
        let elsewhere =
            test_relay::start(test_relay::route(test_relay::verified("m"), "leaked")).await;
        let target = format!("{}{STATUS_PATH}", elsewhere.root);
        let route: Route = Arc::new(move |_| (307, target.clone()));
        let relay = test_relay::start(route).await;
        let e = engine(&relay.root, &key);
        let msg = format!("{:#}", e.send("sys", &hi()).await.unwrap_err());
        assert!(
            msg.contains("307") && msg.contains("No prompt was sent"),
            "{msg}"
        );
        assert!(
            elsewhere.all().is_empty(),
            "followed: {:?}",
            elsewhere.all()
        );
    }

    #[tokio::test]
    async fn a_missing_or_loose_key_stops_everything_before_the_network() {
        use std::os::unix::fs::PermissionsExt;
        let relay = test_relay::start(test_relay::route(test_relay::verified("m"), "leaked")).await;
        let dir = tempfile::tempdir().unwrap();
        let good = test_relay::key_file(dir.path(), KEY);
        let loose = |mode: u32| {
            let p = dir.path().join(format!("loose-{mode:o}"));
            std::fs::write(&p, KEY).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            p
        };
        let link = dir.path().join("link.key");
        std::os::unix::fs::symlink(&good, &link).unwrap();
        let empty = dir.path().join("empty.key");
        std::fs::write(&empty, "\n").unwrap();
        std::fs::set_permissions(&empty, std::fs::Permissions::from_mode(0o600)).unwrap();
        let cases = [
            (dir.path().join("absent.key"), "no session key at"),
            (loose(0o640), "readable or writable by others"),
            (loose(0o604), "readable or writable by others"),
            (link, "symbolic link"),
            (empty, "does not hold one key"),
            (dir.path().to_path_buf(), "not a regular file"),
        ];
        for (path, why) in cases {
            let e = engine(&relay.root, &path);
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let msg = format!("{:#}", e.stream("sys", &hi(), tx).await.unwrap_err());
            assert!(
                msg.contains(why) && msg.contains("No prompt was sent"),
                "{msg}"
            );
        }
        assert!(
            relay.all().is_empty(),
            "a request left before the key was checked"
        );
    }

    /// Re-executed with decoy keys and a proxy in the environment: the Bearer
    /// is the file's, and the relay is reached directly.
    #[test]
    fn decoy_keys_and_proxies_in_the_environment_are_never_used() {
        const CHILD: &str = "X0NE_TEST_ENGINE_ENV_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let rt = tokio::runtime::Runtime::new().unwrap();
            return rt.block_on(decoy_child());
        }
        let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
        let lines = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let log = lines.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            for mut sock in proxy.incoming().flatten() {
                let mut line = String::new();
                let _ = std::io::BufReader::new(&sock).read_line(&mut line);
                log.lock().unwrap().push(line.trim().to_string());
                let _ = sock
                    .write_all(b"HTTP/1.1 502 X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            }
        });
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "engine_local::tests::decoy_keys_and_proxies_in_the_environment_are_never_used",
        ])
        .args(["--test-threads=1", "--nocapture"])
        .env(CHILD, "1")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy");
        for var in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            cmd.env(var, &proxy_url);
        }
        for var in [
            "X0NE_OPENAI_COMPAT_KEY",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "X0NE_ENGINE_KEY",
        ] {
            cmd.env(var, format!("decoy-{var}"));
        }
        let out = cmd.output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success() && text.contains("1 passed"), "{text}");
        let lines = lines.lock().unwrap().clone();
        assert!(
            lines.iter().any(|l| l.contains("/control")),
            "the proxy setting did not reach a default client: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("/x0ne/") || l.contains("/chat/")),
            "an engine request went through the proxy: {lines:?}"
        );
    }

    async fn decoy_child() {
        let dir = tempfile::tempdir().unwrap();
        let key = test_relay::key_file(dir.path(), KEY);
        let relay = test_relay::start(test_relay::route(test_relay::verified("m"), "ok")).await;
        let e = engine(&relay.root, &key);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        e.stream("sys", &hi(), tx).await.unwrap();
        let seen = relay.all();
        assert_eq!(seen.len(), 2, "status then completion, directly: {seen:?}");
        for s in seen {
            assert_eq!(s.auth.as_deref(), Some(format!("Bearer {KEY}").as_str()));
        }
        // Control: a default client does use the proxy from the environment.
        let _ = reqwest::Client::new()
            .get(format!("{}/control", relay.root))
            .timeout(Duration::from_secs(3))
            .send()
            .await;
    }

    #[test]
    fn the_base_url_must_be_the_loopback_relay() {
        for ok in [
            "http://127.0.0.1:8478/v1",
            "http://127.0.0.1:8478/v1/",
            "http://127.0.0.1:8478",
            "http://[::1]:8478/v1",
        ] {
            assert!(parse_base_url(ok).is_ok(), "{ok}");
        }
        assert_eq!(
            parse_base_url(DEFAULT_BASE_URL).unwrap().0,
            "http://127.0.0.1:8478"
        );
        for (bad, why) in [
            ("http://127.0.0.1:8477/v1", "without attestation"),
            ("http://localhost:8478/v1", "127.0.0.1 or [::1]"),
            ("http://127.0.0.1.example.com:8478/v1", "127.0.0.1 or [::1]"),
            ("http://10.0.0.1:8478/v1", "127.0.0.1 or [::1]"),
            ("http://0.0.0.0:8478/v1", "127.0.0.1 or [::1]"),
            ("https://127.0.0.1:8478/v1", "plain http"),
            ("http://user:pw@127.0.0.1:8478/v1", "no user"),
            ("http://127.0.0.1:8478/v2", "the path must be /v1"),
            ("http://127.0.0.1:8478/v1?x=1", "no query"),
        ] {
            let msg = format!("{:#}", parse_base_url(bad).unwrap_err());
            assert!(msg.contains(why), "{bad}: {msg}");
        }
        let msg = format!(
            "{:#}",
            parse_base_url("http://user:hunter2@127.0.0.1:8478/v1").unwrap_err()
        );
        assert!(
            !msg.contains("hunter2"),
            "the refusal echoes the password: {msg}"
        );
    }

    #[test]
    fn an_unverified_mode_and_an_inline_key_are_refused() {
        let cfg = X0neEngineConfig {
            require_verified: false,
            ..Default::default()
        };
        let msg = format!(
            "{:#}",
            EngineBackend::from_config(&cfg).err().expect("refused")
        );
        assert!(msg.contains("openai-compat"), "{msg}");
        let inline = "[llm]\nprovider = \"x0ne-engine\"\n[llm.x0ne_engine]\napi_key = \"k\"\n";
        let err = toml::from_str::<crate::config::Config>(inline).unwrap_err();
        assert!(err.to_string().contains("api_key"), "{err}");
    }

    #[test]
    fn both_spellings_build_this_backend_and_never_the_anthropic_one() {
        for provider in ["x0ne-engine", "x0ne_engine"] {
            let mut llm = crate::config::LlmConfig::default();
            llm.provider = provider.into();
            let b = crate::api::create_backend(&llm, Some("sk-ant-api03-decoy".into())).unwrap();
            assert!(
                b.display_name().starts_with("x0ne-engine/"),
                "{}",
                b.display_name()
            );
        }
    }

    #[test]
    fn the_key_path_defaults_to_the_runtime_dir() {
        let home = Path::new("/home/u");
        let at = |c: Option<&str>, x: Option<&str>| resolve_key_file(c, x, Some(home), 1000);
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(
            at(None, Some("/run/user/1000")).unwrap(),
            p("/run/user/1000/x0ne-engine/local.key")
        );
        assert_eq!(
            at(None, None).unwrap(),
            p("/run/user/1000/x0ne-engine/local.key")
        );
        assert_eq!(
            at(None, Some("relative")).unwrap(),
            p("/run/user/1000/x0ne-engine/local.key")
        );
        assert_eq!(
            at(Some("$XDG_RUNTIME_DIR/x0ne-engine/local.key"), Some("/xdg")).unwrap(),
            p("/xdg/x0ne-engine/local.key")
        );
        assert_eq!(at(Some("~/k"), None).unwrap(), p("/home/u/k"));
        assert_eq!(at(Some("/abs/k"), None).unwrap(), p("/abs/k"));
        assert!(at(Some("relative/k"), None).is_err());
    }

    /// Lines as Linux prints them on a little-endian machine.
    #[cfg(target_endian = "little")]
    #[test]
    fn listeners_are_read_from_proc_net_tcp() {
        let head = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
        let tcp = format!(
            "{head}\
   0: 0100007F:211E 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 11 1 0 100 0 0 10 0\n\
   1: 0100007F:211E 0100007F:9C40 01 00000000:00000000 00:00000000 00000000  1001        0 12 1 0 100 0 0 10 0\n\
   2: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1002        0 13 1 0 100 0 0 10 0\n"
        );
        let tcp6 = format!(
            "{head}\
   0: 00000000000000000000000001000000:211E 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1003        0 14 1 0 100 0 0 10 0\n\
   1: 0000000000000000FFFF00000100007F:2121 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1004        0 15 1 0 100 0 0 10 0\n"
        );
        let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert_eq!(
            listeners(v4, 8478, &tcp, &tcp6),
            vec![1000],
            "the established row is not a listener"
        );
        assert_eq!(
            listeners(v4, 8080, &tcp, &tcp6),
            vec![1002],
            "the wildcard takes loopback"
        );
        assert_eq!(listeners(v4, 8481, &tcp, &tcp6), vec![1004], "v4-mapped");
        assert_eq!(listeners(v6, 8478, &tcp, &tcp6), vec![1003]);
        assert!(listeners(v4, 9999, &tcp, &tcp6).is_empty());
    }

    #[test]
    fn the_status_summary_names_the_verdict_and_the_time_left() {
        let s = RelayStatus {
            state: Some("serving".into()),
            verified: Some(true),
            expires_at_unix: Some(1_760_303_100),
            ..Default::default()
        };
        assert_eq!(
            s.summary(1_760_303_100 - 3120),
            "verified · 52 min left (until 21:05 UTC)"
        );
        assert_eq!(s.summary(1_760_303_100), "verified · ended at 21:05 UTC");
        assert!(s.require_serving(1_760_303_100 - 1).is_ok());
        assert!(s.require_serving(1_760_303_100).is_err());
        let not = RelayStatus {
            state: Some("verifying".into()),
            ..Default::default()
        };
        assert_eq!(
            not.summary(0),
            "NOT verified (state verifying) · end time not given"
        );
    }

    struct NoWait;

    #[async_trait]
    impl crate::agent::model::Sleeper for NoWait {
        async fn sleep(&self, _d: Duration) {}
    }

    /// A relay that answers 400 to `tools`: the turn ends in the text protocol,
    /// and no request ever offered tools.
    #[tokio::test]
    async fn an_agent_turn_ends_in_text_protocol_against_a_relay_that_refuses_tools() {
        use crate::agent::driver::{AgentState, Driver, DriverLimits, TurnContext, TurnOutcome};
        let dir = tempfile::tempdir().unwrap();
        let key = test_relay::key_file(dir.path(), KEY);
        let relay = test_relay::start(test_relay::route(
            test_relay::verified("qwen3:0.6b"),
            "Six times seven is 42.",
        ))
        .await;
        let backend = engine(&relay.root, &key);
        let config = crate::config::Config::default();
        let mut state = AgentState::new(100_000, 0.0, 0.0);
        state.history.add_user("What is six times seven?");
        let cancel = crate::agent::cancel::CancelToken::new();
        let mut sink = crate::display_sink::NullDisplaySink;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = TurnContext {
            backend: &backend,
            config: &config,
            system_prompt: "sys",
            bridge: None,
            sysaicalls: None,
            spawn_factory: None,
            sink: &mut sink,
            cancel: &cancel,
            sleeper: &NoWait,
            prices: &crate::agent::pricing::NoPrices,
            limits: DriverLimits {
                turn_deadline: Duration::from_secs(30),
                ..Default::default()
            },
            chunks: tx,
            frontend: None,
            journal: None,
            goal: None,
        };
        let outcome = Driver::new().run_turn(&mut state, &mut ctx).await.outcome;
        match outcome {
            TurnOutcome::Completed { message } => assert!(message.contains("42"), "{message}"),
            other => panic!("the turn did not end on the answer: {other:?}"),
        }
        let completions = relay.completions();
        assert!(!completions.is_empty());
        for c in completions {
            assert!(c.body.get("tools").is_none(), "tools offered: {}", c.body);
        }
    }
}
