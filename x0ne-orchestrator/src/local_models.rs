//! Local model servers: Ollama, llama.cpp's `llama-server`, LM Studio.
//!
//! A preset names a server's documented default address; discovery asks it
//! which models it serves. Discovery runs only on a user's request
//! (`/model list`, `/setup`, `/provider llamacpp` without a model), only
//! towards a loopback address, with a short timeout and no redirect, proxy
//! or retry. Nothing here runs at startup or in the background.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::config::OpenAICompatConfig;
use crate::redact::url_for_display;

/// Where a server lists its models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListApi {
    /// `GET /api/tags`, answering `{"models":[{"name":…}]}`.
    Ollama,
    /// `GET /v1/models`, answering `{"data":[{"id":…}]}`.
    OpenAi,
}

/// A local server at its default address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    /// What `/provider` takes, and the saved profile's name.
    pub name: &'static str,
    pub label: &'static str,
    /// Root URL, without `/v1`.
    pub root: &'static str,
    pub api: ListApi,
}

// Defaults read 2026-10-27, each on 127.0.0.1 rather than `localhost`:
// Ollama: "binds 127.0.0.1 port 11434 by default" (ollama docs/faq.mdx).
// llama-server: --host 127.0.0.1, --port 9931 (llama.cpp tools/server/README.md).
// LM Studio: base_url "http://localhost:1234/v1" (lmstudio.ai/docs/developer/openai-compat).
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "ollama",
        label: "Ollama",
        root: "http://127.0.0.1:11434",
        api: ListApi::Ollama,
    },
    Preset {
        name: "llamacpp",
        label: "llama.cpp server",
        root: "http://127.0.0.1:9931",
        api: ListApi::OpenAi,
    },
    LLAMACPP_BEFORE_9931,
    Preset {
        name: "lmstudio",
        label: "LM Studio",
        root: "http://127.0.0.1:1234",
        api: ListApi::OpenAi,
    },
];

/// llama-server's default port was 8080 until ggml-org/llama.cpp b42b7e6d30
/// (2026-10-09, "server: use port 9931 by default"); builds from before
/// still listen there.
pub const LLAMACPP_BEFORE_9931: Preset = Preset {
    name: "llamacpp",
    label: "llama.cpp server (port before 2026-10-09)",
    root: "http://127.0.0.1:8080",
    api: ListApi::OpenAi,
};

/// How long one server may take to answer a listing.
pub const TIMEOUT: Duration = Duration::from_millis(1500);

/// A listing larger than this is not a model list.
const MAX_BODY: usize = 1 << 20;
const MAX_NAME_CHARS: usize = 200;

/// The models one server listed, and how many names were left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub models: Vec<String>,
    /// Names refused by `usable_name`: empty, overlong, or able to move the
    /// terminal's cursor or reorder its text.
    pub skipped: usize,
}

/// A preset that answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub preset: Preset,
    pub models: Vec<String>,
    pub skipped: usize,
}

/// `root` checked as a loopback URL, credentials refused, a trailing `/v1`
/// dropped. `localhost` becomes 127.0.0.1: an address skips the resolver, a
/// name does not. Called before any socket is opened.
pub fn loopback_root(root: &str) -> Result<reqwest::Url> {
    let shown = url_for_display(root);
    let mut url =
        reqwest::Url::parse(root.trim()).with_context(|| format!("{shown}: not a URL"))?;
    let host = url.host_str().unwrap_or("");
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = match bare.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback(),
        Ok(IpAddr::V6(ip)) => ip.to_canonical().is_loopback(),
        Err(_) => bare.trim_end_matches('.').eq_ignore_ascii_case("localhost"),
    };
    if !matches!(url.scheme(), "http" | "https") || !loopback {
        bail!("{shown} is not a loopback address: local discovery asks 127.0.0.1 or ::1 only");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("{shown}: credentials in the URL are refused");
    }
    if bare.eq_ignore_ascii_case("localhost") || bare.eq_ignore_ascii_case("localhost.") {
        url.set_host(Some("127.0.0.1"))
            .map_err(|e| anyhow!("{shown}: {e}"))?;
    }
    let path = url
        .path()
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_string();
    url.set_path(&format!("{path}/"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// A name that can go into a config and onto a terminal as it is.
fn usable_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && crate::text_safety::sanitize(name) == name
}

/// Ask the server at `root` for its models. Refuses a non-loopback `root`
/// before connecting; follows no redirect, uses no proxy, reads at most
/// 1 MiB, and gives up after `timeout`.
pub async fn list_models(root: &str, api: ListApi, timeout: Duration) -> Result<Listing> {
    let base = loopback_root(root)?;
    let url = base
        .join(match api {
            ListApi::Ollama => "api/tags",
            ListApi::OpenAi => "v1/models",
        })
        .map_err(|e| anyhow!("{e}"))?;
    let shown = url_for_display(base.as_str());
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout)
        .timeout(timeout)
        .build()?;
    let mut resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("nothing answers at {shown}"))?;
    if !resp.status().is_success() {
        bail!("{shown} answered {}", resp.status());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > MAX_BODY {
            bail!("{shown}: a model list larger than 1 MiB is refused");
        }
        body.extend_from_slice(&chunk);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&body).with_context(|| format!("{shown}: not JSON"))?;
    let (list, field) = match api {
        ListApi::Ollama => (&value["models"], "name"),
        ListApi::OpenAi => (&value["data"], "id"),
    };
    let list = list
        .as_array()
        .ok_or_else(|| anyhow!("{shown}: no model list in the answer"))?;
    let mut listing = Listing::default();
    for name in list.iter().map(|m| m[field].as_str().unwrap_or("")) {
        if usable_name(name) {
            listing.models.push(name.to_string());
        } else {
            listing.skipped += 1;
        }
    }
    listing.models.sort();
    listing.models.dedup();
    Ok(listing)
}

/// Ask every preset at once; the ones that answered, in preset order. A
/// second preset under the same name (llama.cpp's old port) is asked too,
/// and kept only when the first did not answer.
pub async fn discover(presets: &[Preset], timeout: Duration) -> Vec<Found> {
    let asks = presets.iter().map(|p| list_models(p.root, p.api, timeout));
    let answers = futures::future::join_all(asks).await;
    let mut found: Vec<Found> = Vec::new();
    for (preset, answer) in presets.iter().zip(answers) {
        let Ok(listing) = answer else { continue };
        if found.iter().any(|f| f.preset.name == preset.name) {
            continue;
        }
        found.push(Found {
            preset: *preset,
            models: listing.models,
            skipped: listing.skipped,
        });
    }
    found
}

/// The preset a `/provider` argument names. Ollama keeps its native
/// provider, so it is not one of them.
pub fn preset_for(typed: &str) -> Option<&'static Preset> {
    let name = match typed.to_ascii_lowercase().as_str() {
        "llamacpp" | "llama.cpp" | "llama-cpp" | "llama-server" => "llamacpp",
        "lmstudio" | "lm-studio" | "lm_studio" => "lmstudio",
        _ => return None,
    };
    PRESETS.iter().find(|p| p.name == name)
}

/// The openai-compat profile for `preset` and `model`. It starts from the
/// defaults, so no key of the slot it replaces goes to the local server.
pub fn profile(preset: &Preset, model: &str) -> OpenAICompatConfig {
    OpenAICompatConfig {
        name: Some(preset.name.to_string()),
        base_url: preset.root.to_string(),
        model: model.to_string(),
        ..OpenAICompatConfig::default()
    }
}

/// One line per server that answered, for `/model list` and `/setup`. Names
/// are already `usable_name`; the line is bounded all the same.
pub fn describe(found: &Found) -> String {
    let host = found.preset.root.trim_start_matches("http://");
    let models = if found.models.is_empty() {
        "no model loaded".to_string()
    } else {
        found.models.join(", ")
    };
    let skipped = if found.skipped > 0 {
        format!(" ({} unprintable name(s) left out)", found.skipped)
    } else {
        String::new()
    };
    crate::text_safety::snippet(
        &format!("{} at {host}: {models}{skipped}", found.preset.label),
        400,
    )
}

/// What `/provider <preset> [model]` activates: the profile, or the lines
/// to print instead. Without a model, `candidates` (the preset's addresses)
/// are asked and a server's only model taken.
pub async fn choose(
    preset: &Preset,
    model: Option<&str>,
    candidates: &[Preset],
) -> std::result::Result<OpenAICompatConfig, Vec<String>> {
    if let Some(model) = model {
        return Ok(profile(preset, model));
    }
    let Some(found) = discover(candidates, TIMEOUT).await.into_iter().next() else {
        return Err(vec![format!(
            "nothing answers at {}: start {}, or name its address with \
             /provider openai-compat <url> <model>",
            preset.root, preset.label
        )]);
    };
    match found.models.as_slice() {
        [only] => Ok(profile(&found.preset, only)),
        // None loaded, or several: the report says which and how to pick.
        _ => Err(report(std::slice::from_ref(&found))),
    }
}

/// What `/model list` and `/setup` print after a discovery: each server
/// that answered and the command that switches to it, or that none did.
pub fn report(found: &[Found]) -> Vec<String> {
    if found.is_empty() {
        return vec![
            "no local server answered on 127.0.0.1 (Ollama :11434, llama.cpp :9931 or :8080, \
             LM Studio :1234)"
                .to_string(),
        ];
    }
    let mut lines = Vec::new();
    for f in found {
        lines.push(describe(f));
        lines.push(match (f.preset.api, f.preset.root) {
            (ListApi::Ollama, _) => "  switch: /provider ollama, then /model <name>".to_string(),
            (_, root) if root == LLAMACPP_BEFORE_9931.root => format!(
                "  switch: /provider openai-compat {root} <model> \"\" {}",
                f.preset.name
            ),
            _ => format!("  switch: /provider {} <model>", f.preset.name),
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A loopback server that answers every request with `head` + `body`
    /// and records each request line.
    async fn serve(
        status: &'static str,
        extra: String,
        body: String,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let mut n = 0;
                while let Ok(k) = sock.read(&mut buf[n..]).await {
                    if k == 0 {
                        break;
                    }
                    n += k;
                    if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                log.lock()
                    .unwrap()
                    .push(text.lines().next().unwrap_or("").to_string());
                let head = format!(
                    "HTTP/1.1 {status}\r\n{extra}content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (root, seen)
    }

    const QUICK: Duration = Duration::from_millis(1500);

    #[tokio::test]
    async fn a_llama_server_lists_its_models_on_v1_models() {
        let body = r#"{"object":"list","data":[{"id":"qwen3-8b"},{"id":"gemma-3"}]}"#;
        let (root, seen) = serve("200 OK", String::new(), body.into()).await;
        let found = list_models(&root, ListApi::OpenAi, QUICK).await.unwrap();
        assert_eq!(found.models, vec!["gemma-3", "qwen3-8b"]);
        assert_eq!(*seen.lock().unwrap(), vec!["GET /v1/models HTTP/1.1"]);
    }

    #[tokio::test]
    async fn ollama_is_asked_on_api_tags() {
        let body = r#"{"models":[{"name":"llama3.2:3b"}]}"#;
        let (root, seen) = serve("200 OK", String::new(), body.into()).await;
        let found = list_models(&root, ListApi::Ollama, QUICK).await.unwrap();
        assert_eq!(found.models, vec!["llama3.2:3b"]);
        assert_eq!(*seen.lock().unwrap(), vec!["GET /api/tags HTTP/1.1"]);
    }

    /// The address is checked before any socket is opened: a LAN host, a
    /// public name, or a name that merely starts like a loopback address.
    #[tokio::test]
    async fn a_non_loopback_address_is_refused_without_a_connection() {
        for root in [
            "http://172.16.1.20:11434",
            "http://198.51.100.5:9931",
            "https://llm.example.com",
            "http://127.0.0.1.nip.io:1234",
            "http://[2001:db8::1]:1234",
            "file:///etc/passwd",
        ] {
            let t = std::time::Instant::now();
            let err = list_models(root, ListApi::OpenAi, QUICK).await.unwrap_err();
            assert!(format!("{err:#}").contains("loopback"), "{root}: {err:#}");
            assert!(
                t.elapsed() < Duration::from_millis(100),
                "{root} was contacted"
            );
        }
    }

    #[test]
    fn localhost_is_asked_as_127_0_0_1_and_v1_is_dropped() {
        let u = loopback_root("http://localhost:1234/v1").unwrap();
        assert_eq!(u.as_str(), "http://127.0.0.1:1234/");
        assert!(loopback_root("http://[::1]:9931").is_ok());
        assert!(loopback_root("http://127.0.0.2:9931").is_ok());
        assert!(loopback_root("http://user:pw@127.0.0.1:9931").is_err());
    }

    /// A redirect could send the request off the machine: not followed.
    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        let (target, hits) = serve("200 OK", String::new(), r#"{"data":[]}"#.into()).await;
        let (root, _) = serve(
            "302 Found",
            format!("location: {target}/v1/models\r\n"),
            String::new(),
        )
        .await;
        assert!(list_models(&root, ListApi::OpenAi, QUICK).await.is_err());
        assert!(hits.lock().unwrap().is_empty(), "the redirect was followed");
    }

    #[tokio::test]
    async fn a_silent_server_costs_the_timeout_and_no_more() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        let t = std::time::Instant::now();
        assert!(
            list_models(&root, ListApi::OpenAi, Duration::from_millis(300))
                .await
                .is_err()
        );
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    }

    /// A name goes into the config and onto the terminal: one that carries
    /// an escape sequence or a bidi override is left out, and counted.
    #[tokio::test]
    async fn a_model_name_that_could_redraw_the_terminal_is_skipped() {
        let body =
            r#"{"data":[{"id":"ok-model"},{"id":"\u001b[2Jevil"},{"id":"a\u202eb"},{"id":""}]}"#;
        let (root, _) = serve("200 OK", String::new(), body.into()).await;
        let found = list_models(&root, ListApi::OpenAi, QUICK).await.unwrap();
        assert_eq!(found.models, vec!["ok-model"]);
        assert_eq!(found.skipped, 3);
    }

    #[tokio::test]
    async fn an_oversized_answer_is_refused() {
        let body = format!(r#"{{"data":[{{"id":"{}"}}]}}"#, "a".repeat(2 << 20));
        let (root, _) = serve("200 OK", String::new(), body).await;
        assert!(list_models(&root, ListApi::OpenAi, QUICK).await.is_err());
    }

    /// The defaults each project documents (read 2026-10-27), on 127.0.0.1.
    #[test]
    fn the_presets_are_the_documented_defaults() {
        let by = |name: &str| PRESETS.iter().find(|p| p.name == name).unwrap().root;
        assert_eq!(by("ollama"), "http://127.0.0.1:11434");
        assert_eq!(by("llamacpp"), "http://127.0.0.1:9931");
        assert_eq!(by("lmstudio"), "http://127.0.0.1:1234");
        assert_eq!(LLAMACPP_BEFORE_9931.root, "http://127.0.0.1:8080");
        for p in PRESETS {
            assert!(loopback_root(p.root).is_ok(), "{}", p.name);
        }
    }

    #[test]
    fn provider_names_resolve_to_presets() {
        for (typed, name) in [
            ("llamacpp", "llamacpp"),
            ("llama.cpp", "llamacpp"),
            ("llama-server", "llamacpp"),
            ("lmstudio", "lmstudio"),
            ("lm-studio", "lmstudio"),
        ] {
            assert_eq!(preset_for(typed).map(|p| p.name), Some(name), "{typed}");
        }
        assert!(
            preset_for("ollama").is_none(),
            "ollama keeps its native provider"
        );
        assert!(preset_for("groq").is_none());
    }

    /// A preset profile starts empty: no key from the slot it replaces.
    #[test]
    fn a_preset_profile_carries_no_key() {
        let p = profile(preset_for("lmstudio").unwrap(), "qwen3-8b");
        assert_eq!(p.name.as_deref(), Some("lmstudio"));
        assert_eq!(p.base_url, "http://127.0.0.1:1234");
        assert_eq!(p.model, "qwen3-8b");
        assert!(p.api_key.is_none() && p.api_key_env.is_none());
    }

    #[test]
    fn the_report_names_the_command_that_switches() {
        let found = |preset: Preset, models: &[&str]| Found {
            preset,
            models: models.iter().map(|m| m.to_string()).collect(),
            skipped: 0,
        };
        let lines = report(&[
            found(*preset_for("lmstudio").unwrap(), &["qwen3-8b"]),
            found(PRESETS[0], &[]),
            found(LLAMACPP_BEFORE_9931, &["m"]),
        ]);
        assert_eq!(lines[0], "LM Studio at 127.0.0.1:1234: qwen3-8b");
        assert_eq!(lines[1], "  switch: /provider lmstudio <model>");
        assert_eq!(lines[2], "Ollama at 127.0.0.1:11434: no model loaded");
        assert_eq!(lines[3], "  switch: /provider ollama, then /model <name>");
        assert!(
            lines[5].contains("openai-compat http://127.0.0.1:8080"),
            "{lines:?}"
        );
        assert!(report(&[])[0].starts_with("no local server answered"));
    }

    fn leak(root: String) -> &'static str {
        Box::leak(root.into_boxed_str())
    }

    #[tokio::test]
    async fn provider_llamacpp_takes_the_only_model_the_server_lists() {
        let (root, _) = serve(
            "200 OK",
            String::new(),
            r#"{"data":[{"id":"qwen3-8b"}]}"#.into(),
        )
        .await;
        let at = Preset {
            root: leak(root.clone()),
            ..*preset_for("llamacpp").unwrap()
        };
        let p = choose(preset_for("llamacpp").unwrap(), None, &[at])
            .await
            .unwrap();
        assert_eq!(
            (p.base_url.as_str(), p.model.as_str()),
            (root.as_str(), "qwen3-8b")
        );
        assert_eq!(p.name.as_deref(), Some("llamacpp"));
    }

    #[tokio::test]
    async fn provider_llamacpp_lists_several_models_and_switches_to_none() {
        let (root, _) = serve(
            "200 OK",
            String::new(),
            r#"{"data":[{"id":"a"},{"id":"b"}]}"#.into(),
        )
        .await;
        let at = Preset {
            root: leak(root),
            ..*preset_for("llamacpp").unwrap()
        };
        let lines = choose(preset_for("llamacpp").unwrap(), None, &[at])
            .await
            .unwrap_err();
        assert!(lines[0].ends_with(": a, b"), "{lines:?}");
        assert_eq!(lines[1], "  switch: /provider llamacpp <model>");
    }

    #[tokio::test]
    async fn provider_lmstudio_with_a_model_asks_nothing() {
        let (root, seen) = serve("200 OK", String::new(), r#"{"data":[]}"#.into()).await;
        let at = Preset {
            root: leak(root),
            ..*preset_for("lmstudio").unwrap()
        };
        let p = choose(preset_for("lmstudio").unwrap(), Some("m"), &[at])
            .await
            .unwrap();
        assert_eq!(
            (p.base_url.as_str(), p.model.as_str()),
            ("http://127.0.0.1:1234", "m")
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "a named model needs no discovery"
        );
    }

    #[tokio::test]
    async fn provider_llamacpp_with_no_server_says_where_it_looked() {
        let closed = Preset {
            root: "http://127.0.0.1:9",
            ..*preset_for("llamacpp").unwrap()
        };
        let lines = choose(preset_for("llamacpp").unwrap(), None, &[closed])
            .await
            .unwrap_err();
        assert!(
            lines[0].contains("nothing answers at http://127.0.0.1:9931"),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn discovery_reports_what_answered_and_skips_the_rest() {
        let (root, _) = serve("200 OK", String::new(), r#"{"data":[{"id":"m"}]}"#.into()).await;
        let listening = Preset {
            name: "llamacpp",
            label: "llama.cpp server",
            root: Box::leak(root.into_boxed_str()),
            api: ListApi::OpenAi,
        };
        let closed = Preset {
            name: "lmstudio",
            label: "LM Studio",
            root: "http://127.0.0.1:9",
            api: ListApi::OpenAi,
        };
        let found = discover(&[listening, closed], QUICK).await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].preset.name, "llamacpp");
        assert_eq!(found[0].models, vec!["m"]);
    }
}
