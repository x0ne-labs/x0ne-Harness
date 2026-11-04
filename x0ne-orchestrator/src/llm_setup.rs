use anyhow::{anyhow, Result};
use colored::Colorize;

use crate::api::{self, LlmBackend};
use crate::config::Config;
use crate::conversation::{Message, Role};
use crate::llm_quickstart::{self as qs, KeyEnv, Start};
use crate::terminal::*;

// ═══════════════════════════════════════════════════════════════════════════
//  LLM setup: from a key to a working backend
// ═══════════════════════════════════════════════════════════════════════════

/// Connect to a model. A provider someone chose (config file, env, flag) is
/// used as is; otherwise it is read from the key found, then a local Ollama;
/// otherwise one question is asked.
pub async fn setup_llm(config: &mut Config) -> Result<Box<dyn LlmBackend>> {
    // A verified relay or an error; never a question, never another provider.
    if crate::engine_local::is_engine_provider(&config.llm.provider) {
        return connect_engine(config).await;
    }
    // `claude-max`/`anthropic-oauth` were removed: an old config keeps
    // launching on the API-key path.
    if matches!(
        config.llm.provider.as_str(),
        "claude-max" | "anthropic-oauth"
    ) {
        config.llm.provider = "anthropic".into();
    }

    // `--endpoint` is the server the probe asks, as the session would.
    let endpoint = config
        .llm
        .model_flags
        .endpoint
        .clone()
        .unwrap_or_else(|| config.llm.ollama.endpoint.clone());
    let keys = KeyEnv::from_env();
    let ollama_models = if config.llm.provider_explicit || qs::detect_from_keys(&keys).is_some() {
        Vec::new()
    } else {
        qs::ollama_models(&endpoint).await.unwrap_or_default()
    };
    let mut key_hint = None;
    match qs::plan_start(
        config.llm.provider_explicit,
        &keys,
        !ollama_models.is_empty(),
    ) {
        Start::Configured => {}
        Start::Use { provider, key } => {
            config.llm.provider = provider.into();
            config.apply_llm_env(|k| std::env::var(k).ok());
            if provider == "ollama" && !ollama_models.contains(&config.llm.ollama.model) {
                config.llm.ollama.model = ollama_models[0].clone();
            }
            // The flags beat what was just applied, on the provider picked.
            config.apply_model_flags();
            key_hint = key;
        }
        Start::Ask => return ask_and_connect(config).await,
    }

    match try_auto_connect(config, key_hint).await {
        Ok(backend) => Ok(backend),
        // Nothing can be asked: the reason is the error, on stderr. Review of
        // the pre-release build (S4): `-p` printed a refused key on stdout, then "Set
        // OPENAI_API_KEY", which was set.
        Err(reason) if !std::io::IsTerminal::is_terminal(&std::io::stdin()) => Err(anyhow!(reason)),
        Err(reason) => {
            println!("  {} {}", "✗".red(), reason);
            ask_and_connect(config).await
        }
    }
}

/// The relay's status is checked here, then again before every call.
async fn connect_engine(config: &Config) -> Result<Box<dyn LlmBackend>> {
    let engine = crate::engine_local::EngineBackend::from_config(&config.llm.x0ne_engine)?;
    engine.preflight().await?;
    Ok(Box::new(engine))
}

/// Build the backend the config names, without asking anything. `Err` says
/// why it is not usable.
async fn try_auto_connect(
    config: &Config,
    key_hint: Option<String>,
) -> std::result::Result<Box<dyn LlmBackend>, String> {
    // The key is checked before any request, the probe below included.
    let key = crate::model_keys::resolve(&config.llm, key_hint).map_err(|e| format!("{e:#}"))?;
    match config.llm.provider.as_str() {
        "anthropic" => {
            let key = key.ok_or("provider anthropic, but no Anthropic API key found")?;
            api::create_backend(&config.llm, Some(key)).map_err(|e| format!("{e:#}"))
        }
        "openai" => {
            if key.is_none() {
                return Err(format!(
                    "provider openai, but {} is not set",
                    config.llm.openai.key_var()
                ));
            }
            api::create_backend(&config.llm, None).map_err(|e| format!("{e:#}"))
        }
        "openai-compat" | "openai_compat" | "litellm" => {
            let cfg = &config.llm.openai_compat;
            if cfg.base_url.is_empty() || cfg.model.is_empty() {
                return Err("provider openai-compat, but its base_url or model is empty".into());
            }
            let local = cfg.base_url.contains("localhost") || cfg.base_url.contains("127.0.0.1");
            if !local && key.is_none() {
                return Err(format!(
                    "provider openai-compat at {}, but no key is set",
                    crate::redact::url_for_display(&cfg.base_url)
                ));
            }
            api::create_backend(&config.llm, None).map_err(|e| format!("{e:#}"))
        }
        "ollama" => {
            // Building an OllamaClient proves nothing: ask the server first
            // (field report, 2026-08-11).
            let endpoint = config.llm.ollama.endpoint.clone();
            if list_ollama_models(&endpoint).await.is_err() {
                return Err(format!(
                    "provider ollama, but nothing answers at {}",
                    crate::redact::url_for_display(&endpoint)
                ));
            }
            api::create_backend(&config.llm, None).map_err(|e| format!("{e:#}"))
        }
        other => Err(format!("unknown provider {other:?}")),
    }
}

/// The one question: paste a key or type ollama. Tested for real before
/// "ready"; the key is saved once, owner-only, in `~/.x0ne/.env`.
pub async fn ask_and_connect(config: &mut Config) -> Result<Box<dyn LlmBackend>> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "no usable model. Set ANTHROPIC_API_KEY or OPENAI_API_KEY, or start Ollama."
        ));
    }
    let x0ne_dir = Config::default_user_config_path()
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| anyhow!("no home directory"))?;
    let mut ep = qs::Endpoints::real(config.llm.ollama.endpoint.clone());
    ep.x0ne_engine = config.llm.x0ne_engine.clone();
    for _ in 0..3 {
        println!();
        print!("  {} ", qs::QUESTION.bold());
        flush_stdout();
        let answer = read_secret_line()?;
        if answer.trim().is_empty() || answer.trim() == "/quit" {
            break;
        }
        println!("  {}", "testing…".dimmed());
        match qs::quickstart(&answer, &ep, &x0ne_dir).await {
            Ok(ready) => {
                if let (Some(var), Some(key)) = (ready.vendor.key_var(), ready.key.as_deref()) {
                    std::env::set_var(var, key);
                }
                config.llm.provider = ready.vendor.provider().into();
                config.llm.provider_explicit = true;
                config.set_active_model(ready.model.clone());
                let backend = if ready.vendor == qs::Vendor::X0neEngine {
                    connect_engine(config).await?
                } else {
                    api::create_backend(&config.llm, ready.key.clone())?
                };
                let saved = if ready.masked.is_empty() {
                    String::new()
                } else {
                    format!(
                        " · key {} saved in {}",
                        ready.masked,
                        x0ne_dir.join(".env").display()
                    )
                };
                print_ok(&format!(
                    "ready: {}{}",
                    format_provider_model(&backend.display_name()),
                    saved
                ));
                return Ok(backend);
            }
            Err(e) => println!("  {} {:#}", "✗".red(), e),
        }
    }
    Err(anyhow!(
        "no usable model. Set ANTHROPIC_API_KEY or OPENAI_API_KEY, or start Ollama."
    ))
}

/// One real round trip through `backend`. "Ready" is said only after it.
pub async fn verify_backend(backend: &dyn LlmBackend) -> Result<()> {
    let ping = [Message {
        role: Role::User,
        content: "Reply with OK.".into(),
        images: vec![],
    }];
    backend
        .send("Reply with OK.", &ping)
        .await
        .map(|_| ())
        .map_err(|e| anyhow!("test call to {} failed: {e:#}", backend.display_name()))
}

async fn list_ollama_models(endpoint: &str) -> Result<Vec<String>> {
    let url = format!("{}/api/tags", endpoint.trim_end_matches('/'));
    let resp = api::client_for(&url)
        .build()?
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?;
    let body: serde_json::Value = resp.json().await?;
    let mut models: Vec<String> = body["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    models.sort();
    Ok(models)
}

async fn check_ollama_alive(url: &str) -> bool {
    let check_url = format!("{}/api/tags", url.trim_end_matches('/'));
    let Ok(client) = api::client_for(&check_url).build() else {
        return false;
    };
    client
        .get(&check_url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

pub async fn check_ollama_alive_pub(url: &str) -> bool {
    check_ollama_alive(url).await
}

pub async fn list_ollama_models_pub(endpoint: &str) -> Result<Vec<String>> {
    list_ollama_models(endpoint).await
}

pub async fn scan_lan_for_ollama() -> Result<Vec<String>> {
    let mut found = Vec::new();

    // Get local IPs to find the subnet
    let local_ip = local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_default();

    if local_ip.is_empty() {
        return Ok(found);
    }

    // Extract subnet prefix (e.g., "192.168.1")
    let parts: Vec<&str> = local_ip.split('.').collect();
    if parts.len() != 4 {
        return Ok(found);
    }
    let subnet = format!("{}.{}.{}", parts[0], parts[1], parts[2]);

    // Common Ollama port
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()?;

    let mut handles = Vec::new();
    for i in 1..=254u8 {
        let ip = format!("{}.{}", subnet, i);
        if ip == local_ip {
            continue;
        }
        let url = format!("http://{}:11434/api/tags", ip);
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            match c.get(&url).send().await {
                Ok(r) if r.status().is_success() => Some(format!("http://{}:11434", ip)),
                _ => None,
            }
        }));
    }

    for h in handles {
        if let Ok(Some(url)) = h.await {
            found.push(url);
        }
    }

    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve_once(status: u16, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 64 * 1024];
            let _ = sock.read(&mut buf).await;
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        root
    }

    #[tokio::test]
    async fn a_backend_that_answers_401_is_not_ready() {
        let root = serve_once(401, r#"{"error":{"message":"Incorrect API key"}}"#).await;
        let b = api::OpenAICompatClient::new(root, "m", 8, Some("sk-x".into())).unwrap();
        let e = verify_backend(&b).await.expect_err("a 401 is a failure");
        assert!(format!("{e:#}").contains("test call"), "{e:#}");
    }

    #[tokio::test]
    async fn a_backend_that_answers_is_ready() {
        let root = serve_once(
            200,
            r#"{"choices":[{"message":{"role":"assistant","content":"OK"}}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
        )
        .await;
        let b = api::OpenAICompatClient::new(root, "m", 8, None).unwrap();
        verify_backend(&b).await.unwrap();
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;

    /// Re-executed with decoy keys in the environment: an `x0ne-engine` that
    /// is not ready is an error, never a question nor another provider.
    #[test]
    fn an_engine_that_is_not_ready_is_an_error_not_a_fallback() {
        const CHILD: &str = "X0NE_TEST_ENGINE_SETUP_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let rt = tokio::runtime::Runtime::new().unwrap();
            return rt.block_on(async {
                for explicit in [true, false] {
                    let mut config = Config::default();
                    config.llm.provider = "x0ne-engine".into();
                    config.llm.provider_explicit = explicit;
                    config.llm.x0ne_engine.key_file =
                        Some("/nonexistent/x0ne-engine-test/local.key".into());
                    let err = setup_llm(&mut config).await.err().expect("refused");
                    let msg = format!("{err:#}");
                    assert!(
                        msg.contains("x0ne-engine cloud up"),
                        "explicit={explicit}: {msg}"
                    );
                    assert_eq!(config.llm.provider, "x0ne-engine", "explicit={explicit}");
                }
            });
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "llm_setup::engine_tests::an_engine_that_is_not_ready_is_an_error_not_a_fallback",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("ANTHROPIC_API_KEY", "sk-ant-api03-decoy-not-a-key")
            .env("OPENAI_API_KEY", "sk-proj-decoy-not-a-key")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success() && text.contains("1 passed"), "{text}");
    }
}
