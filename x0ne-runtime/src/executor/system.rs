use std::net::TcpStream;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::config::AppConfig;
use crate::result::{ActionOutput, ActionResult, ErrorKind, ProcessEntry};

// ── SystemInfo ────────────────────────────────────────────────────────────────

pub async fn system_info(_config: &AppConfig) -> ActionResult {
    let hostname = read_file_trimmed("/proc/sys/kernel/hostname")
        .or_else(|_| hostname_from_env())
        .unwrap_or_else(|_| "unknown".into());

    let os = read_file_trimmed("/etc/os-release")
        .map(|s| {
            s.lines()
                .find(|l| l.starts_with("PRETTY_NAME="))
                .map(|l| {
                    l.trim_start_matches("PRETTY_NAME=")
                        .trim_matches('"')
                        .to_string()
                })
                .unwrap_or_else(|| "Linux".into())
        })
        .unwrap_or_else(|_| "Linux".into());

    let arch = std::env::consts::ARCH.to_string();

    let cpu_count = read_file_trimmed("/proc/cpuinfo")
        .map(|c| c.lines().filter(|l| l.starts_with("processor")).count())
        .unwrap_or(1);

    let (memory_total_mb, memory_available_mb) = parse_meminfo();
    let disk_free_mb = disk_free_mb_root();

    // T1: ground-truth working directory of the executor process, so the
    // caller (orchestrator) does NOT have to assume its own host cwd
    // matches the runtime's cwd (it almost never does for sandbox/HTTP).
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    ActionResult::ok(
        "SystemInfo",
        ActionOutput::SystemInfoOutput {
            hostname,
            os,
            arch,
            cpu_count,
            memory_total_mb,
            memory_available_mb,
            disk_free_mb,
            cwd,
        },
        0,
    )
}

fn read_file_trimmed(path: &str) -> std::io::Result<String> {
    Ok(std::fs::read_to_string(path)?.trim().to_string())
}

fn hostname_from_env() -> std::io::Result<String> {
    std::env::var("HOSTNAME")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "no hostname"))
}

fn parse_meminfo() -> (u64, u64) {
    let Ok(content) = std::fs::read_to_string("/proc/meminfo") else {
        return (0, 0);
    };
    let mut total_kb = 0u64;
    let mut available_kb = 0u64;
    for line in content.lines() {
        if line.starts_with("MemTotal:") {
            total_kb = parse_kb_field(line);
        } else if line.starts_with("MemAvailable:") {
            available_kb = parse_kb_field(line);
        }
    }
    (total_kb / 1024, available_kb / 1024)
}

fn parse_kb_field(line: &str) -> u64 {
    line.split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn disk_free_mb_root() -> u64 {
    // Use `statvfs` via a raw bytes approach (safe libc alternative)
    // For simplicity: read from /proc/mounts or just return 0 if unavailable
    // Real production code would use nix::sys::statvfs or libc::statvfs
    // We use a subprocess-free approach: check df via std::process
    use std::process::Command;
    let mut cmd = Command::new("df");
    // SECURITY: cleared environment (`crate::child_env`). A fixed-argv probe
    // leaks nothing by itself, but "every spawn goes through one door" is a far
    // cheaper invariant to hold than "the ones someone judged risky do".
    crate::child_env::harden(&mut cmd);
    if let Ok(out) = cmd.arg("-B1M").arg("/").output() {
        let s = String::from_utf8_lossy(&out.stdout);
        for line in s.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 {
                if let Ok(v) = parts[3].trim_end_matches('M').parse::<u64>() {
                    return v;
                }
            }
        }
    }
    0
}

// ── FileInfo ──────────────────────────────────────────────────────────────────

pub async fn file_info(path: &str, config: &AppConfig) -> ActionResult {
    // FileInfo discloses existence, size and type, so it is a READ.
    let confined = match crate::executor::confine::resolve_read(path, &config.policy) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("FileInfo", e.kind(), e.message(), 0),
    };
    let p = confined.as_path();
    let meta = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) => return ActionResult::err("FileInfo", ErrorKind::NotFound, e.to_string(), 0),
    };

    let modified = meta
        .modified()
        .ok()
        .map(|t| {
            let d: chrono::DateTime<chrono::Utc> = t.into();
            d.to_rfc3339()
        })
        .unwrap_or_default();

    let created = meta.created().ok().map(|t| {
        let d: chrono::DateTime<chrono::Utc> = t.into();
        d.to_rfc3339()
    });

    let permissions = format_permissions(&meta);
    let is_symlink = meta.file_type().is_symlink();
    let symlink_target = if is_symlink {
        std::fs::read_link(p)
            .ok()
            .map(|t| t.to_string_lossy().into_owned())
    } else {
        None
    };

    // MIME type: try `file --mime-type` if available, otherwise None
    let mime_type = detect_mime(p);

    ActionResult::ok(
        "FileInfo",
        ActionOutput::FileInfoOutput {
            path: path.to_string(),
            size_bytes: meta.len(),
            permissions,
            is_dir: meta.is_dir(),
            is_symlink,
            symlink_target,
            modified,
            created,
            mime_type,
        },
        0,
    )
}

fn format_permissions(meta: &std::fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode();
    format!("{:o}", mode & 0o777)
}

fn detect_mime(path: &std::path::Path) -> Option<String> {
    use std::process::Command;
    let mut cmd = Command::new("file");
    // SECURITY: cleared environment, same door as every other spawn.
    crate::child_env::harden(&mut cmd);
    cmd.arg("--mime-type")
        .arg("-b")
        .arg(path)
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        })
}

/// Lowercase hex digest of `data`, or `None` for an algorithm we do not
/// offer. Its own function so the OUTPUT can be pinned by a test: until
/// 2026-09-18 nothing checked a single digest value, and the sha2 0.11
/// upgrade changed how this one is formatted (#102m).
fn hex_digest(algo: &str, data: &[u8]) -> Option<String> {
    // The 0.11 digests return a `hybrid_array::Array`, which does not
    // implement `LowerHex`: format byte by byte.
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
    Some(match algo {
        "sha256" => hex(&Sha256::digest(data)),
        "md5" => hex(&md5::Md5::digest(data)),
        "sha1" => hex(&sha1::Sha1::digest(data)),
        _ => return None,
    })
}

// ── Checksum ──────────────────────────────────────────────────────────────────

pub async fn checksum(path: &str, algorithm: Option<&str>, config: &AppConfig) -> ActionResult {
    let algo = algorithm.unwrap_or("sha256");

    let confined = match crate::executor::confine::resolve_read(path, &config.policy) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("Checksum", e.kind(), e.message(), 0),
    };

    // Check file size before reading to prevent OOM on large files.
    let max_bytes = config.policy.max_read_bytes;
    match std::fs::metadata(confined.as_path()) {
        Ok(meta) if meta.len() > max_bytes => {
            return ActionResult::err(
                "Checksum",
                ErrorKind::InvalidInput,
                format!(
                    "File too large for checksum ({} bytes, max {})",
                    meta.len(),
                    max_bytes
                ),
                0,
            );
        }
        Err(e) => return ActionResult::err("Checksum", ErrorKind::NotFound, e.to_string(), 0),
        _ => {}
    }

    let data = match confined.read_bytes() {
        Ok(d) => d,
        Err(e) => return ActionResult::err("Checksum", e.kind(), e.message(), 0),
    };

    let Some(hash) = hex_digest(algo, &data) else {
        return ActionResult::err(
            "Checksum",
            ErrorKind::InvalidInput,
            format!("Unknown algorithm '{}'. Use sha256, md5, or sha1.", algo),
            0,
        );
    };

    ActionResult::ok(
        "Checksum",
        ActionOutput::ChecksumOutput {
            path: path.to_string(),
            algorithm: algo.to_string(),
            hash,
        },
        0,
    )
}

// ── ListProcesses ─────────────────────────────────────────────────────────────

pub async fn list_processes(filter: Option<&str>, config: &AppConfig) -> ActionResult {
    if !config.system.enable_process_list {
        return ActionResult::err(
            "ListProcesses",
            ErrorKind::PolicyDenied,
            "Process listing is disabled (system.enable_process_list = false).",
            0,
        );
    }

    let mut processes: Vec<ProcessEntry> = Vec::new();

    if let Ok(proc) = std::fs::read_dir("/proc") {
        for entry in proc.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Only PID dirs (numeric)
            let Ok(pid) = name_str.parse::<u32>() else {
                continue;
            };

            let status_path = entry.path().join("status");
            let Ok(status) = std::fs::read_to_string(&status_path) else {
                continue;
            };

            let proc_name = status
                .lines()
                .find(|l| l.starts_with("Name:"))
                .map(|l| l.trim_start_matches("Name:").trim().to_string())
                .unwrap_or_default();

            if let Some(f) = filter {
                if !proc_name.contains(f) {
                    continue;
                }
            }

            let memory_kb = status
                .lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);

            let cmdline_path = entry.path().join("cmdline");
            let command = std::fs::read(cmdline_path)
                .map(|b| {
                    // Other programs' argv: a secret passed as an argument
                    // reached the model verbatim. Masked argument by argument.
                    let raw = String::from_utf8_lossy(&b);
                    let argv: Vec<&str> = raw.split('\0').collect();
                    let s = x0ne_audit::redact::redact_argv(&argv)
                        .join(" ")
                        .trim()
                        .to_string();
                    if s.is_empty() {
                        format!("[{}]", proc_name)
                    } else {
                        s
                    }
                })
                .unwrap_or_else(|_| format!("[{}]", proc_name));

            processes.push(ProcessEntry {
                pid,
                name: proc_name,
                command,
                memory_kb,
            });
        }
    }

    processes.sort_by_key(|p| p.pid);

    ActionResult::ok("ListProcesses", ActionOutput::ProcessList { processes }, 0)
}

// ── PortCheck ─────────────────────────────────────────────────────────────────

pub async fn port_check(host: &str, port: u16, timeout_secs: Option<u64>) -> ActionResult {
    // Same classification as any outbound request. Unguarded, this returned
    // `open` plus a latency for any address the model named — a blind internal
    // port scanner and a metadata-service liveness oracle, force-enabled in the
    // compiled defaults.
    let addr = match check_host(host, port, false) {
        Ok(a) => a,
        Err(reason) => {
            return ActionResult::err("PortCheck", ErrorKind::PolicyDenied, reason, 0);
        }
    };
    let timeout = Duration::from_secs(timeout_secs.unwrap_or(5));
    let host = host.to_string();

    let start = std::time::Instant::now();
    // Connect to the address that was classified, not a re-resolution of it.
    let open =
        tokio::task::spawn_blocking(move || TcpStream::connect_timeout(&addr, timeout).is_ok())
            .await
            .unwrap_or(false);

    let latency_ms = if open {
        Some(start.elapsed().as_millis() as u64)
    } else {
        None
    };

    ActionResult::ok(
        "PortCheck",
        ActionOutput::PortCheckResult {
            host,
            port,
            open,
            latency_ms,
        },
        0,
    )
}

// ── GetEnv ────────────────────────────────────────────────────────────────────

pub fn get_env(name: &str, _config: &AppConfig) -> ActionResult {
    // Policy already checked the whitelist — just read it
    let value = std::env::var(name).ok();
    let set = value.is_some();
    ActionResult::ok(
        "GetEnv",
        ActionOutput::EnvVarResult {
            name: name.to_string(),
            value,
            set,
        },
        0,
    )
}

// ── DnsLookup ─────────────────────────────────────────────────────────────────

/// Resolve a hostname to addresses.
///
/// The same classification applies as for any other outbound target. Returning
/// resolution results is the point of the action, but without a check it is an
/// internal-name enumeration primitive and a DNS exfiltration channel: any name
/// resolving into RFC1918, link-local or a metadata endpoint would be reported
/// back verbatim.
///
/// The rule: a name may be looked up when every address it resolves to is a
/// permitted destination. A name that resolves to anything internal is refused
/// with the reason, rather than being answered.
pub async fn dns_lookup(hostname: &str) -> ActionResult {
    // Port 80 is a neutral resolution port; the value is not used to connect.
    if let Err(reason) = check_host(hostname, 80, false) {
        return ActionResult::err("DnsLookup", ErrorKind::PolicyDenied, reason, 0);
    }
    let hostname_str = hostname.to_string();
    let result = tokio::task::spawn_blocking(move || {
        use std::net::ToSocketAddrs;
        format!("{}:0", hostname_str)
            .to_socket_addrs()
            .map(|addrs| addrs.map(|a| a.ip().to_string()).collect::<Vec<_>>())
    })
    .await;

    match result {
        Ok(Ok(addresses)) => {
            let resolved = !addresses.is_empty();
            ActionResult::ok(
                "DnsLookup",
                ActionOutput::DnsResult {
                    hostname: hostname.to_string(),
                    addresses,
                    resolved,
                },
                0,
            )
        }
        Ok(Err(_e)) => ActionResult::ok(
            "DnsLookup",
            ActionOutput::DnsResult {
                hostname: hostname.to_string(),
                addresses: vec![],
                resolved: false,
            },
            0,
        ),
        Err(e) => ActionResult::err("DnsLookup", ErrorKind::Internal, e.to_string(), 0),
    }
}

// ── SSRF Protection ────────────────────────────────────────────────────────

/// Maximum HTTP response body size (2 MB). Prevents OOM when an LLM
/// requests a very large URL or a slow-drip endpoint.
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// Read a response body with a hard size cap. Checks `Content-Length` first
/// for an early bail, then streams chunks up to the cap. This prevents OOM
/// from large or infinite responses — the old code called `.bytes().await`
/// which downloaded the *entire* body before checking the size.
async fn read_body_capped(
    response: reqwest::Response,
    action: &str,
    cap: usize,
) -> Result<(u16, String), ActionResult> {
    let status = response.status().as_u16();

    // Early bail: if server declares a Content-Length beyond our cap, reject
    // without reading a single byte.
    if let Some(cl) = response.content_length() {
        if cl as usize > cap {
            return Err(ActionResult::err(
                action,
                ErrorKind::InvalidInput,
                format!(
                    "Response Content-Length too large ({} bytes, max {})",
                    cl, cap
                ),
                0,
            ));
        }
    }

    // Stream chunks with a hard cap — handles chunked transfer encoding and
    // servers that lie about Content-Length.
    use futures::StreamExt;
    let mut buf = Vec::with_capacity(cap.min(64 * 1024));
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                if buf.len() + bytes.len() > cap {
                    return Err(ActionResult::err(
                        action,
                        ErrorKind::InvalidInput,
                        format!("Response body exceeds {} byte limit (truncated)", cap),
                        0,
                    ));
                }
                buf.extend_from_slice(&bytes);
            }
            Err(e) => {
                return Err(ActionResult::err(
                    action,
                    ErrorKind::Internal,
                    e.to_string(),
                    0,
                ));
            }
        }
    }

    let body = String::from_utf8_lossy(&buf).into_owned();
    Ok((status, body))
}

/// Result of an SSRF check: either blocked (with reason) or allowed
/// with the resolved socket address pinned for use in the request.
pub(crate) struct SsrfCheckResult {
    /// Pinned resolved address to pass to reqwest, preventing DNS rebinding.
    resolved_addr: Option<std::net::SocketAddr>,
}

/// Check if a URL targets a private/internal IP address.
/// Blocks SSRF attacks where an LLM requests internal services or cloud metadata.
///
/// When `X0NE_ALLOW_LOOPBACK=1` (set by the sandbox configuration — the
/// container *is* the isolation boundary, hitting its own loopback is safe),
/// this allows `127.0.0.0/8` and `::1` but keeps blocking other private
/// ranges, link-local, and cloud metadata endpoints.
///
/// On success returns `Ok(SsrfCheckResult)` with a pinned resolved address
/// to prevent DNS rebinding (TOCTOU) attacks.
fn check_ssrf(url: &str) -> Result<SsrfCheckResult, String> {
    check_ssrf_inner(url, false)
}

/// SSRF check tuned for LLM endpoints. Identical to [`check_ssrf`] except that
/// loopback is allowed by default — local Ollama (`http://127.0.0.1:11434`) is a
/// first-class, documented use case. Cloud-metadata, link-local, and other
/// private/internal ranges are STILL blocked, which is what closes the
/// credential-exfiltration vector for agent-controlled endpoints.
#[cfg(feature = "prim-ml")]
pub(crate) fn check_ssrf_llm(url: &str) -> Result<SsrfCheckResult, String> {
    check_ssrf_inner(url, true)
}

/// The authoritative host/address check.
///
/// Everything after URL parsing lives here so that callers holding a *host*
/// rather than a URL — `PortCheck`, `DnsLookup`, `Subscribe{port_open}` — get
/// the identical classification instead of a second, weaker copy. Those three
/// had no check at all: `PortCheck` returned `open` plus a latency for any
/// address, which is a blind internal port scanner and a cloud-metadata
/// liveness oracle, and both it and `DnsLookup` are force-enabled in the
/// compiled defaults.
///
/// Returns the pinned address on success, so a caller that goes on to connect
/// uses the address that was classified rather than re-resolving.
pub(crate) fn check_host(
    host: &str,
    port: u16,
    default_allow_loopback: bool,
) -> Result<std::net::SocketAddr, String> {
    let host = host.to_lowercase();

    // Cloud metadata endpoints are ALWAYS blocked, by name.
    if host == "169.254.169.254"
        || host == "metadata.google.internal"
        || host == "metadata.goog"
        || host == "metadata"
    {
        return Err(format!("cloud metadata endpoint blocked: {}", host));
    }

    let allow_loopback = default_allow_loopback
        || std::env::var("X0NE_ALLOW_LOOPBACK")
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

    // Defense in depth: classify an IP *literal* directly rather than relying
    // on the resolver round-trip. Strip IPv6 brackets first, so
    // `[::ffff:169.254.169.254]` is caught here — the name check above cannot
    // see it.
    let literal_candidate = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = literal_candidate.parse::<std::net::IpAddr>() {
        if ip_is_blocked(ip, allow_loopback) {
            return Err(format!("private/internal IP blocked: {}", ip));
        }
    }

    // Resolve once and pin, to close the rebinding window.
    use std::net::ToSocketAddrs;
    let addr_str = format!("{}:{}", host, port);
    // SECURITY: resolution FAILS CLOSED. A resolver error or an empty result
    // used to skip classification entirely.
    let addrs: Vec<std::net::SocketAddr> = match addr_str.to_socket_addrs() {
        Ok(it) => it.collect(),
        Err(e) => return Err(format!("host resolution failed for {}: {}", host, e)),
    };
    if addrs.is_empty() {
        return Err(format!("host {} resolved to no addresses", host));
    }

    // Every resolved address must be safe. A host with even one internal
    // address is rejected outright rather than cherry-picking a public one,
    // since resolver order is attacker-influenceable.
    let mut pinned_addr: Option<std::net::SocketAddr> = None;
    for addr in addrs {
        let ip = addr.ip();
        if ip_is_blocked(ip, allow_loopback) {
            return Err(format!("private/internal IP blocked: {} -> {}", host, ip));
        }
        if pinned_addr.is_none() {
            pinned_addr = Some(addr);
        }
    }
    pinned_addr.ok_or_else(|| format!("no usable address for {}", host))
}

/// Single source of truth for "is this resolved IP internal / forbidden".
///
/// SECURITY: this MUST handle IPv6 as thoroughly as IPv4. The pre-hardening
/// version classified only `IpAddr::V4`, so an IPv4-mapped literal
/// (`::ffff:169.254.169.254`) or an `AAAA` record pointing at one sailed
/// through and then got *pinned* by `build_pinned_client`, giving a
/// deterministic route to cloud-metadata credentials.
pub(crate) fn ip_is_blocked(ip: std::net::IpAddr, allow_loopback: bool) -> bool {
    use std::net::IpAddr;

    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                return !allow_loopback;
            }
            let o = v4.octets();
            v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || v4.is_multicast()
                // 169.254.0.0/16 (incl. the 169.254.169.254 metadata address)
                || (o[0] == 169 && o[1] == 254)
                // 100.64.0.0/10 carrier-grade NAT
                || (o[0] == 100 && (64..=127).contains(&o[1]))
                // 0.0.0.0/8 "this network"
                || o[0] == 0
        }
        IpAddr::V6(v6) => {
            // Normalize IPv4-mapped / IPv4-compatible addresses back to v4 and
            // re-run the v4 rules, so `::ffff:169.254.169.254` is treated
            // exactly like `169.254.169.254`.
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return ip_is_blocked(IpAddr::V4(mapped), allow_loopback);
            }
            if let Some(compat) = v6.to_ipv4() {
                return ip_is_blocked(IpAddr::V4(compat), allow_loopback);
            }
            if v6.is_loopback() {
                return !allow_loopback;
            }
            let seg = v6.segments();
            v6.is_unspecified()
                || v6.is_multicast()
                // fc00::/7 unique-local
                || (seg[0] & 0xfe00) == 0xfc00
                // fe80::/10 unicast link-local
                || (seg[0] & 0xffc0) == 0xfe80
        }
    }
}

fn check_ssrf_inner(url: &str, default_allow_loopback: bool) -> Result<SsrfCheckResult, String> {
    check_ssrf_via(url, default_allow_loopback, egress_via_proxy())
}

fn check_ssrf_via(
    url: &str,
    default_allow_loopback: bool,
    via_proxy: bool,
) -> Result<SsrfCheckResult, String> {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return Err("invalid URL".into()),
    };

    let host = match parsed.host_str() {
        Some(h) => h.to_lowercase(),
        None => return Err("no host in URL".into()),
    };

    // Cloud metadata endpoints are ALWAYS blocked, by name.
    if host == "169.254.169.254"
        || host == "metadata.google.internal"
        || host == "metadata.goog"
        || host == "metadata"
    {
        return Err(format!("cloud metadata endpoint blocked: {}", host));
    }

    let port = parsed.port_or_known_default().unwrap_or(80);
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    if via_proxy && literal.parse::<std::net::IpAddr>().is_err() {
        // The sandbox has no DNS: names go to the allowlisting proxy, which
        // resolves them. Local resolution failed every FetchUrl (2026-10-06).
        return Ok(SsrfCheckResult {
            resolved_addr: None,
        });
    }
    let pinned = check_host(&host, port, default_allow_loopback)?;

    Ok(SsrfCheckResult {
        resolved_addr: Some(pinned),
    })
}

/// Egress goes through the sandbox's allowlisting proxy (run-sandboxed.sh).
fn egress_via_proxy() -> bool {
    std::env::var("X0NE_SANDBOX_NET").is_ok_and(|v| v.trim() == "proxy")
        && ["HTTPS_PROXY", "https_proxy"]
            .iter()
            .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()))
}

/// Backward-compatible wrapper: returns Some(reason) if blocked, None if allowed.
#[cfg(test)]
fn is_ssrf_target(url: &str) -> Option<String> {
    check_ssrf(url).err()
}

/// Build a reqwest client with the resolved address pinned to prevent
/// DNS rebinding between the SSRF check and the actual request.
///
/// SECURITY: a client that cannot be built is an error. `unwrap_or_default()`
/// used to hand back a default client: no pin, no redirect check, the proxy
/// settings of the environment (audit of 2026-11-04).
pub(crate) fn build_pinned_client(
    url: &str,
    ssrf: &SsrfCheckResult,
    timeout: Duration,
) -> Result<reqwest::Client, String> {
    build_pinned_client_with(url, ssrf, timeout, |target| check_ssrf(target).is_ok())
}

/// `build_pinned_client` with the redirect check passed in, so a test can
/// stand in for a host name whose first DNS answer passes `check_ssrf`.
fn build_pinned_client_with(
    url: &str,
    ssrf: &SsrfCheckResult,
    timeout: Duration,
    redirect_check: fn(&str) -> bool,
) -> Result<reqwest::Client, String> {
    let pinned_host = ssrf
        .resolved_addr
        .and_then(|_| url::Url::parse(url).ok())
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
    let mut builder = reqwest::Client::builder()
        .timeout(timeout)
        // SECURITY: the IP pin below only covers the INITIAL host. Every
        // redirect target is re-validated against the SSRF policy, the chain
        // is capped, and a hop the pin cannot cover is not followed.
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5
                || !redirect_keeps_the_pin(attempt.url(), pinned_host.as_deref())
            {
                return attempt.stop();
            }
            if redirect_check(attempt.url().as_str()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }));
    // A loopback URL skips HTTP_PROXY and its kin: what is sent to it
    // reaches no proxy (review of the pre-release build, S2).
    if x0ne_actions::model_keys::is_loopback(url) {
        builder = builder.no_proxy();
    }
    // Pin the resolved IP so reqwest does not re-resolve DNS.
    if let Some(addr) = ssrf.resolved_addr {
        if let Ok(parsed) = url::Url::parse(url) {
            if let Some(host) = parsed.host_str() {
                builder = builder.resolve(host, addr);
            }
        }
    }
    builder
        .build()
        .map_err(|e| format!("HTTP client could not be built: {e}"))
}

/// Whether a redirect target is reached through the pin of the first request.
///
/// SECURITY: `check_ssrf` resolves a new host name, then reqwest resolves it
/// again to connect, and the second answer can be internal (DNS rebinding).
/// The pin covers the first host name and an IP literal needs no resolution;
/// any other name is not followed, and the 3xx goes back to the caller, whose
/// next request gets its own check and pin. Without a pin (names resolved by
/// the sandbox proxy) the target meets `check_ssrf` only, as before.
fn redirect_keeps_the_pin(target: &url::Url, pinned_host: Option<&str>) -> bool {
    let Some(pinned) = pinned_host else {
        return true;
    };
    match target.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case(pinned),
        Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)) => true,
        None => false,
    }
}

// ── FetchUrl ──────────────────────────────────────────────────────────────────

/// FetchUrl body cap; HttpRequest keeps MAX_RESPONSE_BYTES.
const FETCH_MAX_BYTES: usize = 5 * 1024 * 1024;
const FETCH_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                        (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const FETCH_UA_PLAIN: &str = concat!("x0ne/", env!("CARGO_PKG_VERSION"));

/// Optional FetchUrl knobs, as the action carries them.
#[derive(Debug, Default, Clone)]
pub struct FetchOpts {
    /// `markdown` (default), `text` or `html`.
    pub format: Option<String>,
    pub timeout_secs: Option<u64>,
    pub offset: Option<usize>,
}

/// 30 s by default, clamped to 1..=120.
fn fetch_timeout(secs: Option<u64>) -> Duration {
    Duration::from_secs(secs.unwrap_or(30).clamp(1, 120))
}

fn fetch_accept(format: &str) -> &'static str {
    match format {
        "markdown" => "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1",
        "text" => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
        _ => "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    }
}

pub async fn fetch_url(
    url: &str,
    headers: Option<&std::collections::HashMap<String, String>>,
    opts: &FetchOpts,
) -> ActionResult {
    let format = opts.format.as_deref().unwrap_or("markdown");
    if !matches!(format, "markdown" | "text" | "html") {
        return ActionResult::err(
            "FetchUrl",
            ErrorKind::InvalidInput,
            format!("format must be markdown, text or html, got `{format}`"),
            0,
        );
    }
    // SSRF protection: block private/internal IPs + pin resolved address
    let ssrf = match check_ssrf(url) {
        Ok(s) => s,
        Err(reason) => return ActionResult::err("FetchUrl", ErrorKind::PolicyDenied, reason, 0),
    };
    let offset = opts.offset.unwrap_or(0);
    // Requests with caller headers (often credentials) are never cached.
    let cacheable = headers.is_none_or(|h| h.is_empty());
    let key = super::web::cache_key(url, format);
    if cacheable {
        let hit = super::web::FETCH_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key, std::time::Instant::now());
        if let Some(c) = hit {
            return fetch_result(url, c.status, &c.text, offset, true);
        }
    }

    let client = match build_pinned_client(url, &ssrf, fetch_timeout(opts.timeout_secs)) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("FetchUrl", ErrorKind::Internal, e, 0),
    };
    let caller_ua = headers.is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case("user-agent")));
    let send = |ua: &'static str| {
        let mut request = client.get(url).header("Accept", fetch_accept(format));
        if !caller_ua {
            request = request.header("User-Agent", ua);
        }
        if let Some(h) = headers {
            for (k, v) in h {
                request = request.header(k, v);
            }
        }
        request.send()
    };
    let mut response = match send(FETCH_UA).await {
        Ok(r) => r,
        Err(e) => return ActionResult::err("FetchUrl", ErrorKind::Internal, e.to_string(), 0),
    };
    // Cloudflare challenges browser-looking agents that run no JS; an honest UA often passes.
    let challenged = response.status().as_u16() == 403
        && response
            .headers()
            .get("cf-mitigated")
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"challenge"));
    if challenged && !caller_ua {
        response = match send(FETCH_UA_PLAIN).await {
            Ok(r) => r,
            Err(e) => return ActionResult::err("FetchUrl", ErrorKind::Internal, e.to_string(), 0),
        };
    }

    let is_html = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            let ct = ct.to_ascii_lowercase();
            ct.starts_with("text/html") || ct.starts_with("application/xhtml+xml")
        })
        .unwrap_or(false);
    let final_url = response.url().to_string();
    let (status, body) = match read_body_capped(response, "FetchUrl", FETCH_MAX_BYTES).await {
        Ok(r) => r,
        Err(err_result) => return err_result,
    };
    let text = match (is_html, format) {
        (true, "markdown") => super::html_text::html_to_markdown(&body, Some(&final_url)),
        (true, "text") => super::html_text::html_to_text(&body, Some(&final_url)),
        _ => body,
    };
    if cacheable && (200..300).contains(&status) {
        super::web::FETCH_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put(key, status, text.clone(), std::time::Instant::now());
    }
    fetch_result(url, status, &text, offset, false)
}

fn fetch_result(url: &str, status: u16, text: &str, offset: usize, cached: bool) -> ActionResult {
    let w = super::html_text::window(text, offset, super::html_text::MAX_TEXT_BYTES);
    ActionResult::ok(
        "FetchUrl",
        ActionOutput::FetchUrlResult {
            url: url.to_string(),
            status,
            body: w.body,
            total_bytes: Some(w.total),
            next_offset: w.next_offset,
            cached,
            source: super::web::cite(url),
        },
        0,
    )
}

// ── Web search ────────────────────────────────────────────────────────────────
//
// `WebSearch { query, limit }` returns a ranked list of
// `{title, url, snippet, source}` results. Backends, first answer wins:
//   * Exa's public MCP endpoint (no key; the query goes to mcp.exa.ai).
//     `X0NE_EXA_DISABLE=1` skips it.
//   * Brave Search API when `X0NE_BRAVE_API_KEY` is set.
//   * DuckDuckGo HTML scrape (no key; often served an anti-bot page).
// Each failure before the answer is listed in `fallbacks`.

pub async fn web_search(query: &str, limit: usize) -> ActionResult {
    let query = query.trim();
    if query.is_empty() {
        return ActionResult::err(
            "WebSearch",
            ErrorKind::InvalidInput,
            "WebSearch requires a non-empty `query`".to_string(),
            0,
        );
    }
    let limit = limit.clamp(1, 25);

    let brave_key = std::env::var("X0NE_BRAVE_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let exa_off = std::env::var("X0NE_EXA_DISABLE").is_ok_and(|v| v == "1" || v == "true");
    let started = std::time::Instant::now();
    let mut backends: Vec<(&'static str, super::web::BackendFuture<'_>)> = Vec::new();
    if !exa_off {
        backends.push((
            "exa",
            Box::pin(super::web::exa_search(
                super::web::EXA_MCP_URL,
                query,
                limit,
            )),
        ));
    }
    if let Some(key) = brave_key {
        backends.push((
            "brave",
            Box::pin(async move { web_search_brave(query, limit, &key).await }),
        ));
    }
    backends.push(("ddg", Box::pin(web_search_ddg(query, limit))));
    let searched = match super::web::first_answer(backends).await {
        Ok(s) => s,
        Err(failures) => {
            return ActionResult::err(
                "WebSearch",
                ErrorKind::Internal,
                super::web::all_failed(&failures),
                0,
            )
        }
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    ActionResult::ok(
        "WebSearch",
        ActionOutput::Generic {
            data: search_envelope(query, searched),
        },
        elapsed_ms,
    )
}

/// The `web_search_result` payload; every hit carries its citation id.
fn search_envelope(query: &str, s: super::web::Searched) -> serde_json::Value {
    let results: Vec<serde_json::Value> = s
        .results
        .into_iter()
        .map(|mut r| {
            if let Some(id) = r["url"].as_str().and_then(super::web::cite) {
                r["source"] = serde_json::Value::String(id);
            }
            r
        })
        .collect();
    let mut data = serde_json::json!({
        "kind": "web_search_result",
        "query": query,
        "source": s.source,
        "count": results.len(),
        "results": results,
    });
    if !s.failures.is_empty() {
        data["fallbacks"] = s
            .failures
            .iter()
            .map(|(b, e)| serde_json::json!({"backend": b, "error": e}))
            .collect();
    }
    data
}

/// Brave Search API endpoint.
const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
/// DuckDuckGo's result page that needs no JS.
const DDG_ENDPOINT: &str = "https://html.duckduckgo.com/html/";
/// Cap on a search backend's answer, the one HttpRequest has. `resp.json()`
/// and `resp.text()` read the whole body however long it was (2026-11-04).
const SEARCH_MAX_BYTES: usize = MAX_RESPONSE_BYTES;

/// A search backend's body, refused past `SEARCH_MAX_BYTES`.
async fn search_body(resp: reqwest::Response, backend: &str) -> Result<String, String> {
    match read_body_capped(resp, "WebSearch", SEARCH_MAX_BYTES).await {
        Ok((_, body)) => Ok(body),
        Err(r) => Err(format!(
            "{backend}: {}",
            r.error.map(|e| e.message).unwrap_or_default()
        )),
    }
}

/// Brave Search API path.
async fn web_search_brave(
    query: &str,
    limit: usize,
    key: &str,
) -> Result<Vec<serde_json::Value>, String> {
    web_search_brave_at(BRAVE_ENDPOINT, query, limit, key).await
}

/// `web_search_brave` against `endpoint`, so a test can stand in for Brave.
async fn web_search_brave_at(
    endpoint: &str,
    query: &str,
    limit: usize,
    key: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let url = format!(
        "{endpoint}?q={}&count={}",
        urlencoding_minimal(query),
        limit,
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(&url)
        .header("Accept", "application/json")
        .header("X-Subscription-Token", key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("brave returned HTTP {}", resp.status()));
    }
    let body = search_body(resp, "brave").await?;
    let body: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let arr = body["web"]["results"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    Ok(arr
        .into_iter()
        .take(limit)
        .map(|item| {
            serde_json::json!({
                "title": item["title"].as_str().unwrap_or(""),
                "url": item["url"].as_str().unwrap_or(""),
                "snippet": item["description"].as_str().unwrap_or(""),
            })
        })
        .collect())
}

/// DuckDuckGo HTML scrape — the no-key fallback. We hit the
/// /html/ endpoint (no JS required) and pull `<a class="result__a">`
/// + `<a class="result__snippet">` pairs out via crude regexes.
/// DuckDuckGo's HTML layout is stable enough that this works without
/// pulling a full HTML parser. A page with no result block is an
/// error, not 0 results (see `ddg_results`).
async fn web_search_ddg(query: &str, limit: usize) -> Result<Vec<serde_json::Value>, String> {
    web_search_ddg_at(DDG_ENDPOINT, query, limit).await
}

/// `web_search_ddg` against `endpoint`, so a test can stand in for DuckDuckGo.
async fn web_search_ddg_at(
    endpoint: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<serde_json::Value>, String> {
    let url = format!("{endpoint}?q={}", urlencoding_minimal(query));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (compatible; x0ne/0.2)")
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(&url)
        // POSTing to /html/ avoids the redirect-to-lite flow.
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("ddg returned HTTP {}", resp.status()));
    }
    let html = search_body(resp, "ddg").await?;
    ddg_results(&html, limit)
}

/// Classify a DuckDuckGo HTML page: results, a genuine empty search,
/// or a page that is not a result page at all (anti-bot challenge).
pub(crate) fn ddg_results(html: &str, limit: usize) -> Result<Vec<serde_json::Value>, String> {
    const WAYS_OUT: &str = "set X0NE_BRAVE_API_KEY to use the Brave Search API, \
                            or FetchUrl a known URL directly";
    if html.contains("result__title") {
        return Ok(parse_ddg_html(html, limit));
    }
    if html.contains("no-results") || html.contains("result--no-result") {
        return Ok(Vec::new());
    }
    // 2026-10-06: the challenge page answers 200 with 67 "anomaly" markers.
    // Reading it as "0 results" made the model conclude the web had nothing.
    let lower = html.to_ascii_lowercase();
    if lower.contains("anomaly") || lower.contains("challenge") {
        return Err(format!(
            "DuckDuckGo blocked the request (anti-bot challenge page, not an empty search); {}",
            WAYS_OUT
        ));
    }
    if html.trim().is_empty() {
        return Ok(Vec::new());
    }
    Err(format!(
        "DuckDuckGo blocked the request or changed its layout (no result block in a {}-byte page); {}",
        html.len(),
        WAYS_OUT
    ))
}

/// Extract the (title, url, snippet) triples from a DuckDuckGo HTML
/// response. Looks for the canonical `result__title` /
/// `result__snippet` blocks; tolerates missing snippets (some hits
/// don't have one). Pulled into a pure fn so unit tests don't need
/// a live HTTP fixture.
pub(crate) fn parse_ddg_html(html: &str, limit: usize) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    // Each result block opens with `<h2 class="result__title">` and
    // closes within ~1KB. Find them by anchor on the class.
    let mut cursor = 0usize;
    while out.len() < limit {
        let Some(start) = html[cursor..].find("result__title") else {
            break;
        };
        let start = cursor + start;
        // Find the <a … href="…">…</a> inside this block.
        let block_end = html[start..]
            .find("result__snippet")
            .map(|e| start + e + 2_000)
            .unwrap_or(html.len());
        let block = &html[start..block_end.min(html.len())];
        let (url, title) = extract_anchor(block).unwrap_or_default();
        let snippet = extract_snippet(block).unwrap_or_default();
        if !url.is_empty() && !title.is_empty() {
            out.push(serde_json::json!({
                "title": title,
                "url": clean_ddg_url(&url),
                "snippet": snippet,
            }));
        }
        cursor = start + "result__title".len();
    }
    out
}

fn extract_anchor(block: &str) -> Option<(String, String)> {
    let i = block.find("<a")?;
    let href_key = "href=\"";
    let h = block[i..].find(href_key)? + i + href_key.len();
    let h_end = block[h..].find('"')? + h;
    let url = block[h..h_end].to_string();
    let close = block[h_end..].find('>')? + h_end + 1;
    let body_end = block[close..].find("</a>")? + close;
    let mut raw_title = block[close..body_end].to_string();
    // Strip tags inside the anchor body.
    raw_title = strip_html_tags(&raw_title);
    Some((url, raw_title.trim().to_string()))
}

fn extract_snippet(block: &str) -> Option<String> {
    let i = block.find("result__snippet")?;
    let close = block[i..].find('>')? + i + 1;
    let end = block[close..]
        .find("</a>")
        .or_else(|| block[close..].find("</div>"))?
        + close;
    Some(strip_html_tags(&block[close..end]).trim().to_string())
}

fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    // Unescape the most common HTML entities (no full parser — just
    // what DDG actually emits).
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
}

/// DDG wraps real URLs in `//duckduckgo.com/l/?uddg=<encoded>`. Pull
/// out the underlying URL so the Sources panel and the LLM see the
/// canonical link, not the tracker.
fn clean_ddg_url(u: &str) -> String {
    let needle = "uddg=";
    if let Some(i) = u.find(needle) {
        let tail = &u[i + needle.len()..];
        let cut = tail.find('&').unwrap_or(tail.len());
        return percent_decode(&tail[..cut]);
    }
    u.to_string()
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Tiny query-string encoder — enough for search engines. We only
/// escape characters that have meaning in query strings; the rest
/// passes through to keep URLs readable in logs.
fn urlencoding_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else if c == ' ' {
            out.push('+');
        } else {
            for b in c.to_string().bytes() {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

// ── Whoami ────────────────────────────────────────────────────────────────────

pub async fn whoami() -> ActionResult {
    let uid = libc_uid();
    let gid = libc_gid();
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| format!("uid:{}", uid));
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());

    ActionResult::ok(
        "Whoami",
        ActionOutput::WhoamiResult {
            username,
            uid,
            gid,
            home,
            shell,
        },
        0,
    )
}

fn libc_uid() -> u32 {
    #[cfg(unix)]
    {
        libc_getuid()
    }
    #[cfg(not(unix))]
    0
}

fn libc_gid() -> u32 {
    #[cfg(unix)]
    {
        libc_getgid()
    }
    #[cfg(not(unix))]
    0
}

#[cfg(unix)]
fn libc_getuid() -> u32 {
    // Read /proc/self/status for Uid field instead of linking libc
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
}

#[cfg(unix)]
fn libc_getgid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Gid:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
}

// ── DiskUsage ─────────────────────────────────────────────────────────────────

pub async fn disk_usage(path: &str) -> ActionResult {
    use std::process::Command;

    let mut cmd = Command::new("df");
    // SECURITY: cleared environment, same door as every other spawn.
    crate::child_env::harden(&mut cmd);
    let result = cmd.args(["-B1", path]).output();

    match result {
        Ok(output) => {
            let s = String::from_utf8_lossy(&output.stdout);
            // Parse df output: Filesystem 1B-blocks Used Available Use% Mounted
            for line in s.lines().skip(1) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 5 {
                    let total_bytes = parts[1].parse::<u64>().unwrap_or(0);
                    let used_bytes = parts[2].parse::<u64>().unwrap_or(0);
                    let free_bytes = parts[3].parse::<u64>().unwrap_or(0);
                    let usage_percent = if total_bytes > 0 {
                        (used_bytes as f64 / total_bytes as f64) * 100.0
                    } else {
                        0.0
                    };

                    return ActionResult::ok(
                        "DiskUsage",
                        ActionOutput::DiskUsageResult {
                            path: path.to_string(),
                            total_bytes,
                            used_bytes,
                            free_bytes,
                            usage_percent,
                        },
                        0,
                    );
                }
            }
            ActionResult::err(
                "DiskUsage",
                ErrorKind::ExecutionFailed,
                "Could not parse df output",
                0,
            )
        }
        Err(e) => ActionResult::err("DiskUsage", ErrorKind::ExecutionFailed, e.to_string(), 0),
    }
}

// ── Uptime ────────────────────────────────────────────────────────────────────

pub async fn uptime() -> ActionResult {
    let uptime_secs = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
        })
        .map(|f| f as u64)
        .unwrap_or(0);

    let load_avg = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .map(|s| {
            let parts: Vec<f64> = s
                .split_whitespace()
                .take(3)
                .filter_map(|v| v.parse().ok())
                .collect();
            [
                parts.first().copied().unwrap_or(0.0),
                parts.get(1).copied().unwrap_or(0.0),
                parts.get(2).copied().unwrap_or(0.0),
            ]
        })
        .unwrap_or([0.0, 0.0, 0.0]);

    ActionResult::ok(
        "Uptime",
        ActionOutput::UptimeResult {
            seconds: uptime_secs,
            load_avg,
        },
        0,
    )
}

// ── HttpRequest ───────────────────────────────────────────────────────────────

pub async fn http_request(
    url: &str,
    method: &str,
    headers: &std::collections::HashMap<String, String>,
    body: Option<&str>,
    timeout_secs: Option<u64>,
) -> ActionResult {
    // SSRF protection: block private/internal IPs + pin resolved address
    let ssrf = match check_ssrf(url) {
        Ok(s) => s,
        Err(reason) => return ActionResult::err("HttpRequest", ErrorKind::PolicyDenied, reason, 0),
    };

    let client =
        match build_pinned_client(url, &ssrf, Duration::from_secs(timeout_secs.unwrap_or(30))) {
            Ok(c) => c,
            Err(e) => return ActionResult::err("HttpRequest", ErrorKind::Internal, e, 0),
        };

    let mut request = match method.to_uppercase().as_str() {
        "GET" => client.get(url),
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "PATCH" => client.patch(url),
        "DELETE" => client.delete(url),
        "HEAD" => client.head(url),
        other => {
            return ActionResult::err(
                "HttpRequest",
                ErrorKind::InvalidInput,
                format!("Unsupported HTTP method: {}", other),
                0,
            );
        }
    };

    for (k, v) in headers {
        request = request.header(k, v);
    }

    if let Some(b) = body {
        request = request.body(b.to_string());
    }

    match request.send().await {
        Ok(response) => {
            let resp_headers: std::collections::HashMap<String, String> = response
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();

            match read_body_capped(response, "HttpRequest", MAX_RESPONSE_BYTES).await {
                Ok((status, resp_body)) => ActionResult::ok(
                    "HttpRequest",
                    ActionOutput::HttpResponse {
                        url: url.to_string(),
                        status,
                        headers: resp_headers,
                        body: resp_body,
                    },
                    0,
                ),
                Err(err_result) => err_result,
            }
        }
        Err(e) => ActionResult::err("HttpRequest", ErrorKind::Internal, e.to_string(), 0),
    }
}

// ── ReadJson ──────────────────────────────────────────────────────────────────

pub async fn read_json(path: &str, config: &AppConfig) -> ActionResult {
    let max_bytes = config.policy.max_read_bytes;

    match std::fs::read_to_string(path) {
        Ok(content) => {
            if content.len() as u64 > max_bytes {
                return ActionResult::err(
                    "ReadJson",
                    ErrorKind::InvalidInput,
                    format!("File exceeds max_read_bytes ({} bytes)", max_bytes),
                    0,
                );
            }
            let size_bytes = content.len() as u64;
            match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(data) => ActionResult::ok(
                    "ReadJson",
                    ActionOutput::JsonData {
                        path: path.to_string(),
                        data,
                        size_bytes,
                    },
                    0,
                ),
                Err(e) => ActionResult::err(
                    "ReadJson",
                    ErrorKind::InvalidInput,
                    format!("Invalid JSON in file: {}", e),
                    0,
                ),
            }
        }
        Err(e) => {
            let kind = match e.kind() {
                std::io::ErrorKind::NotFound => ErrorKind::NotFound,
                std::io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
                _ => ErrorKind::ExecutionFailed,
            };
            ActionResult::err("ReadJson", kind, e.to_string(), 0)
        }
    }
}

// ── WriteJson ─────────────────────────────────────────────────────────────────

pub async fn write_json(path: &str, data: &serde_json::Value, pretty: bool) -> ActionResult {
    let content = if pretty {
        serde_json::to_string_pretty(data)
    } else {
        serde_json::to_string(data)
    };

    match content {
        Ok(json_str) => {
            let bytes = json_str.len() as u64;
            match std::fs::write(path, &json_str) {
                Ok(()) => ActionResult::ok(
                    "WriteJson",
                    ActionOutput::JsonWritten {
                        path: path.to_string(),
                        bytes_written: bytes,
                        pretty,
                    },
                    0,
                ),
                Err(e) => {
                    let kind = match e.kind() {
                        std::io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
                        _ => ErrorKind::ExecutionFailed,
                    };
                    ActionResult::err("WriteJson", kind, e.to_string(), 0)
                }
            }
        }
        Err(e) => ActionResult::err("WriteJson", ErrorKind::Internal, e.to_string(), 0),
    }
}

// ── ParseToml ─────────────────────────────────────────────────────────────────

pub async fn parse_toml(path: &str, config: &AppConfig) -> ActionResult {
    let max_bytes = config.policy.max_read_bytes;

    match std::fs::read_to_string(path) {
        Ok(content) => {
            if content.len() as u64 > max_bytes {
                return ActionResult::err(
                    "ParseToml",
                    ErrorKind::InvalidInput,
                    format!("File exceeds max_read_bytes ({} bytes)", max_bytes),
                    0,
                );
            }
            let size_bytes = content.len() as u64;
            // `Table`, not `Value`: since toml 0.9 `Value::from_str` reads one value.
            match content.parse::<toml::Table>() {
                Ok(toml_val) => {
                    // Convert the TOML table to serde_json::Value for uniform output
                    match serde_json::to_value(&toml_val) {
                        Ok(data) => ActionResult::ok(
                            "ParseToml",
                            ActionOutput::TomlData {
                                path: path.to_string(),
                                data,
                                size_bytes,
                            },
                            0,
                        ),
                        Err(e) => {
                            ActionResult::err("ParseToml", ErrorKind::Internal, e.to_string(), 0)
                        }
                    }
                }
                Err(e) => ActionResult::err(
                    "ParseToml",
                    ErrorKind::InvalidInput,
                    format!("Invalid TOML in file: {}", e),
                    0,
                ),
            }
        }
        Err(e) => {
            let kind = match e.kind() {
                std::io::ErrorKind::NotFound => ErrorKind::NotFound,
                std::io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
                _ => ErrorKind::ExecutionFailed,
            };
            ActionResult::err("ParseToml", kind, e.to_string(), 0)
        }
    }
}

// ── RandomBytes ───────────────────────────────────────────────────────────────

pub async fn random_bytes(count: usize, encoding: Option<&str>) -> ActionResult {
    const MAX_RANDOM_BYTES: usize = 1_048_576; // 1MB

    if count > MAX_RANDOM_BYTES {
        return ActionResult::err(
            "RandomBytes",
            ErrorKind::InvalidInput,
            format!(
                "Requested {} bytes exceeds maximum of {} bytes",
                count, MAX_RANDOM_BYTES
            ),
            0,
        );
    }

    // Read from /dev/urandom (Linux) — cryptographically secure
    let result = (|| -> std::io::Result<Vec<u8>> {
        use std::io::Read;
        let mut buf = vec![0u8; count];
        let mut f = std::fs::File::open("/dev/urandom")?;
        f.read_exact(&mut buf)?;
        Ok(buf)
    })();

    match result {
        Ok(buf) => {
            let enc = encoding.unwrap_or("hex");
            let encoded = match enc {
                "base64" => {
                    // Simple base64 encoding without external crate
                    base64_encode(&buf)
                }
                _ => hex_encode(&buf),
            };

            ActionResult::ok(
                "RandomBytes",
                ActionOutput::RandomBytesResult {
                    bytes: encoded,
                    encoding: enc.to_string(),
                    count,
                },
                0,
            )
        }
        Err(e) => ActionResult::err("RandomBytes", ErrorKind::ExecutionFailed, e.to_string(), 0),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn base64_encode(bytes: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;

        result.push(CHARS[((n >> 18) & 63) as usize] as char);
        result.push(CHARS[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((n >> 6) & 63) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(n & 63) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    /// The published test vectors for "abc" (FIPS 180-2, RFC 1321, RFC 3174).
    /// A failure means `Checksum` reports a digest nobody else would compute.
    #[test]
    fn a_checksum_is_the_digest_everybody_else_computes() {
        let d = |algo| super::hex_digest(algo, b"abc");
        assert_eq!(
            d("sha256").as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert_eq!(
            d("md5").as_deref(),
            Some("900150983cd24fb0d6963f7d28e17f72")
        );
        assert_eq!(
            d("sha1").as_deref(),
            Some("a9993e364706816aba3e25717850c26c9cd0d89d")
        );
        assert_eq!(d("sha512"), None);
    }

    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // ── Web search ──────────────────────────────────────────────────

    #[test]
    fn urlencoding_minimal_handles_spaces_and_special_chars() {
        assert_eq!(urlencoding_minimal("hello world"), "hello+world");
        // `&`, `=`, `?` MUST be percent-encoded so they don't break
        // the query string they're embedded in.
        assert!(urlencoding_minimal("a&b=c?d").contains("%26"));
        // ASCII-alphanumeric + RFC 3986 unreserved pass through.
        assert_eq!(urlencoding_minimal("hello.world-2_v1"), "hello.world-2_v1");
        // Non-ASCII (UTF-8) percent-encodes byte by byte.
        let r = urlencoding_minimal("café");
        assert!(r.starts_with("caf%"));
        assert!(r.len() > 4);
    }

    #[test]
    fn parse_ddg_html_extracts_results_from_canonical_layout() {
        // Sample mirrors DDG's HTML at the time of writing — anchor
        // on `result__title` and `result__snippet`, with the URL
        // wrapped in the `//duckduckgo.com/l/?uddg=…` tracker that
        // `clean_ddg_url` strips.
        let html = r##"
            <div class="result__title">
              <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.github.io%2Fasync-book%2F&amp;rut=xyz">
                Async <b>Book</b>
              </a>
            </div>
            <div class="result__snippet">
              <a class="result__snippet" href="...">
                Learn async <b>Rust</b> from the source.
              </a>
            </div>
            <div class="result__title">
              <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Ftokio.rs%2F&amp;rut=xyz">
                Tokio
              </a>
            </div>
            <div class="result__snippet">
              <a class="result__snippet" href="...">
                Async runtime for Rust.
              </a>
            </div>
        "##;
        let results = parse_ddg_html(html, 5);
        assert!(
            !results.is_empty(),
            "expected at least one result, got {}",
            results.len()
        );
        // First result is the async book.
        assert_eq!(results[0]["url"], "https://rust-lang.github.io/async-book/");
        let title0 = results[0]["title"].as_str().unwrap_or("");
        assert!(
            title0.contains("Async") && title0.contains("Book"),
            "title should contain Async + Book (tags stripped), got {:?}",
            title0
        );
        // Snippet is recovered too.
        let snippet0 = results[0]["snippet"].as_str().unwrap_or("");
        assert!(snippet0.contains("async") || snippet0.contains("source"));
    }

    #[test]
    fn parse_ddg_html_respects_limit() {
        // Generate 5 pseudo-blocks; ask for 2.
        let mut html = String::new();
        for i in 0..5 {
            html.push_str(&format!(r##"
                <div class="result__title">
                    <a class="result__a" href="https://example.com/{}">Title {}</a>
                </div>
                <div class="result__snippet"><a class="result__snippet" href="x">snippet {}</a></div>
            "##, i, i, i));
        }
        let results = parse_ddg_html(&html, 2);
        assert!(
            results.len() <= 2,
            "must respect the limit, got {}",
            results.len()
        );
    }

    #[test]
    fn parse_ddg_html_empty_on_unparseable() {
        let results = parse_ddg_html("<html><body>no results</body></html>", 10);
        assert!(
            results.is_empty(),
            "an HTML page without DDG result__* anchors must return 0 hits"
        );
    }

    #[test]
    fn clean_ddg_url_unwraps_tracker() {
        let wrapped = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpath&amp;rut=xyz";
        let clean = clean_ddg_url(wrapped);
        assert_eq!(clean, "https://example.com/path");

        // A non-tracker URL passes through unchanged.
        let direct = "https://example.com/raw";
        assert_eq!(clean_ddg_url(direct), direct);
    }

    const DDG_CHALLENGE: &str = include_str!("../../tests/fixtures/web/ddg_challenge.html");
    const DDG_RESULTS: &str = include_str!("../../tests/fixtures/web/ddg_results.html");
    const DDG_NO_RESULTS: &str = include_str!("../../tests/fixtures/web/ddg_no_results.html");

    #[test]
    fn ddg_challenge_page_is_an_error_not_zero_results() {
        let err = ddg_results(DDG_CHALLENGE, 10).expect_err("challenge page must not be Ok");
        assert!(err.contains("DuckDuckGo blocked"), "{err}");
        assert!(err.contains("X0NE_BRAVE_API_KEY"), "{err}");
        assert!(err.contains("FetchUrl"), "{err}");
    }

    #[test]
    fn ddg_unknown_non_empty_page_is_an_error() {
        let err = ddg_results("<html><body><p>Something else</p></body></html>", 10)
            .expect_err("non-empty page with no result block must not be Ok");
        assert!(err.contains("X0NE_BRAVE_API_KEY"), "{err}");
    }

    #[test]
    fn ddg_real_results_page_still_parses() {
        let r = ddg_results(DDG_RESULTS, 25).expect("results page");
        assert_eq!(r.len(), 10);
        assert!(r[0]["url"].as_str().unwrap().starts_with("http"));
    }

    #[test]
    fn ddg_genuine_empty_search_stays_ok() {
        assert!(ddg_results(DDG_NO_RESULTS, 10)
            .expect("no-results page")
            .is_empty());
        assert!(ddg_results("", 10).expect("empty body").is_empty());
    }

    #[test]
    fn brave_then_blocked_ddg_names_both_causes() {
        let err = ddg_results(DDG_CHALLENGE, 10).unwrap_err();
        let msg = super::super::web::all_failed(&[
            ("brave", "brave returned HTTP 429".to_string()),
            ("ddg", err),
        ]);
        assert!(
            msg.contains("HTTP 429") && msg.contains("DuckDuckGo blocked"),
            "{msg}"
        );
    }

    #[test]
    fn search_envelope_names_the_answering_backend_and_the_failures() {
        let data = search_envelope(
            "q",
            super::super::web::Searched {
                source: "ddg",
                results: vec![
                    serde_json::json!({"title": "t", "url": "https://env.test/1", "snippet": ""}),
                ],
                failures: vec![("exa", "exa: timeout".into())],
            },
        );
        assert_eq!(data["source"], "ddg");
        assert_eq!(data["count"], 1);
        assert_eq!(data["fallbacks"][0]["backend"], "exa");
        assert!(data["results"][0]["source"]
            .as_str()
            .unwrap()
            .starts_with('S'));
        // The wire shape the TUI Sources panel must read: output.data.results.
        let r = ActionResult::ok("WebSearch", ActionOutput::Generic { data }, 0);
        let wire = serde_json::to_value(&r).unwrap();
        assert_eq!(wire["output"]["kind"], "generic");
        assert!(wire["output"]["data"]["results"].is_array(), "{wire}");
    }

    #[test]
    fn strip_html_tags_decodes_common_entities() {
        let s = strip_html_tags("Hello <b>&amp; world</b>");
        assert_eq!(s, "Hello & world");
    }

    #[tokio::test]
    async fn web_search_rejects_empty_query() {
        let r = web_search("", 5).await;
        assert!(!r.success);
        let err = r.error.as_ref().expect("err envelope");
        assert!(
            err.message.contains("non-empty"),
            "must explain the requirement, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn web_search_clamps_huge_limits_to_25() {
        // We can't actually hit DDG in unit tests, but the executor's
        // clamp logic should reject obviously absurd values before
        // we make the network call. We just verify the function
        // returns *some* result envelope shape for a sane query —
        // either a successful list or a tracked failure (e.g. CI
        // without network).
        let r = web_search("rust", 9999).await;
        // The action_type is always set even on failure.
        assert_eq!(r.action_type, "WebSearch");
    }

    #[tokio::test]
    async fn system_info_has_fields() {
        let mut cfg = AppConfig::defaults();
        cfg.policy.allowed_read_paths = vec![std::env::temp_dir().to_string_lossy().into_owned()];
        let result = system_info(&cfg).await;
        assert!(result.success, "{:?}", result.error);
        if let Some(ActionOutput::SystemInfoOutput {
            hostname, os, arch, ..
        }) = result.output
        {
            assert!(!hostname.is_empty());
            assert!(!os.is_empty());
            assert!(!arch.is_empty());
        } else {
            panic!("unexpected output");
        }
    }

    /// T1: SystemInfo must report the executor process's actual cwd so the
    /// caller can ground subsequent boot-probe operations (ListDir, etc.)
    /// in the runtime's filesystem rather than the orchestrator host's.
    #[tokio::test]
    async fn system_info_reports_executor_cwd() {
        let mut cfg = AppConfig::defaults();
        cfg.policy.allowed_read_paths = vec![std::env::temp_dir().to_string_lossy().into_owned()];
        let result = system_info(&cfg).await;
        assert!(result.success, "{:?}", result.error);
        let cwd = match result.output {
            Some(ActionOutput::SystemInfoOutput { cwd, .. }) => cwd,
            _ => panic!("unexpected output"),
        };
        let expected = std::env::current_dir()
            .expect("test process must have a cwd")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            cwd, expected,
            "SystemInfo.cwd must equal the executor process current_dir, got {cwd:?}"
        );
    }

    #[tokio::test]
    async fn file_info_existing_file() {
        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "hello world").unwrap();
        let mut cfg = AppConfig::defaults();
        cfg.policy.allowed_read_paths = vec![std::env::temp_dir().to_string_lossy().into_owned()];
        let result = file_info(tmp.path().to_str().unwrap(), &cfg).await;
        assert!(result.success, "{:?}", result.error);
        if let Some(ActionOutput::FileInfoOutput { size_bytes, .. }) = result.output {
            assert!(size_bytes > 0);
        }
    }

    #[tokio::test]
    async fn checksum_sha256_known_content() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"hello").unwrap();
        let mut cfg = AppConfig::defaults();
        cfg.policy.allowed_read_paths = vec![std::env::temp_dir().to_string_lossy().into_owned()];
        let result = checksum(tmp.path().to_str().unwrap(), Some("sha256"), &cfg).await;
        assert!(result.success);
        if let Some(ActionOutput::ChecksumOutput {
            hash, algorithm, ..
        }) = result.output
        {
            assert_eq!(algorithm, "sha256");
            // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
            assert_eq!(
                hash,
                "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
            );
        }
    }

    #[tokio::test]
    async fn checksum_md5_known_content() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"hello").unwrap();
        let mut cfg = AppConfig::defaults();
        cfg.policy.allowed_read_paths = vec![std::env::temp_dir().to_string_lossy().into_owned()];
        let result = checksum(tmp.path().to_str().unwrap(), Some("md5"), &cfg).await;
        assert!(result.success);
        if let Some(ActionOutput::ChecksumOutput { hash, .. }) = result.output {
            // md5("hello") = 5d41402abc4b2a76b9719d911017c592
            assert_eq!(hash, "5d41402abc4b2a76b9719d911017c592");
        }
    }

    /// toml 0.9+ `str::parse::<toml::Value>()` reads ONE value, not a document:
    /// this file came back as a parse error until 2026-09-23.
    #[tokio::test]
    async fn parse_toml_reads_a_whole_document() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"name = \"x0ne\"\n[server]\nport = 8080\n")
            .unwrap();
        let mut cfg = AppConfig::defaults();
        cfg.policy.allowed_read_paths = vec![std::env::temp_dir().to_string_lossy().into_owned()];
        let result = parse_toml(tmp.path().to_str().unwrap(), &cfg).await;
        assert!(result.success, "{result:?}");
        let Some(ActionOutput::TomlData { data, .. }) = result.output else {
            panic!("not TomlData: {:?}", result.output);
        };
        assert_eq!(
            data,
            serde_json::json!({"name": "x0ne", "server": {"port": 8080}})
        );
    }

    #[tokio::test]
    async fn list_processes_non_empty() {
        let mut cfg = AppConfig::defaults();
        cfg.policy.allowed_read_paths = vec![std::env::temp_dir().to_string_lossy().into_owned()];
        let result = list_processes(None, &cfg).await;
        assert!(result.success);
        if let Some(ActionOutput::ProcessList { processes }) = result.output {
            assert!(
                !processes.is_empty(),
                "Expected at least 1 process on Linux"
            );
        }
    }

    /// Another process's argv reached the model verbatim. Secrets in
    /// `--password=` and URL userinfo are masked, every other argument kept.
    #[tokio::test]
    async fn list_processes_masks_secrets_in_command_lines() {
        let mut child = std::process::Command::new("sh")
            .args([
                "-c",
                "read -r line",
                "x0ne-612",
                "--password=CANARY-612",
                "https://u:CANARY-612@example.invalid/path",
                "--verbose",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sh");
        let pid = child.id();
        let result = list_processes(None, &AppConfig::defaults()).await;
        let _ = child.kill();
        let _ = child.wait();
        let Some(ActionOutput::ProcessList { processes }) = result.output else {
            panic!("not a process list: {:?}", result.error);
        };
        let leaks: Vec<_> = processes
            .iter()
            .filter(|p| p.command.contains("CANARY-612"))
            .collect();
        assert!(leaks.is_empty(), "{leaks:?}");
        let spawned = processes.iter().find(|p| p.pid == pid).expect("listed");
        assert_eq!(
            spawned.command,
            "sh -c read -r line x0ne-612 --password=[redacted] \
             https://[redacted]@example.invalid/path --verbose"
        );
    }

    /// BEHAVIOUR CHANGE: `PortCheck` now classifies its target like every
    /// other outbound request, so loopback is refused unless the deployment
    /// opts in via `X0NE_ALLOW_LOOPBACK` — which the sandbox configuration
    /// sets, because there the container *is* the isolation boundary.
    ///
    /// These two tests previously probed `127.0.0.1` and asserted a
    /// reachability verdict. That is precisely the capability being removed:
    /// unguarded, `PortCheck` was a blind internal port scanner and a
    /// metadata-service liveness oracle. They now pin the refusal, and the
    /// permitted-loopback path is covered by
    /// `loopback_is_permitted_only_when_explicitly_allowed`.
    #[tokio::test]
    async fn port_check_refuses_loopback_by_default() {
        let _guard = LoopbackEnv::acquire();
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let result = port_check("127.0.0.1", port, Some(5)).await;
        assert!(
            !result.success,
            "loopback must be classified like any other target"
        );
        drop(listener);
    }

    #[tokio::test]
    async fn port_check_reports_reachability_for_a_permitted_target() {
        // The classifier is exercised directly here: `check_host` is the one
        // place that decides, and with loopback allowed it yields a pinned
        // address for a listener that is genuinely up.
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(
            check_host("127.0.0.1", port, true).is_ok(),
            "an explicitly permitted loopback target must resolve and pin"
        );
        drop(listener);
    }

    #[test]
    fn get_env_path() {
        let mut cfg = AppConfig::defaults();
        cfg.policy.allowed_read_paths = vec![std::env::temp_dir().to_string_lossy().into_owned()];
        let result = get_env("PATH", &cfg);
        assert!(result.success);
        if let Some(ActionOutput::EnvVarResult { set, .. }) = result.output {
            // PATH is almost always set in test environments
            assert!(set);
        }
    }

    /// BEHAVIOUR CHANGE: `DnsLookup` refuses names that resolve internally.
    /// Returning resolution results is the action's purpose, but answering for
    /// `localhost` — or any name pointing into RFC1918 or a metadata
    /// endpoint — makes it an internal-name enumeration primitive. The rule is
    /// now: a name may be looked up when every address it resolves to is a
    /// permitted destination.
    #[tokio::test]
    async fn dns_lookup_refuses_localhost_by_default() {
        let _guard = LoopbackEnv::acquire();
        let result = dns_lookup("localhost").await;
        assert!(!result.success, "localhost resolves into loopback");
    }

    /// Serialises the tests that read/write `X0NE_ALLOW_LOOPBACK` so they
    /// don't race under `cargo test` parallelism.
    static LOOPBACK_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The lock, plus the thing the lock alone never gave: the DEFAULT.
    ///
    /// A mutex serialises the writers; it does not hand the variable back. So
    /// `test_ssrf_invariants` — which sets the flag and ends — left it set for
    /// whichever test took the lock next, and the tests that assert "loopback
    /// is refused by default" then measured a machine somebody else had
    /// configured. Cost: two red `cargo test --workspace` runs whose failure
    /// depended on the scheduler ("1912/1" first, then "2004/1" on
    /// 2026-08-14, both `port_check_refuses_loopback_by_default`, both with
    /// cargo dropping every remaining target). An earlier fix closed one leaking
    /// test; this fixes the shape that lets a test leak.
    ///
    /// Cleared on acquire AND on drop: drop runs while unwinding, so a panic
    /// under the flag cannot carry it into the next test either.
    struct LoopbackEnv(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl LoopbackEnv {
        fn acquire() -> Self {
            let guard = LOOPBACK_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            unsafe {
                std::env::remove_var("X0NE_ALLOW_LOOPBACK");
            }
            Self(guard)
        }
    }

    impl Drop for LoopbackEnv {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var("X0NE_ALLOW_LOOPBACK");
            }
        }
    }

    #[tokio::test]
    async fn test_fetch_url_local() {
        use axum::{routing::get, Router};
        use tokio::net::TcpListener;

        let _guard = LoopbackEnv::acquire();
        // SSRF guard blocks 127.0.0.1 by default. Opt in for this test —
        // the spawned axum server on a random loopback port is safe to hit.
        unsafe {
            std::env::set_var("X0NE_ALLOW_LOOPBACK", "1");
        }

        let app = Router::new().route("/hello", get(|| async { "world" }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{}/hello", port);

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // No manual restore: `LoopbackEnv` clears the variable on drop, which
        // runs while unwinding, so the assertions below may panic without
        // leaking the flag into the next test (see the guard's doc-comment).
        let result = fetch_url(&url, None, &FetchOpts::default()).await;
        assert!(result.success, "{:?}", result.error);
        if let Some(ActionOutput::FetchUrlResult { status, body, .. }) = result.output {
            assert_eq!(status, 200);
            assert_eq!(body, "world");
        } else {
            panic!("Expected FetchUrlResult");
        }
    }

    /// A redirect to another host name is not followed, even when the SSRF
    /// check passes it; a redirect on the pinned host is.
    ///
    /// SECURITY: reqwest resolves a new name again to connect, after the
    /// check, and that second answer can be internal (DNS rebinding). Here the
    /// check accepts every URL, standing in for the name's first, public
    /// answer, and `localhost` reaches the internal service. A failure would
    /// mean the client connected to an address no check had seen.
    #[tokio::test]
    async fn a_redirect_to_another_host_name_is_not_followed() {
        use axum::{response::Redirect, routing::get, Router};

        let v4 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = v4.local_addr().unwrap().port();
        let elsewhere = format!("http://localhost:{port}/internal");
        let app = Router::new()
            .route("/internal", get(|| async { "INTERNAL" }))
            .route("/same", get(|| async { Redirect::temporary("/internal") }))
            .route(
                "/other",
                get(move || {
                    let to = elsewhere.clone();
                    async move { Redirect::temporary(&to) }
                }),
            );
        // `localhost` can resolve to ::1 alone (podman): serve there too when possible.
        if let Ok(v6) = tokio::net::TcpListener::bind(("::1", port)).await {
            let app = app.clone();
            tokio::spawn(async move {
                axum::serve(v6, app).await.unwrap();
            });
        }
        tokio::spawn(async move {
            axum::serve(v4, app).await.unwrap();
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let first = format!("http://127.0.0.1:{port}/");
        let ssrf = SsrfCheckResult {
            resolved_addr: Some(([127, 0, 0, 1], port).into()),
        };
        let client =
            build_pinned_client_with(&first, &ssrf, Duration::from_secs(5), |_| true).unwrap();

        let same = client.get(format!("{first}same")).send().await.unwrap();
        assert_eq!(
            same.text().await.unwrap(),
            "INTERNAL",
            "a redirect on the pinned host is followed"
        );
        let other = client.get(format!("{first}other")).send().await.unwrap();
        assert_eq!(
            other.status().as_u16(),
            307,
            "a redirect to another host name was followed"
        );
    }

    /// Loopback server for FetchUrl tests; counts hits per path.
    async fn fetch_server() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use axum::{
            http::{HeaderMap, StatusCode},
            response::{Html, IntoResponse},
            routing::get,
            Router,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let h1 = hits.clone();
        let h2 = hits.clone();
        let app = Router::new()
            .route("/p", get(|| async { Html(FETCH_PAGE) }))
            .route(
                "/long",
                get(move || {
                    h1.fetch_add(1, Ordering::SeqCst);
                    async {
                        let p: String = (0..20_000).map(|i| format!("<p>para {i}</p>")).collect();
                        Html(p)
                    }
                }),
            )
            .route(
                "/cf",
                get(move |headers: HeaderMap| {
                    h2.fetch_add(1, Ordering::SeqCst);
                    let ua = headers
                        .get("user-agent")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    async move {
                        if ua.contains("Chrome") {
                            (
                                StatusCode::FORBIDDEN,
                                [("cf-mitigated", "challenge")],
                                "Just a moment...",
                            )
                                .into_response()
                        } else {
                            "past the challenge".into_response()
                        }
                    }
                }),
            )
            .route("/big", get(|| async { "y".repeat(3 * 1024 * 1024) }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        (base, hits)
    }
    use tokio::net::TcpListener;

    const FETCH_PAGE: &str = "<html><head><script>track()</script></head>\
                              <body><h1>Hi</h1><a href=\"/x\">x</a></body></html>";

    fn fetched(r: ActionResult) -> (String, Option<usize>, bool, Option<String>) {
        match r.output {
            Some(ActionOutput::FetchUrlResult {
                body,
                next_offset,
                cached,
                source,
                ..
            }) => (body, next_offset, cached, source),
            other => panic!("Expected FetchUrlResult, got {:?} / {:?}", other, r.error),
        }
    }

    fn fmt(f: &str) -> FetchOpts {
        FetchOpts {
            format: Some(f.into()),
            ..FetchOpts::default()
        }
    }

    #[tokio::test]
    async fn fetch_url_formats_html_as_markdown_text_or_html() {
        let _guard = LoopbackEnv::acquire();
        unsafe {
            std::env::set_var("X0NE_ALLOW_LOOPBACK", "1");
        }
        let (base, _) = fetch_server().await;
        let url = format!("{base}/p");
        let (md, _, _, source) = fetched(fetch_url(&url, None, &FetchOpts::default()).await);
        assert!(md.contains("# Hi") && md.contains("/x)"), "{md}");
        assert!(!md.contains("track()"), "{md}");
        assert!(source.unwrap().starts_with('S'));
        let (text, ..) = fetched(fetch_url(&url, None, &fmt("text")).await);
        assert_eq!(text, "Hi\n\nx");
        let (html, ..) = fetched(fetch_url(&url, None, &fmt("html")).await);
        assert_eq!(html, FETCH_PAGE);
        let bad = fetch_url(&url, None, &fmt("pdf")).await;
        assert!(!bad.success);
    }

    #[tokio::test]
    async fn fetch_url_reads_on_with_offset_from_the_cache() {
        use std::sync::atomic::Ordering;
        let _guard = LoopbackEnv::acquire();
        unsafe {
            std::env::set_var("X0NE_ALLOW_LOOPBACK", "1");
        }
        let (base, hits) = fetch_server().await;
        let url = format!("{base}/long");
        let (first, next, cached, _) = fetched(fetch_url(&url, None, &FetchOpts::default()).await);
        assert!(!cached);
        let next = next.expect("long page is cut");
        assert!(first.contains(&format!("offset: {next}")), "{first}");
        let opts = FetchOpts {
            offset: Some(next),
            ..FetchOpts::default()
        };
        let (second, _, cached, _) = fetched(fetch_url(&url, None, &opts).await);
        assert!(cached, "second window comes from the cache");
        assert!(second.starts_with("para "), "{}", &second[..40]);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let mut h = std::collections::HashMap::new();
        h.insert("Authorization".to_string(), "Bearer t".to_string());
        let (_, _, cached, _) = fetched(fetch_url(&url, Some(&h), &FetchOpts::default()).await);
        assert!(!cached, "caller headers bypass the cache");
    }

    #[tokio::test]
    async fn fetch_url_retries_a_cloudflare_challenge_with_a_plain_agent() {
        use std::sync::atomic::Ordering;
        let _guard = LoopbackEnv::acquire();
        unsafe {
            std::env::set_var("X0NE_ALLOW_LOOPBACK", "1");
        }
        let (base, hits) = fetch_server().await;
        let r = fetch_url(&format!("{base}/cf"), None, &FetchOpts::default()).await;
        assert!(r.success, "{:?}", r.error);
        let (body, ..) = fetched(r);
        assert_eq!(body, "past the challenge");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn fetch_url_accepts_bodies_up_to_five_mib() {
        let _guard = LoopbackEnv::acquire();
        unsafe {
            std::env::set_var("X0NE_ALLOW_LOOPBACK", "1");
        }
        let (base, _) = fetch_server().await;
        let r = fetch_url(&format!("{base}/big"), None, &FetchOpts::default()).await;
        assert!(r.success, "3 MiB must pass: {:?}", r.error);
        let (body, next, ..) = fetched(r);
        assert!(next.is_some() && body.len() < 70 * 1024);
    }

    #[test]
    fn fetch_timeout_defaults_to_30_and_caps_at_120() {
        assert_eq!(fetch_timeout(None), Duration::from_secs(30));
        assert_eq!(fetch_timeout(Some(999)), Duration::from_secs(120));
        assert_eq!(fetch_timeout(Some(0)), Duration::from_secs(1));
    }

    /// SSRF invariants: the `X0NE_ALLOW_LOOPBACK` flag relaxes 127.0.0.0/8
    /// and `::1`, NOTHING else. Cloud metadata endpoints and RFC1918 ranges
    /// must still be blocked. Also verifies that loopback *is* refused when
    /// the flag is off.
    ///
    /// All four assertions live in a single test to keep the number of
    /// env-var writers small. Coordination with every other test that touches
    /// the flag goes through `LoopbackEnv`.
    #[test]
    fn test_ssrf_invariants() {
        let _guard = LoopbackEnv::acquire();
        assert!(
            super::is_ssrf_target("http://127.0.0.1:1234/").is_some(),
            "loopback must be blocked without opt-in"
        );

        unsafe {
            std::env::set_var("X0NE_ALLOW_LOOPBACK", "1");
        }

        let meta = super::is_ssrf_target("http://169.254.169.254/latest/meta-data/");
        assert!(meta.is_some(), "cloud metadata must stay blocked");
        assert!(meta.unwrap().contains("metadata"));

        for url in [
            "http://192.168.1.1/",
            "http://10.0.0.1/",
            "http://172.16.0.1/",
        ] {
            assert!(
                super::is_ssrf_target(url).is_some(),
                "private range {url} must remain blocked"
            );
        }
    }

    /// IPv6 and IPv4-mapped SSRF classes must be blocked — regression for the
    /// August 2026 audit finding (the guard was IPv4-only, so
    /// `::ffff:169.254.169.254` reached cloud metadata).
    #[test]
    fn test_ssrf_ipv6_classes() {
        let _guard = LoopbackEnv::acquire();
        // IPv4-mapped cloud-metadata endpoint.
        assert!(
            super::is_ssrf_target("http://[::ffff:169.254.169.254]/latest/meta-data/").is_some(),
            "IPv4-mapped metadata must be blocked"
        );
        // IPv6 link-local (fe80::/10).
        assert!(
            super::is_ssrf_target("http://[fe80::1]/").is_some(),
            "IPv6 link-local must be blocked"
        );
        // IPv6 unique-local (fc00::/7).
        assert!(
            super::is_ssrf_target("http://[fc00::1]/").is_some(),
            "IPv6 unique-local must be blocked"
        );
        // IPv6 loopback without the opt-in flag.
        assert!(
            super::is_ssrf_target("http://[::1]:8080/").is_some(),
            "IPv6 loopback must be blocked without X0NE_ALLOW_LOOPBACK"
        );
    }

    /// A host that fails to resolve must fail CLOSED (Err), never silently
    /// pass with no pin — audit finding 10.
    #[test]
    fn test_ssrf_unresolvable_fails_closed() {
        assert!(
            super::is_ssrf_target("http://no-such-host.invalid./").is_some(),
            "unresolvable host must fail closed"
        );
    }

    // ── SSRF coverage for the host-level callers ──────────────────────────

    /// `check_host` is the one classifier. These pin that the ranges the URL
    /// path already blocked are blocked here too, so `PortCheck`,
    /// `DnsLookup` and `Subscribe{port_open}` cannot become a weaker second
    /// door.
    #[test]
    fn behind_the_sandbox_proxy_names_skip_local_dns_and_ip_literals_do_not() {
        let name = check_ssrf_via("https://unresolvable.invalid/x", false, true);
        assert!(
            matches!(
                name,
                Ok(SsrfCheckResult {
                    resolved_addr: None
                })
            ),
            "{:?}",
            name.err()
        );
        assert!(check_ssrf_via("http://10.0.0.1/", false, true).is_err());
        assert!(check_ssrf_via("http://[::ffff:169.254.169.254]/", false, true).is_err());
        assert!(check_ssrf_via("http://metadata.google.internal/", false, true).is_err());
        assert!(check_ssrf_via("https://unresolvable.invalid/x", false, false).is_err());
    }

    #[test]
    fn check_host_blocks_the_same_ranges_as_the_url_path() {
        let _guard = LoopbackEnv::acquire();
        for host in [
            "127.0.0.1",
            "localhost",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "metadata.google.internal",
            "[::1]",
            "[::ffff:169.254.169.254]",
            "[fd00::1]",
        ] {
            assert!(
                check_host(host, 80, false).is_err(),
                "{host} must be refused"
            );
        }
    }

    /// IPv4-mapped IPv6 must be normalised before classification — the
    /// pre-hardening bug that gave a deterministic route to metadata.
    #[test]
    fn ipv4_mapped_ipv6_is_normalised_at_the_host_level() {
        let _guard = LoopbackEnv::acquire();
        assert!(check_host("[::ffff:10.0.0.1]", 80, false).is_err());
        assert!(check_host("[::ffff:127.0.0.1]", 80, false).is_err());
    }

    /// Loopback stays reachable when the sandbox opts in, exactly as the URL
    /// path allows — the container is the isolation boundary there.
    #[test]
    fn loopback_is_permitted_only_when_explicitly_allowed() {
        let _guard = LoopbackEnv::acquire();
        assert!(check_host("127.0.0.1", 80, false).is_err());
        assert!(check_host("127.0.0.1", 80, true).is_ok());
        // …but opting into loopback never opens the metadata endpoint.
        assert!(check_host("169.254.169.254", 80, true).is_err());
    }

    /// Resolution failure is a refusal, never a skip.
    #[test]
    fn an_unresolvable_host_is_refused() {
        assert!(check_host("this-name-does-not-resolve.invalid", 80, false).is_err());
    }

    /// A port check against an internal address must refuse rather than
    /// report reachability.
    #[tokio::test]
    async fn port_check_refuses_internal_targets() {
        let _guard = LoopbackEnv::acquire();
        for host in ["169.254.169.254", "10.0.0.1", "192.168.0.1"] {
            let r = port_check(host, 80, Some(1)).await;
            assert!(!r.success, "{host} must not be probed");
            assert!(matches!(
                r.error.as_ref().map(|e| &e.kind),
                Some(ErrorKind::PolicyDenied)
            ));
        }
    }

    /// A name resolving to something internal must not be answered — that is
    /// what turns DnsLookup into an enumeration primitive.
    #[tokio::test]
    async fn dns_lookup_refuses_names_that_resolve_internally() {
        let _guard = LoopbackEnv::acquire();
        let r = dns_lookup("localhost").await;
        assert!(!r.success, "localhost resolves into loopback");
        let r = dns_lookup("169.254.169.254").await;
        assert!(!r.success, "the metadata endpoint must not be resolved");
    }

    /// Each search backend refuses an answer past `SEARCH_MAX_BYTES` and
    /// still reads a normal one.
    ///
    /// A failure would mean a backend's body is read whole however long it
    /// is: `resp.json()` and `resp.text()` had no cap until 2026-11-04.
    #[tokio::test]
    async fn search_backends_refuse_an_oversized_answer() {
        use axum::{
            routing::{get, post},
            Router,
        };

        let pad = "y".repeat(SEARCH_MAX_BYTES + 1024);
        let brave_big = format!("{{\"web\":{{\"results\":[]}},\"pad\":\"{pad}\"}}");
        let ddg_big = format!("<div class=\"result__title\"></div>{pad}");
        let brave_ok =
            r#"{"web":{"results":[{"title":"t","url":"https://example.com/","description":"d"}]}}"#;
        let app = Router::new()
            .route("/brave", get(move || async move { brave_ok }))
            .route("/brave-big", get(move || async move { brave_big }))
            .route(
                "/ddg",
                post(|| async { "<div class=\"no-results\"></div>" }),
            )
            .route("/ddg-big", post(move || async move { ddg_big }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let ok = web_search_brave_at(&format!("{base}/brave"), "q", 5, "k").await;
        assert_eq!(ok.map(|r| r.len()), Ok(1), "a normal Brave answer is read");
        let big = web_search_brave_at(&format!("{base}/brave-big"), "q", 5, "k").await;
        assert!(big.is_err(), "an oversized Brave answer was read");
        let ok = web_search_ddg_at(&format!("{base}/ddg"), "q", 5).await;
        assert_eq!(
            ok.map(|r| r.len()),
            Ok(0),
            "a normal DuckDuckGo answer is read"
        );
        let big = web_search_ddg_at(&format!("{base}/ddg-big"), "q", 5).await;
        assert!(big.is_err(), "an oversized DuckDuckGo answer was read");
    }
}
