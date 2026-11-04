//! Key-first model setup: one question, the vendor read from the key, a real
//! test call before "ready", and a key never sent to a vendor that did not
//! issue it (2026-10-06: an sk-ant- key was sent to api.openai.com).

use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::time::Duration;

pub const ANTHROPIC_BASE: &str = "https://api.anthropic.com";
pub const OPENAI_BASE: &str = "https://api.openai.com";
pub const ANTHROPIC_DEFAULT_MODEL: &str = "claude-haiku-4-5";

/// The question asked when no usable model is configured.
pub const QUESTION: &str =
    "Paste your API key (Anthropic or OpenAI), or type ollama or x0ne-engine:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Anthropic,
    OpenAi,
    Ollama,
    /// The relay of `x0ne-engine cloud up`: no key to paste, it is read from
    /// the relay's file before every call (`engine_local`).
    X0neEngine,
}

impl Vendor {
    pub fn provider(self) -> &'static str {
        match self {
            Vendor::Anthropic => "anthropic",
            Vendor::OpenAi => "openai",
            Vendor::Ollama => "ollama",
            Vendor::X0neEngine => "x0ne-engine",
        }
    }
    pub fn key_var(self) -> Option<&'static str> {
        match self {
            Vendor::Anthropic => Some("ANTHROPIC_API_KEY"),
            Vendor::OpenAi => Some("OPENAI_API_KEY"),
            Vendor::Ollama | Vendor::X0neEngine => None,
        }
    }
    pub fn default_model(self) -> String {
        match self {
            Vendor::Anthropic => ANTHROPIC_DEFAULT_MODEL.into(),
            Vendor::OpenAi => crate::config::OpenAIConfig::default().model,
            Vendor::Ollama => crate::config::OllamaConfig::default().model,
            Vendor::X0neEngine => String::new(),
        }
    }
}

/// The vendor that issued a key, read from its format in
/// [`crate::model_keys::KEYS`].
pub fn vendor_of_key(key: &str) -> Option<Vendor> {
    [Vendor::Anthropic, Vendor::OpenAi].into_iter().find(|v| {
        v.key_var()
            .and_then(crate::model_keys::row)
            .is_some_and(|r| r.takes(key))
    })
}

/// "…wxyz": only the last four characters. One implementation with the
/// runtime's refusals (`x0ne_actions::model_keys::mask`).
pub use x0ne_actions::model_keys::mask as mask_key;

/// API keys found in the environment.
#[derive(Default, Clone)]
pub struct KeyEnv {
    pub anthropic: Option<String>,
    pub openai: Option<String>,
    pub compat: Option<String>,
}

impl KeyEnv {
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Self {
            anthropic: get("ANTHROPIC_API_KEY"),
            openai: get("OPENAI_API_KEY"),
            compat: get("X0NE_OPENAI_COMPAT_KEY"),
        }
    }
}

/// The vendor and key to use, read from the keys present.
pub fn detect_from_keys(env: &KeyEnv) -> Option<(Vendor, String)> {
    let set = |v: &Option<String>| {
        v.as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    if let Some(k) = set(&env.anthropic) {
        return Some((Vendor::Anthropic, k));
    }
    for k in [set(&env.openai), set(&env.compat)].into_iter().flatten() {
        if vendor_of_key(&k) == Some(Vendor::Anthropic) {
            return Some((Vendor::Anthropic, k));
        }
    }
    set(&env.openai).map(|k| (Vendor::OpenAi, k))
}

/// What startup does when no provider was chosen by anyone.
#[derive(Debug, PartialEq, Eq)]
pub enum Start {
    /// Use this provider; `key` is the key detection found, if any.
    Use {
        provider: &'static str,
        key: Option<String>,
    },
    /// Keep the configured provider (someone chose it).
    Configured,
    /// Nothing usable: ask the one question.
    Ask,
}

pub fn plan_start(explicit: bool, keys: &KeyEnv, ollama_up: bool) -> Start {
    if explicit {
        return Start::Configured;
    }
    if let Some((v, k)) = detect_from_keys(keys) {
        return Start::Use {
            provider: v.provider(),
            key: Some(k),
        };
    }
    if ollama_up {
        return Start::Use {
            provider: "ollama",
            key: None,
        };
    }
    Start::Ask
}

/// Where each vendor is reached. Tests point these at a loopback server.
#[derive(Clone)]
pub struct Endpoints {
    pub anthropic: String,
    pub openai: String,
    pub ollama: String,
    pub x0ne_engine: crate::config::X0neEngineConfig,
}

impl Endpoints {
    pub fn real(ollama: String) -> Self {
        Self {
            anthropic: ANTHROPIC_BASE.into(),
            openai: OPENAI_BASE.into(),
            ollama,
            x0ne_engine: Default::default(),
        }
    }
}

fn client(base: &str, secs: u64) -> Result<reqwest::Client> {
    crate::api::client_for(base)
        .timeout(Duration::from_secs(secs))
        .build()
        .context("cannot build an HTTP client")
}

fn error_text(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let msg = v["error"]["message"]
        .as_str()
        .or_else(|| v["error"].as_str())
        .unwrap_or(body);
    msg.chars().take(200).collect()
}

/// One real, minimal call: proves the key, the endpoint and the model.
pub async fn probe(vendor: Vendor, base: &str, key: Option<&str>, model: &str) -> Result<()> {
    let base = base.trim_end_matches('/').trim_end_matches("/v1");
    let ping = serde_json::json!([{"role": "user", "content": "Reply with OK."}]);
    let req = match vendor {
        Vendor::Anthropic => {
            let key = key.ok_or_else(|| anyhow!("no key"))?;
            crate::model_keys::check(key, &crate::model_keys::Source::Given, "anthropic", base)?;
            client(base, 30)?
                .post(format!("{base}/v1/messages"))
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
                .json(&serde_json::json!({"model": model, "max_tokens": 5, "messages": ping}))
        }
        Vendor::OpenAi => {
            let key = key.ok_or_else(|| anyhow!("no key"))?;
            crate::model_keys::check(key, &crate::model_keys::Source::Given, "openai", base)?;
            client(base, 30)?
                .post(format!("{base}/v1/chat/completions"))
                .bearer_auth(key)
                .json(&serde_json::json!({"model": model, "messages": ping}))
        }
        Vendor::Ollama => client(base, 120)?.post(format!("{base}/api/chat")).json(
            &serde_json::json!({"model": model, "messages": ping, "stream": false,
                                "options": {"num_predict": 5}}),
        ),
        Vendor::X0neEngine => {
            bail!("x0ne-engine is checked through its status route, never with a prompt")
        }
    };
    let resp = req.send().await.with_context(|| {
        format!(
            "{} did not answer at {}",
            vendor.provider(),
            crate::redact::url_for_display(base)
        )
    })?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "{} answered {} for model {model}: {}",
            vendor.provider(),
            status.as_u16(),
            error_text(&body)
        );
    }
    Ok(())
}

/// Models pulled on an Ollama server.
pub async fn ollama_models(base: &str) -> Result<Vec<String>> {
    let base = base.trim_end_matches('/');
    let body: serde_json::Value = client(base, 3)?
        .get(format!("{base}/api/tags"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut names: Vec<String> = body["models"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    Ok(names)
}

/// Outcome of a successful quickstart. No `Debug`: it carries the key.
pub struct Ready {
    pub vendor: Vendor,
    pub model: String,
    pub masked: String,
    pub key: Option<String>,
}

/// The answer to [`QUESTION`]: detect, test for real, then save. Nothing is
/// written unless the test call succeeded. `x0ne_dir` is `~/.x0ne`.
pub async fn quickstart(answer: &str, ep: &Endpoints, x0ne_dir: &Path) -> Result<Ready> {
    let answer = answer.trim();
    if crate::engine_local::is_engine_provider(&answer.to_ascii_lowercase()) {
        // Status only: no prompt, no key to save, no model pinned.
        let engine = crate::engine_local::EngineBackend::from_config(&ep.x0ne_engine)?;
        engine
            .preflight()
            .await
            .map_err(|e| anyhow!("{e:#} Nothing was saved."))?;
        persist_choice(&x0ne_dir.join("config.toml"), Vendor::X0neEngine, "", None)?;
        return Ok(Ready {
            vendor: Vendor::X0neEngine,
            model: String::new(),
            masked: String::new(),
            key: None,
        });
    }
    if answer.eq_ignore_ascii_case("ollama") {
        let models = ollama_models(&ep.ollama).await.with_context(|| {
            format!(
                "Ollama does not answer at {} (start it: ollama serve)",
                ep.ollama
            )
        })?;
        let wanted = Vendor::Ollama.default_model();
        let tagged = format!("{wanted}:");
        let model = models
            .iter()
            .find(|m| **m == wanted || m.starts_with(&tagged))
            .or(models.first())
            .cloned()
            .ok_or_else(|| anyhow!("Ollama has no model pulled. Run: ollama pull {wanted}"))?;
        probe(Vendor::Ollama, &ep.ollama, None, &model).await?;
        let default_ep = crate::config::OllamaConfig::default().endpoint;
        let endpoint = (ep.ollama != default_ep).then_some(ep.ollama.as_str());
        persist_choice(
            &x0ne_dir.join("config.toml"),
            Vendor::Ollama,
            &model,
            endpoint,
        )?;
        return Ok(Ready {
            vendor: Vendor::Ollama,
            model,
            masked: String::new(),
            key: None,
        });
    }
    let Some(vendor) = vendor_of_key(answer) else {
        bail!(
            "not recognised: an Anthropic key starts with sk-ant-, an OpenAI key with sk-. \
             Type ollama for a local model. Nothing was saved."
        );
    };
    let model = vendor.default_model();
    let base = match vendor {
        Vendor::Anthropic => &ep.anthropic,
        _ => &ep.openai,
    };
    probe(vendor, base, Some(answer), &model)
        .await
        .map_err(|e| anyhow!("{e:#}. Nothing was saved."))?;
    let var = vendor.key_var().expect("keyed vendor");
    crate::setup_menu::save_secret_at(&x0ne_dir.join(".env"), var, answer)?;
    persist_choice(&x0ne_dir.join("config.toml"), vendor, &model, None)?;
    Ok(Ready {
        vendor,
        model,
        masked: mask_key(answer),
        key: Some(answer.to_string()),
    })
}

/// Write `[llm] provider` and the model into the one config file, in place:
/// comments and other settings survive. Owner-only, like every x0ne config.
pub fn persist_choice(
    path: &Path,
    vendor: Vendor,
    model: &str,
    ollama_endpoint: Option<&str>,
) -> Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("invalid TOML in {}", path.display()))?;
    if doc.get("llm").is_none() {
        doc["llm"] = toml_edit::table();
    }
    let provider = vendor.provider();
    doc["llm"]["provider"] = toml_edit::value(provider);
    // x0ne-engine: the relay names its model; nothing is pinned.
    if vendor != Vendor::X0neEngine {
        if doc["llm"].get(provider).is_none() {
            doc["llm"][provider] = toml_edit::table();
        }
        doc["llm"][provider]["model"] = toml_edit::value(model);
    }
    if let Some(ep) = ollama_endpoint {
        doc["llm"]["ollama"]["endpoint"] = toml_edit::value(ep);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::setup_menu::write_owner_only(path, doc.to_string().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Loopback server: answers every request with (status, body) and
    /// records each request's head and body.
    async fn fake(status: u16, body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 64 * 1024];
                let mut n = 0;
                // Read until the head and the announced body are in.
                loop {
                    let Ok(k) = sock.read(&mut buf[n..]).await else {
                        break;
                    };
                    if k == 0 {
                        break;
                    }
                    n += k;
                    let text = String::from_utf8_lossy(&buf[..n]).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| {
                                let l = l.to_ascii_lowercase();
                                l.strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if n >= end + 4 + len {
                            break;
                        }
                    }
                }
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());
                let reason = if status == 200 { "OK" } else { "ERR" };
                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (root, seen)
    }

    fn eps(root: &str) -> Endpoints {
        Endpoints {
            anthropic: root.into(),
            openai: root.into(),
            ollama: root.into(),
            x0ne_engine: Default::default(),
        }
    }

    const ANT_OK: &str = r#"{"id":"m","type":"message","role":"assistant","content":[{"type":"text","text":"OK"}],"usage":{"input_tokens":3,"output_tokens":1}}"#;

    #[test]
    fn the_key_prefix_names_the_vendor() {
        assert_eq!(vendor_of_key("sk-ant-api03-abcd"), Some(Vendor::Anthropic));
        assert_eq!(vendor_of_key("  sk-proj-abcd\n"), Some(Vendor::OpenAi));
        assert_eq!(vendor_of_key("sk-abcd1234"), Some(Vendor::OpenAi));
        assert_eq!(
            vendor_of_key("sk-or-v1-abcd"),
            None,
            "OpenRouter is not OpenAI"
        );
        assert_eq!(vendor_of_key("gsk_abcd"), None);
        assert_eq!(vendor_of_key(""), None);
    }

    #[test]
    fn a_mask_shows_the_last_four_characters_only() {
        let m = mask_key("sk-ant-api03-SECRETSECRET-wxyz");
        assert!(m.ends_with("wxyz"), "{m}");
        assert!(!m.contains("SECRET") && !m.contains("sk-ant"), "{m}");
        assert!(!mask_key("abc").contains("abc"));
    }

    #[test]
    fn detection_reads_the_key_not_the_variable_name() {
        let k = |a: Option<&str>, o: Option<&str>, c: Option<&str>| KeyEnv {
            anthropic: a.map(Into::into),
            openai: o.map(Into::into),
            compat: c.map(Into::into),
        };
        assert_eq!(
            detect_from_keys(&k(Some("sk-ant-1"), Some("sk-2"), None))
                .unwrap()
                .0,
            Vendor::Anthropic
        );
        // An Anthropic key pasted into OPENAI_API_KEY is still Anthropic's.
        let (v, key) = detect_from_keys(&k(None, Some("sk-ant-9"), None)).unwrap();
        assert_eq!((v, key.as_str()), (Vendor::Anthropic, "sk-ant-9"));
        assert_eq!(
            detect_from_keys(&k(None, None, Some("sk-ant-3")))
                .unwrap()
                .0,
            Vendor::Anthropic
        );
        assert_eq!(
            detect_from_keys(&k(None, Some("sk-proj-4"), None))
                .unwrap()
                .0,
            Vendor::OpenAi
        );
        assert!(detect_from_keys(&k(None, None, Some("gsk_5"))).is_none());
        assert!(detect_from_keys(&k(Some(""), None, None)).is_none());
        assert!(detect_from_keys(&KeyEnv::default()).is_none());
    }

    #[test]
    fn startup_detects_when_nobody_chose() {
        let ant = KeyEnv {
            anthropic: Some("sk-ant-1".into()),
            ..Default::default()
        };
        assert_eq!(
            plan_start(false, &ant, true),
            Start::Use {
                provider: "anthropic",
                key: Some("sk-ant-1".into())
            }
        );
        assert_eq!(plan_start(true, &ant, false), Start::Configured);
        assert_eq!(
            plan_start(false, &KeyEnv::default(), true),
            Start::Use {
                provider: "ollama",
                key: None
            }
        );
        assert_eq!(plan_start(false, &KeyEnv::default(), false), Start::Ask);
    }

    #[test]
    fn anthropic_defaults_to_haiku() {
        assert_eq!(Vendor::Anthropic.default_model(), "claude-haiku-4-5");
        assert_eq!(Vendor::Anthropic.provider(), "anthropic");
        assert_eq!(Vendor::OpenAi.provider(), "openai");
        assert_eq!(Vendor::Anthropic.key_var(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(Vendor::Ollama.key_var(), None);
    }

    #[tokio::test]
    async fn a_pasted_anthropic_key_is_tested_then_stored_once_in_0600() {
        let (root, seen) = fake(200, ANT_OK).await;
        let dir = tempfile::tempdir().unwrap();
        let key = "sk-ant-api03-SECRET-wxyz";
        let r = quickstart(&format!("  {key}\n"), &eps(&root), dir.path())
            .await
            .unwrap();
        assert_eq!(r.vendor, Vendor::Anthropic);
        assert_eq!(r.model, "claude-haiku-4-5");
        assert!(r.masked.ends_with("wxyz") && !r.masked.contains("SECRET"));

        // A real call was made, with the key in the Anthropic header.
        let reqs = seen.lock().unwrap().clone();
        assert_eq!(reqs.len(), 1, "one test call");
        assert!(reqs[0].starts_with("POST /v1/messages"), "{}", reqs[0]);
        assert!(reqs[0]
            .to_ascii_lowercase()
            .contains(&format!("x-api-key: {key}").to_ascii_lowercase()));
        assert!(reqs[0].contains("claude-haiku-4-5"));

        // The key lives in exactly one file, owner-only.
        let env = dir.path().join(".env");
        let text = std::fs::read_to_string(&env).unwrap();
        assert!(text.contains(&format!("ANTHROPIC_API_KEY={key}")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&env).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let holders: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| {
                std::fs::read_to_string(e.path())
                    .unwrap_or_default()
                    .contains("SECRET")
            })
            .collect();
        assert_eq!(holders.len(), 1, "the key is written to one file only");

        // The choice is in the one config file, without the key.
        let cfg = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        let parsed: crate::config::Config = toml::from_str(&cfg).unwrap();
        assert_eq!(parsed.llm.provider, "anthropic");
        assert_eq!(parsed.llm.anthropic.model, "claude-haiku-4-5");
    }

    #[tokio::test]
    async fn a_rejected_key_is_not_ready_and_not_saved() {
        let (root, seen) = fake(401, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#).await;
        let dir = tempfile::tempdir().unwrap();
        let e = quickstart("sk-ant-api03-bad-0000", &eps(&root), dir.path())
            .await
            .err()
            .unwrap();
        let msg = format!("{e:#}");
        assert!(msg.contains("401"), "{msg}");
        assert!(!msg.contains("bad-0000") || msg.contains("…0000"), "{msg}");
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(
            !dir.path().join(".env").exists(),
            "nothing saved after a failed test call"
        );
        assert!(!dir.path().join("config.toml").exists());
    }

    #[tokio::test]
    async fn an_unknown_answer_makes_no_call() {
        let (root, seen) = fake(200, ANT_OK).await;
        let dir = tempfile::tempdir().unwrap();
        assert!(quickstart("hello", &eps(&root), dir.path()).await.is_err());
        assert!(quickstart("", &eps(&root), dir.path()).await.is_err());
        assert_eq!(seen.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn an_openai_key_goes_to_openai_with_a_bearer() {
        let (root, seen) = fake(
            200,
            r#"{"choices":[{"message":{"role":"assistant","content":"OK"}}]}"#,
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let r = quickstart("sk-proj-SECRET-1234", &eps(&root), dir.path())
            .await
            .unwrap();
        assert_eq!(r.vendor, Vendor::OpenAi);
        let reqs = seen.lock().unwrap().clone();
        assert!(
            reqs[0].starts_with("POST /v1/chat/completions"),
            "{}",
            reqs[0]
        );
        assert!(
            reqs[0].contains("Bearer sk-proj-SECRET-1234")
                || reqs[0].contains("bearer sk-proj-SECRET-1234")
        );
        let text = std::fs::read_to_string(dir.path().join(".env")).unwrap();
        assert!(text.contains("OPENAI_API_KEY=sk-proj-SECRET-1234"));
        let parsed: crate::config::Config =
            toml::from_str(&std::fs::read_to_string(dir.path().join("config.toml")).unwrap())
                .unwrap();
        assert_eq!(parsed.llm.provider, "openai");
    }

    #[tokio::test]
    async fn ollama_needs_no_key_and_picks_a_pulled_model() {
        // The same body answers /api/tags and /api/chat.
        let (root, seen) = fake(200, r#"{"models":[{"name":"qwen2.5:7b"}],"message":{"role":"assistant","content":"OK"},"done":true}"#).await;
        let dir = tempfile::tempdir().unwrap();
        let r = quickstart("Ollama", &eps(&root), dir.path()).await.unwrap();
        assert_eq!(r.vendor, Vendor::Ollama);
        assert_eq!(r.model, "qwen2.5:7b");
        let reqs = seen.lock().unwrap().clone();
        assert!(
            reqs.iter().any(|q| q.starts_with("POST /api/chat")),
            "{reqs:?}"
        );
        assert!(!dir.path().join(".env").exists(), "no key, no secrets file");
        let parsed: crate::config::Config =
            toml::from_str(&std::fs::read_to_string(dir.path().join("config.toml")).unwrap())
                .unwrap();
        assert_eq!(parsed.llm.provider, "ollama");
        assert_eq!(parsed.llm.ollama.model, "qwen2.5:7b");
    }

    #[tokio::test]
    async fn the_choice_is_merged_into_an_existing_config() {
        let (root, _seen) = fake(200, ANT_OK).await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "# mine\n[llm]\nprovider = \"openai-compat\"\n\n[ui]\nauto_confirm = true\n",
        )
        .unwrap();
        quickstart("sk-ant-api03-k-abcd", &eps(&root), dir.path())
            .await
            .unwrap();
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("# mine"), "comments kept: {text}");
        let parsed: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.llm.provider, "anthropic");
        assert!(parsed.ui.auto_confirm, "other settings kept");
    }

    // ── x0ne-engine ───────────────────────────────────────────────────

    fn engine_eps(relay_root: &str, key_file: &Path) -> Endpoints {
        let mut ep = eps(relay_root);
        ep.x0ne_engine = crate::config::X0neEngineConfig {
            base_url: format!("{relay_root}/v1"),
            key_file: Some(key_file.display().to_string()),
            ..Default::default()
        };
        ep
    }

    #[tokio::test]
    async fn typing_x0ne_engine_checks_the_relay_and_saves_no_key_and_no_model() {
        use crate::engine_local::test_relay;
        let dir = tempfile::tempdir().unwrap();
        let key = test_relay::key_file(dir.path(), "file-key-0123456789");
        let relay =
            test_relay::start(test_relay::route(test_relay::verified("qwen3:0.6b"), "x")).await;
        let x0ne = dir.path().join(".x0ne");
        let ready = quickstart(" x0ne-engine ", &engine_eps(&relay.root, &key), &x0ne)
            .await
            .unwrap();
        assert_eq!(ready.vendor, Vendor::X0neEngine);
        assert!(ready.key.is_none() && ready.model.is_empty() && ready.masked.is_empty());
        assert_eq!(Vendor::X0neEngine.key_var(), None, "no key entry");
        let saved = std::fs::read_to_string(x0ne.join("config.toml")).unwrap();
        let doc: toml::Value = toml::from_str(&saved).unwrap();
        assert_eq!(
            doc["llm"]["provider"].as_str(),
            Some("x0ne-engine"),
            "{saved}"
        );
        assert!(
            !saved.contains("model"),
            "the relay names the model: {saved}"
        );
        assert!(!x0ne.join(".env").exists(), "no key is saved");
        let paths: Vec<String> = relay.all().into_iter().map(|s| s.path).collect();
        assert_eq!(
            paths,
            [crate::engine_local::STATUS_PATH],
            "no prompt in a setup check"
        );
        let cfg: crate::config::Config = toml::from_str(&saved).unwrap();
        assert_eq!(cfg.llm.provider, "x0ne-engine");
    }

    #[tokio::test]
    async fn an_engine_that_is_not_verified_saves_nothing() {
        use crate::engine_local::test_relay;
        let dir = tempfile::tempdir().unwrap();
        let key = test_relay::key_file(dir.path(), "file-key-0123456789");
        let status = serde_json::json!({"state":"serving","verified":false,"model":"m"});
        let relay = test_relay::start(test_relay::route(status, "x")).await;
        let x0ne = dir.path().join(".x0ne");
        let err = quickstart("x0ne-engine", &engine_eps(&relay.root, &key), &x0ne)
            .await
            .err()
            .expect("refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("x0ne-engine cloud up") && msg.contains("Nothing was saved"),
            "{msg}"
        );
        assert!(!x0ne.exists(), "nothing written");
        assert!(relay.completions().is_empty());
    }
}
