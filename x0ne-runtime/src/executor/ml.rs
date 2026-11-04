//! ML/LLM primitives — LlmCall, OllamaApi, DatasetGenerate, FineTune.
//!
//! These actions enable X0NE to call LLMs as tools, manage Ollama models,
//! generate training datasets, and launch fine-tuning jobs.

use crate::config::AppConfig;
use crate::result::{ActionOutput, ActionResult, ErrorKind};

/// Resolve an `auth` field to a raw token for `endpoint`. Accepts:
///   * `$ENV:VAR_NAME` — reads `VAR_NAME` from the runtime's env. Empty or
///     unset → returns `Ok(None)` (silently disable auth — the caller may
///     legitimately be hitting a local Ollama).
///   * Any other string → returned verbatim as the token.
///   * `None` → `Ok(None)`.
///
/// Returns `Err(msg)` when the spec is malformed (e.g. `$ENV:` with no
/// name), and when the key may not go to `endpoint`: the table of
/// `x0ne_actions::model_keys` is the session's, and a vendor's variable is
/// named by the model here, not by the operator, so it goes only to the
/// vendor's hosts over https. Measured on the pre-release build (review, S1):
/// `$ENV:ANTHROPIC_API_KEY` reached a loopback endpoint the model named as a
/// Bearer. A missing variable is still not an error: hitting a no-auth
/// localhost endpoint with the same emitted JSON keeps working.
pub(crate) fn resolve_auth(spec: Option<&str>, endpoint: &str) -> Result<Option<String>, String> {
    resolve_auth_from(spec, endpoint, &|var| std::env::var(var).ok())
}

/// [`resolve_auth`], `env` reading a variable.
fn resolve_auth_from(
    spec: Option<&str>,
    endpoint: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<String>, String> {
    use x0ne_actions::model_keys::{host_of, mask, provider_at, refusal, row, Refusal};
    let raw = match spec {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(None),
    };
    let (token, var) = if let Some(var) = raw.strip_prefix("$ENV:") {
        let var = var.trim();
        if var.is_empty() {
            return Err(
                "auth: `$ENV:` requires a variable name (e.g. `$ENV:ANTHROPIC_API_KEY`)".into(),
            );
        }
        match env(var) {
            Some(v) if !v.is_empty() => (v, Some(var)),
            _ => return Ok(None),
        }
    } else {
        (raw.to_string(), None)
    };
    let implicit = var.filter(|v| row(v).is_some());
    let Some(why) = refusal(&token, implicit, provider_at(endpoint), endpoint) else {
        return Ok(Some(token));
    };
    let host = host_of(endpoint).map_or_else(|| "an endpoint that does not parse".into(), |h| h.0);
    let what = var.map_or_else(|| "the literal key".to_string(), |v| format!("$ENV:{v}"));
    Err(match why {
        Refusal::Pinned(r) => format!(
            "auth refused: {what} ({}) is {a_key} and goes only to https://{}, not to {host}. \
             Nothing was sent.",
            mask(&token),
            r.hosts.join(", https://"),
            a_key = r.a_key(),
        ),
        Refusal::Implicit(r) => format!(
            "auth refused: {what} ({}) goes only to https://{}, not to {host}. Nothing was \
             sent. A key for another endpoint goes in a variable the key table does not \
             list, for example $ENV:MY_LLM_KEY.",
            mask(&token),
            r.hosts.join(", https://"),
        ),
        Refusal::Format(r) => format!(
            "auth refused: {host} takes {a_key} (starts with {}), and {what} ({}) is not one. \
             Nothing was sent.",
            r.prefix,
            mask(&token),
            a_key = r.a_key(),
        ),
    })
}

/// Classify an endpoint URL as Anthropic's official API host so callers
/// can pick the right wire shape (Anthropic-style headers + `/v1/messages`
/// body) without diverging case-sensitivity or substring-match rules
/// between sites. Matches on the parsed URL's host, NOT a raw substring,
/// so a loopback URL whose *path* happens to contain "anthropic.com" is
/// not mis-classified. Case-insensitive per RFC 3986 §3.2.2 (host case
/// is irrelevant for routing).
///
/// Returns `false` when the endpoint doesn't parse as an absolute URL —
/// the caller falls back to the generic OpenAI-compat path, which is
/// the safe default.
pub(crate) fn is_anthropic_endpoint(endpoint: &str) -> bool {
    reqwest::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .map(|h| h == "api.anthropic.com" || h.ends_with(".anthropic.com"))
        .unwrap_or(false)
}

/// Attach the right auth header(s) to a reqwest builder based on endpoint.
/// Anthropic uses its own header scheme; everything else (OpenAI, Groq,
/// OpenRouter, Together, Fireworks, ollama.com cloud, …) follows the
/// `Authorization: Bearer <token>` convention.
pub(crate) fn apply_auth(
    builder: reqwest::RequestBuilder,
    endpoint: &str,
    token: &str,
) -> reqwest::RequestBuilder {
    if is_anthropic_endpoint(endpoint) {
        builder
            .header("x-api-key", token)
            .header("anthropic-version", "2023-06-01")
    } else {
        builder.header("Authorization", format!("Bearer {}", token))
    }
}

/// Build the wire-format request body for `flavor`. Pure (no I/O), so it
/// can be unit-tested without a TCP listener. Extracted from `llm_call`
/// to keep the dispatcher thin and to give the test suite a target it
/// can hit on any flavour, including Anthropic, without DNS spoofing.
pub(crate) fn build_llm_body(
    flavor: &str,
    model: &str,
    prompt: &str,
    system: Option<&str>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
) -> serde_json::Value {
    match flavor {
        "anthropic" => {
            let mut req = serde_json::json!({
                "model": model,
                "max_tokens": max_tokens.unwrap_or(1024),
                "messages": [{"role": "user", "content": prompt}],
            });
            if let Some(sys) = system {
                req["system"] = serde_json::Value::String(sys.to_string());
            }
            if let Some(temp) = temperature {
                req["temperature"] = serde_json::Value::from(temp);
            }
            req
        }
        "ollama" => {
            let mut req = serde_json::json!({
                "model": model,
                "prompt": prompt,
                "stream": false,
            });
            if let Some(sys) = system {
                req["system"] = serde_json::Value::String(sys.to_string());
            }
            if let Some(temp) = temperature {
                req["options"] = serde_json::json!({"temperature": temp});
            }
            req
        }
        _ => {
            // OpenAI-compatible /v1/chat/completions
            let mut messages = Vec::new();
            if let Some(sys) = system {
                messages.push(serde_json::json!({"role": "system", "content": sys}));
            }
            messages.push(serde_json::json!({"role": "user", "content": prompt}));
            let mut req = serde_json::json!({
                "model": model,
                "messages": messages,
            });
            if let Some(temp) = temperature {
                req["temperature"] = serde_json::Value::from(temp);
            }
            if let Some(max) = max_tokens {
                req["max_tokens"] = serde_json::Value::from(max);
            }
            req
        }
    }
}

/// Build the canonical URL for `flavor`. Pure. Same rationale as
/// `build_llm_body` — keeps the dispatcher thin and lets tests assert
/// on the exact route per flavour without standing up a server.
pub(crate) fn build_llm_url(flavor: &str, endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    match flavor {
        "anthropic" => format!("{}/v1/messages", trimmed),
        "ollama" => format!("{}/api/generate", trimmed),
        _ => format!("{}/v1/chat/completions", trimmed),
    }
}

/// Call an LLM endpoint (Ollama or OpenAI-compatible) and return the response.
pub async fn llm_call(
    endpoint: &str,
    model: &str,
    prompt: &str,
    system: Option<&str>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    auth: Option<&str>,
    _config: &AppConfig,
) -> ActionResult {
    // SECURITY (SSRF): the endpoint is agent-controlled. Block cloud-metadata,
    // link-local, and RFC1918 targets so a crafted endpoint can't exfiltrate
    // cloud credentials via the response body. Loopback stays allowed (local
    // Ollama). The resolved IP is pinned into the client to stop DNS rebinding.
    let ssrf = match crate::executor::system::check_ssrf_llm(endpoint) {
        Ok(s) => s,
        Err(reason) => {
            return ActionResult::err(
                "LlmCall",
                ErrorKind::InvalidInput,
                format!("endpoint blocked: {}", reason),
                0,
            )
        }
    };
    let client = match crate::executor::system::build_pinned_client(
        endpoint,
        &ssrf,
        std::time::Duration::from_secs(120),
    ) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("LlmCall", ErrorKind::Internal, e, 0),
    };

    // Three wire flavours, picked from the endpoint URL:
    //   Anthropic     — POST /v1/messages with {model, max_tokens, system?, messages[]}
    //   Ollama        — POST /api/generate with {model, prompt, system?, stream:false, options}
    //   OpenAI-compat — POST /v1/chat/completions with {model, messages[]}
    // Anthropic check must come BEFORE the Ollama check: the Anthropic
    // path doesn't share any URL marker with the others, but a custom
    // Anthropic proxy on port 11434 (rare but possible) would otherwise
    // get mis-routed to Ollama. Anthropic-flavored payloads also need a
    // non-optional `max_tokens` — we default to 1024 when the agent
    // didn't supply one, mirroring most client SDKs.
    //
    // Host-based detection (via `is_anthropic_endpoint`) instead of raw
    // substring matching, so a loopback URL whose path contains
    // "anthropic.com" doesn't get mis-classified, and so the two call
    // sites here and in `apply_auth` agree on case.
    let flavor = if is_anthropic_endpoint(endpoint) {
        "anthropic"
    } else if endpoint.contains("11434") || endpoint.contains("/api/") {
        "ollama"
    } else {
        "openai"
    };

    let body = build_llm_body(flavor, model, prompt, system, temperature, max_tokens);
    let url = build_llm_url(flavor, endpoint);

    let start = std::time::Instant::now();

    let token = match resolve_auth(auth, endpoint) {
        Ok(t) => t,
        Err(e) => return ActionResult::err("LlmCall", ErrorKind::InvalidInput, e, 0),
    };
    let mut req = client.post(&url).json(&body);
    if let Some(tok) = &token {
        req = apply_auth(req, endpoint, tok);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let base_msg = format!("Request failed: {}", e);
            let hint = sandbox_localhost_hint(endpoint, &e);
            let msg = if let Some(h) = hint {
                format!("{base_msg}\n\n💡 hint: {h}")
            } else {
                base_msg
            };
            return ActionResult::err(
                "LlmCall",
                ErrorKind::ExecutionFailed,
                msg,
                start.elapsed().as_millis() as u64,
            );
        }
    };

    let status = resp.status().as_u16();
    let resp_body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            return ActionResult::err(
                "LlmCall",
                ErrorKind::ExecutionFailed,
                format!("Read body: {}", e),
                start.elapsed().as_millis() as u64,
            )
        }
    };

    if status >= 400 {
        return ActionResult::err(
            "LlmCall",
            ErrorKind::ExecutionFailed,
            format!(
                "API returned {}: {}",
                status,
                &resp_body[..resp_body.len().min(500)]
            ),
            start.elapsed().as_millis() as u64,
        );
    }

    // Extract response text
    let parsed: serde_json::Value = match serde_json::from_str(&resp_body) {
        Ok(v) => v,
        Err(e) => {
            return ActionResult::err(
                "LlmCall",
                ErrorKind::ExecutionFailed,
                format!("Parse JSON: {}", e),
                start.elapsed().as_millis() as u64,
            )
        }
    };

    let response_text = match flavor {
        "anthropic" => parsed
            .pointer("/content/0/text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "ollama" => parsed["response"].as_str().unwrap_or("").to_string(),
        _ => parsed["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    };

    let tokens_used = match flavor {
        "anthropic" => {
            let inp = parsed
                .pointer("/usage/input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let out = parsed
                .pointer("/usage/output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            inp + out
        }
        "ollama" => parsed["eval_count"].as_u64().unwrap_or(0),
        _ => parsed["usage"]["total_tokens"].as_u64().unwrap_or(0),
    };

    let duration = start.elapsed().as_millis() as u64;

    ActionResult::ok(
        "LlmCall",
        ActionOutput::LlmResponse {
            model: model.to_string(),
            response: response_text,
            tokens_used,
        },
        duration,
    )
}

/// Manage Ollama models (list, pull, create, delete, show).
pub async fn ollama_api(
    endpoint: &str,
    operation: &str,
    model: Option<&str>,
    modelfile: Option<&str>,
    auth: Option<&str>,
    _config: &AppConfig,
) -> ActionResult {
    // SECURITY (SSRF): block metadata/link-local/RFC1918; loopback allowed (local Ollama).
    let ssrf = match crate::executor::system::check_ssrf_llm(endpoint) {
        Ok(s) => s,
        Err(reason) => {
            return ActionResult::err(
                "OllamaApi",
                ErrorKind::InvalidInput,
                format!("endpoint blocked: {}", reason),
                0,
            )
        }
    };
    let client = match crate::executor::system::build_pinned_client(
        endpoint,
        &ssrf,
        std::time::Duration::from_secs(300),
    ) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("OllamaApi", ErrorKind::Internal, e, 0),
    };

    let base = endpoint.trim_end_matches('/');
    let start = std::time::Instant::now();
    let token = match resolve_auth(auth, endpoint) {
        Ok(t) => t,
        Err(e) => return ActionResult::err("OllamaApi", ErrorKind::InvalidInput, e, 0),
    };
    let with_auth = |b: reqwest::RequestBuilder| -> reqwest::RequestBuilder {
        match &token {
            Some(tok) => apply_auth(b, endpoint, tok),
            None => b,
        }
    };

    match operation {
        "list" => {
            let resp = match with_auth(client.get(format!("{}/api/tags", base)))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return ActionResult::err(
                        "OllamaApi",
                        ErrorKind::ExecutionFailed,
                        format!("Request: {}", e),
                        start.elapsed().as_millis() as u64,
                    )
                }
            };
            let body: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    return ActionResult::err(
                        "OllamaApi",
                        ErrorKind::ExecutionFailed,
                        format!("Parse: {}", e),
                        start.elapsed().as_millis() as u64,
                    )
                }
            };
            let models = body["models"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| {
                            let name = m["name"].as_str()?;
                            let size = m["size"].as_u64().unwrap_or(0);
                            Some(serde_json::json!({"name": name, "size_bytes": size}))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            let count = models.len();
            ActionResult::ok(
                "OllamaApi",
                ActionOutput::OllamaModels {
                    operation: "list".into(),
                    models,
                    message: format!("{} models available", count),
                },
                start.elapsed().as_millis() as u64,
            )
        }
        "pull" => {
            let name = match model {
                Some(n) => n,
                None => {
                    return ActionResult::err(
                        "OllamaApi",
                        ErrorKind::InvalidInput,
                        "pull requires model name",
                        0,
                    )
                }
            };
            let resp = with_auth(client.post(format!("{}/api/pull", base)))
                .json(&serde_json::json!({"name": name, "stream": false}))
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => ActionResult::ok(
                    "OllamaApi",
                    ActionOutput::OllamaModels {
                        operation: "pull".into(),
                        models: vec![],
                        message: format!("pulled {}", name),
                    },
                    start.elapsed().as_millis() as u64,
                ),
                Ok(r) => {
                    let body = r.text().await.unwrap_or_default();
                    ActionResult::err(
                        "OllamaApi",
                        ErrorKind::ExecutionFailed,
                        format!("pull failed: {}", body),
                        start.elapsed().as_millis() as u64,
                    )
                }
                Err(e) => ActionResult::err(
                    "OllamaApi",
                    ErrorKind::ExecutionFailed,
                    format!("pull: {}", e),
                    start.elapsed().as_millis() as u64,
                ),
            }
        }
        "create" => {
            let name = match model {
                Some(n) => n,
                None => {
                    return ActionResult::err(
                        "OllamaApi",
                        ErrorKind::InvalidInput,
                        "create requires model name",
                        0,
                    )
                }
            };
            let mf = match modelfile {
                Some(m) => m,
                None => {
                    return ActionResult::err(
                        "OllamaApi",
                        ErrorKind::InvalidInput,
                        "create requires modelfile content",
                        0,
                    )
                }
            };
            let resp = with_auth(client.post(format!("{}/api/create", base)))
                .json(&serde_json::json!({"name": name, "modelfile": mf, "stream": false}))
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => ActionResult::ok(
                    "OllamaApi",
                    ActionOutput::OllamaModels {
                        operation: "create".into(),
                        models: vec![],
                        message: format!("created model {}", name),
                    },
                    start.elapsed().as_millis() as u64,
                ),
                Ok(r) => {
                    let body = r.text().await.unwrap_or_default();
                    ActionResult::err(
                        "OllamaApi",
                        ErrorKind::ExecutionFailed,
                        format!("create failed: {}", body),
                        start.elapsed().as_millis() as u64,
                    )
                }
                Err(e) => ActionResult::err(
                    "OllamaApi",
                    ErrorKind::ExecutionFailed,
                    format!("create: {}", e),
                    start.elapsed().as_millis() as u64,
                ),
            }
        }
        "delete" => {
            let name = match model {
                Some(n) => n,
                None => {
                    return ActionResult::err(
                        "OllamaApi",
                        ErrorKind::InvalidInput,
                        "delete requires model name",
                        0,
                    )
                }
            };
            let resp = with_auth(client.delete(format!("{}/api/delete", base)))
                .json(&serde_json::json!({"name": name}))
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => ActionResult::ok(
                    "OllamaApi",
                    ActionOutput::OllamaModels {
                        operation: "delete".into(),
                        models: vec![],
                        message: format!("deleted {}", name),
                    },
                    start.elapsed().as_millis() as u64,
                ),
                Ok(r) => {
                    let body = r.text().await.unwrap_or_default();
                    ActionResult::err(
                        "OllamaApi",
                        ErrorKind::ExecutionFailed,
                        format!("delete failed: {}", body),
                        start.elapsed().as_millis() as u64,
                    )
                }
                Err(e) => ActionResult::err(
                    "OllamaApi",
                    ErrorKind::ExecutionFailed,
                    format!("delete: {}", e),
                    start.elapsed().as_millis() as u64,
                ),
            }
        }
        "show" => {
            let name = match model {
                Some(n) => n,
                None => {
                    return ActionResult::err(
                        "OllamaApi",
                        ErrorKind::InvalidInput,
                        "show requires model name",
                        0,
                    )
                }
            };
            let resp = with_auth(client.post(format!("{}/api/show", base)))
                .json(&serde_json::json!({"name": name}))
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    let body: serde_json::Value = r.json().await.unwrap_or_default();
                    ActionResult::ok(
                        "OllamaApi",
                        ActionOutput::OllamaModels {
                            operation: "show".into(),
                            models: vec![body],
                            message: format!("model info for {}", name),
                        },
                        start.elapsed().as_millis() as u64,
                    )
                }
                Ok(r) => {
                    let body = r.text().await.unwrap_or_default();
                    ActionResult::err(
                        "OllamaApi",
                        ErrorKind::ExecutionFailed,
                        format!("show failed: {}", body),
                        start.elapsed().as_millis() as u64,
                    )
                }
                Err(e) => ActionResult::err(
                    "OllamaApi",
                    ErrorKind::ExecutionFailed,
                    format!("show: {}", e),
                    start.elapsed().as_millis() as u64,
                ),
            }
        }
        _ => ActionResult::err(
            "OllamaApi",
            ErrorKind::InvalidInput,
            format!(
                "unknown operation: {}. Use list/pull/create/delete/show",
                operation
            ),
            0,
        ),
    }
}

/// Generate a JSONL training dataset using an LLM.
/// Calls the LLM repeatedly to create instruction/response pairs.
pub async fn dataset_generate(
    endpoint: &str,
    model: &str,
    topic: &str,
    count: usize,
    output_path: &str,
    style: Option<&str>,
    config: &AppConfig,
) -> ActionResult {
    let start = std::time::Instant::now();
    // DatasetGenerate writes an agent-chosen path: confine it like any write.
    // This primitive was previously unconfined at BOTH layers (absent from the
    // policy engine's path extraction and writing the raw string here).
    let confined_out = match crate::executor::confine::resolve_write(output_path, &config.policy) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("DatasetGenerate", e.kind(), e.message(), 0),
    };
    let style = style.unwrap_or("instruction-response");
    let count = count.min(1000); // Safety cap

    let system_prompt = format!(
        "You are a training data generator. Generate exactly ONE training example about: {topic}\n\
        Format: a JSON object with 'instruction' and 'response' fields.\n\
        Style: {style}\n\
        The instruction should be a realistic user question/request.\n\
        The response should be a detailed, accurate answer.\n\
        Output ONLY the JSON object, no other text. No markdown."
    );

    let mut entries = Vec::new();
    let mut failures = 0;

    for i in 0..count {
        let prompt = if i == 0 {
            format!("Generate a training example about: {}", topic)
        } else {
            format!("Generate another unique training example about: {}. Example #{}/{}. Make it different from previous ones.", topic, i + 1, count)
        };

        let result = llm_call(
            endpoint,
            model,
            &prompt,
            Some(&system_prompt),
            Some(0.9),
            Some(1024),
            None,
            config,
        )
        .await;

        if result.success {
            if let Some(ActionOutput::LlmResponse { response, .. }) = &result.output {
                // Try to parse as JSON
                let trimmed = response.trim();
                let json_str = if trimmed.starts_with("```") {
                    // Strip markdown code blocks
                    trimmed
                        .lines()
                        .filter(|l| !l.starts_with("```"))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    trimmed.to_string()
                };

                match serde_json::from_str::<serde_json::Value>(&json_str) {
                    Ok(val)
                        if val.get("instruction").is_some() && val.get("response").is_some() =>
                    {
                        entries.push(val);
                    }
                    _ => {
                        // Try to extract JSON from the response
                        if let Some(start_idx) = json_str.find('{') {
                            if let Some(end_idx) = json_str.rfind('}') {
                                if let Ok(val) = serde_json::from_str::<serde_json::Value>(
                                    &json_str[start_idx..=end_idx],
                                ) {
                                    if val.get("instruction").is_some()
                                        && val.get("response").is_some()
                                    {
                                        entries.push(val);
                                        continue;
                                    }
                                }
                            }
                        }
                        failures += 1;
                    }
                }
            }
        } else {
            failures += 1;
        }
    }

    if entries.is_empty() {
        return ActionResult::err(
            "DatasetGenerate",
            ErrorKind::ExecutionFailed,
            format!(
                "Failed to generate any valid entries ({} failures)",
                failures
            ),
            start.elapsed().as_millis() as u64,
        );
    }

    // Write JSONL
    let mut jsonl = String::new();
    for entry in &entries {
        jsonl.push_str(&serde_json::to_string(entry).unwrap_or_default());
        jsonl.push('\n');
    }

    match confined_out
        .write_bytes(jsonl.as_bytes())
        .map_err(|e| std::io::Error::other(e.message()))
    {
        Ok(_) => ActionResult::ok(
            "DatasetGenerate",
            ActionOutput::DatasetCreated {
                path: output_path.to_string(),
                entries_count: entries.len(),
                failures,
                topic: topic.to_string(),
                size_bytes: jsonl.len() as u64,
            },
            start.elapsed().as_millis() as u64,
        ),
        Err(e) => ActionResult::err(
            "DatasetGenerate",
            ErrorKind::ExecutionFailed,
            format!("Write dataset: {}", e),
            start.elapsed().as_millis() as u64,
        ),
    }
}

/// Build an extraction tool with a cleared environment.
///
/// SECURITY: the single door for the six spawns in `document_extract`. It
/// exists as a function rather than six copies of two lines because the defect
/// class this repository keeps hitting is the site that was written later and
/// missed the pass — `podman cp` outlived the stdin fix of 2026-08-14 that way.
/// A new extraction tool that calls `tokio::process::Command::new` directly is
/// caught by `every_extraction_tool_goes_through_the_env_gate`.
///
/// These tools deserve the gate on their own merits: whisper and ffmpeg read
/// API keys and proxy credentials out of the environment when they find them,
/// and the file they are pointed at is chosen by the model.
fn extract_tool(bin: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(bin);
    crate::child_env::harden(&mut cmd);
    cmd
}

/// Extract text content from a document (PDF, video, audio, image, text) and
/// optionally convert it into a JSONL training dataset using an LLM.
///
/// Uses common CLI tools:
/// - PDF: `pdftotext` (poppler-utils)
/// - Video/Audio: `ffmpeg` (subtitle extraction / whisper)
/// - Image: `tesseract` (OCR)
/// - Text/Markdown/HTML: direct read
///
/// SECURITY: every one of those tools is spawned through `extract_tool`, so
/// none of them inherits the operator's environment.
pub async fn document_extract(
    input_path: &str,
    output_path: &str,
    format: &str, // "raw" (text only) or "dataset" (JSONL via LLM)
    llm_endpoint: Option<&str>,
    llm_model: Option<&str>,
    chunk_size: Option<usize>,
    config: &AppConfig,
) -> ActionResult {
    let start = std::time::Instant::now();
    // Reads `input_path` and writes `output_path`, both agent-chosen: confine
    // each against the matching root set before touching the disk.
    let confined_in = match crate::executor::confine::resolve_read(input_path, &config.policy) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("DocumentExtract", e.kind(), e.message(), 0),
    };
    let confined_out = match crate::executor::confine::resolve_write(output_path, &config.policy) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("DocumentExtract", e.kind(), e.message(), 0),
    };
    let path = confined_in.as_path();

    if !path.exists() {
        return ActionResult::err(
            "DocumentExtract",
            ErrorKind::NotFound,
            format!("File not found: {}", input_path),
            0,
        );
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Extract raw text based on file type
    let raw_text = match ext.as_str() {
        // Plain text formats — direct read
        "txt" | "md" | "rst" | "csv" | "tsv" | "json" | "jsonl" | "yaml" | "yml" |
        "toml" | "xml" | "html" | "htm" | "rs" | "py" | "js" | "ts" | "go" |
        "c" | "cpp" | "h" | "java" | "rb" | "sh" | "sql" | "lua" | "zig" => {
            match confined_in
                .read_bytes()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .map_err(|e| std::io::Error::other(e.message()))
            {
                Ok(content) => content,
                Err(e) => return ActionResult::err("DocumentExtract", ErrorKind::ExecutionFailed,
                    format!("Read file: {}", e), start.elapsed().as_millis() as u64),
            }
        }

        // PDF — use pdftotext
        "pdf" => {
            let output = extract_tool("pdftotext")
                .args(["-layout", input_path, "-"])
                .output().await;
            match output {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
                Ok(o) => return ActionResult::err("DocumentExtract", ErrorKind::MissingDependency,
                    format!("pdftotext failed: {}", String::from_utf8_lossy(&o.stderr)), start.elapsed().as_millis() as u64),
                Err(_) => return ActionResult::err("DocumentExtract", ErrorKind::MissingDependency,
                    "pdftotext not found. Install poppler-utils: apt install poppler-utils", start.elapsed().as_millis() as u64),
            }
        }

        // Video — extract subtitles with ffmpeg
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" => {
            // First try embedded subtitles
            let sub_output = extract_tool("ffmpeg")
                .args(["-i", input_path, "-map", "0:s:0", "-f", "srt", "-"])
                .output().await;
            match sub_output {
                Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                    // Parse SRT: strip timestamps, keep text
                    let srt = String::from_utf8_lossy(&o.stdout);
                    srt.lines()
                        .filter(|l| !l.trim().is_empty() && !l.contains("-->") && l.trim().parse::<u32>().is_err())
                        .collect::<Vec<_>>()
                        .join("\n")
                }
                _ => {
                    // No subtitles — try whisper via ffmpeg + pipe
                    // Extract audio to wav first
                    let tmp_wav = format!("/tmp/x0ne_extract_{}.wav", std::process::id());
                    let audio = extract_tool("ffmpeg")
                        .args(["-i", input_path, "-ac", "1", "-ar", "16000", "-y", &tmp_wav])
                        .output().await;
                    match audio {
                        Ok(o) if o.status.success() => {
                            // Try whisper CLI
                            let whisper = extract_tool("whisper")
                                .args([&tmp_wav, "--model", "base", "--output_format", "txt", "--output_dir", "/tmp"])
                                .output().await;
                            let _ = std::fs::remove_file(&tmp_wav);
                            match whisper {
                                Ok(w) if w.status.success() => {
                                    let txt_path = tmp_wav.replace(".wav", ".txt");
                                    std::fs::read_to_string(&txt_path).unwrap_or_else(|_| {
                                        String::from_utf8_lossy(&w.stdout).to_string()
                                    })
                                }
                                _ => return ActionResult::err("DocumentExtract", ErrorKind::MissingDependency,
                                    "No subtitles found and whisper not available. Install: pip install openai-whisper", start.elapsed().as_millis() as u64),
                            }
                        }
                        _ => return ActionResult::err("DocumentExtract", ErrorKind::MissingDependency,
                            "ffmpeg not found. Install: apt install ffmpeg", start.elapsed().as_millis() as u64),
                    }
                }
            }
        }

        // Audio — transcribe with whisper
        "mp3" | "wav" | "ogg" | "flac" | "m4a" | "aac" => {
            let whisper = extract_tool("whisper")
                .args([input_path, "--model", "base", "--output_format", "txt", "--output_dir", "/tmp"])
                .output().await;
            match whisper {
                Ok(w) if w.status.success() => {
                    let txt_path = format!("/tmp/{}.txt", path.file_stem().unwrap_or_default().to_str().unwrap_or("audio"));
                    std::fs::read_to_string(&txt_path).unwrap_or_else(|_| {
                        String::from_utf8_lossy(&w.stdout).to_string()
                    })
                }
                Ok(w) => return ActionResult::err("DocumentExtract", ErrorKind::ExecutionFailed,
                    format!("whisper failed: {}", String::from_utf8_lossy(&w.stderr)), start.elapsed().as_millis() as u64),
                Err(_) => return ActionResult::err("DocumentExtract", ErrorKind::MissingDependency,
                    "whisper not found. Install: pip install openai-whisper", start.elapsed().as_millis() as u64),
            }
        }

        // Image — OCR with tesseract
        "png" | "jpg" | "jpeg" | "bmp" | "tiff" | "tif" | "webp" => {
            let ocr = extract_tool("tesseract")
                .args([input_path, "stdout"])
                .output().await;
            match ocr {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
                Ok(o) => return ActionResult::err("DocumentExtract", ErrorKind::ExecutionFailed,
                    format!("tesseract failed: {}", String::from_utf8_lossy(&o.stderr)), start.elapsed().as_millis() as u64),
                Err(_) => return ActionResult::err("DocumentExtract", ErrorKind::MissingDependency,
                    "tesseract not found. Install: apt install tesseract-ocr", start.elapsed().as_millis() as u64),
            }
        }

        _ => return ActionResult::err("DocumentExtract", ErrorKind::InvalidInput,
            format!("Unsupported format: .{}. Supported: pdf, mp4, mkv, mp3, wav, png, jpg, txt, md, html, rs, py...", ext),
            0),
    };

    if raw_text.trim().is_empty() {
        return ActionResult::err(
            "DocumentExtract",
            ErrorKind::ExecutionFailed,
            "Extracted text is empty",
            start.elapsed().as_millis() as u64,
        );
    }

    // If format is "raw", just write the extracted text
    if format == "raw" {
        match confined_out
            .write_bytes(raw_text.as_bytes())
            .map_err(|e| std::io::Error::other(e.message()))
        {
            Ok(_) => {
                return ActionResult::ok(
                    "DocumentExtract",
                    ActionOutput::DocumentExtracted {
                        input_path: input_path.to_string(),
                        output_path: output_path.to_string(),
                        format: "raw".into(),
                        entries_count: 0,
                        text_length: raw_text.len(),
                        source_type: ext.clone(),
                    },
                    start.elapsed().as_millis() as u64,
                )
            }
            Err(e) => {
                return ActionResult::err(
                    "DocumentExtract",
                    ErrorKind::ExecutionFailed,
                    format!("Write output: {}", e),
                    start.elapsed().as_millis() as u64,
                )
            }
        }
    }

    // format == "dataset": chunk the text and convert to JSONL via LLM
    let endpoint = match llm_endpoint {
        Some(e) => e,
        None => {
            return ActionResult::err(
                "DocumentExtract",
                ErrorKind::InvalidInput,
                "dataset format requires llm_endpoint",
                0,
            )
        }
    };
    let model = match llm_model {
        Some(m) => m,
        None => {
            return ActionResult::err(
                "DocumentExtract",
                ErrorKind::InvalidInput,
                "dataset format requires llm_model",
                0,
            )
        }
    };

    let chunk_size = chunk_size.unwrap_or(2000);
    let chunks: Vec<&str> = raw_text
        .as_bytes()
        .chunks(chunk_size)
        .filter_map(|c| std::str::from_utf8(c).ok())
        .collect();

    let system_prompt = "You are a training data generator. Given a text chunk, \
        generate 3-5 instruction/response training pairs based on the content. \
        Output a JSON array of objects, each with 'instruction' and 'response' fields. \
        Output ONLY the JSON array, no other text.";

    let mut all_entries = Vec::new();
    let mut failures = 0;

    for chunk in &chunks {
        if chunk.trim().len() < 50 {
            continue;
        } // Skip tiny chunks

        let prompt = format!("Generate training pairs from this text:\n\n{}", chunk);
        let result = llm_call(
            endpoint,
            model,
            &prompt,
            Some(system_prompt),
            Some(0.7),
            Some(2048),
            None,
            config,
        )
        .await;

        if result.success {
            if let Some(ActionOutput::LlmResponse { response, .. }) = &result.output {
                let trimmed = response.trim();
                // Try to parse as JSON array
                let json_str = if trimmed.starts_with("```") {
                    trimmed
                        .lines()
                        .filter(|l| !l.starts_with("```"))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    trimmed.to_string()
                };

                if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&json_str) {
                    for val in arr {
                        if val.get("instruction").is_some() && val.get("response").is_some() {
                            all_entries.push(val);
                        }
                    }
                } else if let Some(start_idx) = json_str.find('[') {
                    if let Some(end_idx) = json_str.rfind(']') {
                        if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(
                            &json_str[start_idx..=end_idx],
                        ) {
                            for val in arr {
                                if val.get("instruction").is_some() && val.get("response").is_some()
                                {
                                    all_entries.push(val);
                                }
                            }
                        } else {
                            failures += 1;
                        }
                    }
                } else {
                    failures += 1;
                }
            }
        } else {
            failures += 1;
        }
    }

    if all_entries.is_empty() {
        return ActionResult::err(
            "DocumentExtract",
            ErrorKind::ExecutionFailed,
            format!(
                "Failed to generate dataset entries from {} chunks ({} failures)",
                chunks.len(),
                failures
            ),
            start.elapsed().as_millis() as u64,
        );
    }

    // Write JSONL
    let mut jsonl = String::new();
    for entry in &all_entries {
        jsonl.push_str(&serde_json::to_string(entry).unwrap_or_default());
        jsonl.push('\n');
    }

    match confined_out
        .write_bytes(jsonl.as_bytes())
        .map_err(|e| std::io::Error::other(e.message()))
    {
        Ok(_) => ActionResult::ok(
            "DocumentExtract",
            ActionOutput::DocumentExtracted {
                input_path: input_path.to_string(),
                output_path: output_path.to_string(),
                format: "dataset".into(),
                entries_count: all_entries.len(),
                text_length: raw_text.len(),
                source_type: ext,
            },
            start.elapsed().as_millis() as u64,
        ),
        Err(e) => ActionResult::err(
            "DocumentExtract",
            ErrorKind::ExecutionFailed,
            format!("Write dataset: {}", e),
            start.elapsed().as_millis() as u64,
        ),
    }
}

/// Multi-turn chat with Ollama models, with optional vision (images).
///
/// Uses Ollama's `/api/chat` endpoint with a full message array.
/// Each message can optionally carry base64-encoded images for multimodal
/// models (llava, bakllava, etc.).
pub async fn ollama_chat(
    endpoint: &str,
    model: &str,
    messages: &[crate::action::ChatMessage],
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    auth: Option<&str>,
    _config: &AppConfig,
) -> ActionResult {
    // SECURITY (SSRF): block metadata/link-local/RFC1918; loopback allowed (local Ollama).
    let ssrf = match crate::executor::system::check_ssrf_llm(endpoint) {
        Ok(s) => s,
        Err(reason) => {
            return ActionResult::err(
                "OllamaChat",
                ErrorKind::InvalidInput,
                format!("endpoint blocked: {}", reason),
                0,
            )
        }
    };
    let client = match crate::executor::system::build_pinned_client(
        endpoint,
        &ssrf,
        std::time::Duration::from_secs(300),
    ) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("OllamaChat", ErrorKind::Internal, e, 0),
    };

    let base = endpoint.trim_end_matches('/');
    let start = std::time::Instant::now();

    // Build messages array — each message optionally carries images (base64)
    let api_messages: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            let mut msg = serde_json::json!({
                "role": m.role,
                "content": m.content,
            });
            if !m.images.is_empty() {
                msg["images"] = serde_json::json!(m.images);
            }
            msg
        })
        .collect();

    let mut body = serde_json::json!({
        "model": model,
        "messages": api_messages,
        "stream": false,
    });
    if let Some(temp) = temperature {
        body["options"] = serde_json::json!({"temperature": temp});
    }
    if let Some(max) = max_tokens {
        // Ollama uses num_predict for max tokens
        if let Some(opts) = body.get_mut("options").and_then(|o| o.as_object_mut()) {
            opts.insert("num_predict".into(), serde_json::Value::from(max));
        } else {
            body["options"] = serde_json::json!({"num_predict": max});
        }
    }

    let url = format!("{}/api/chat", base);

    let token = match resolve_auth(auth, endpoint) {
        Ok(t) => t,
        Err(e) => return ActionResult::err("OllamaChat", ErrorKind::InvalidInput, e, 0),
    };
    let mut req = client.post(&url).json(&body);
    if let Some(tok) = &token {
        req = apply_auth(req, endpoint, tok);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let base_msg = format!("Request failed: {}", e);
            let hint = sandbox_localhost_hint(endpoint, &e);
            let msg = if let Some(h) = hint {
                format!("{base_msg}\n\n💡 hint: {h}")
            } else {
                base_msg
            };
            return ActionResult::err(
                "OllamaChat",
                ErrorKind::ExecutionFailed,
                msg,
                start.elapsed().as_millis() as u64,
            );
        }
    };

    let status = resp.status().as_u16();
    let resp_body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            return ActionResult::err(
                "OllamaChat",
                ErrorKind::ExecutionFailed,
                format!("Read body: {}", e),
                start.elapsed().as_millis() as u64,
            )
        }
    };

    if status >= 400 {
        let preview: String = resp_body.chars().take(500).collect();
        return ActionResult::err(
            "OllamaChat",
            ErrorKind::ExecutionFailed,
            format!("API returned {}: {}", status, preview),
            start.elapsed().as_millis() as u64,
        );
    }

    let parsed: serde_json::Value = match serde_json::from_str(&resp_body) {
        Ok(v) => v,
        Err(e) => {
            return ActionResult::err(
                "OllamaChat",
                ErrorKind::ExecutionFailed,
                format!("Parse JSON: {}", e),
                start.elapsed().as_millis() as u64,
            )
        }
    };

    let response_text = parsed
        .pointer("/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let tokens_used = parsed["eval_count"].as_u64().unwrap_or(0);
    let duration = start.elapsed().as_millis() as u64;

    ActionResult::ok(
        "OllamaChat",
        ActionOutput::LlmResponse {
            model: model.to_string(),
            response: response_text,
            tokens_used,
        },
        duration,
    )
}

// ── Sandbox-aware error hints ────────────────────────────────────────────────
//
// When `x0ne-runtime` runs inside a sandbox container (Incus, Podman,
// Docker) and a user action tries to hit `localhost:<port>` on the
// **host**, the request simply fails with a cryptic
// `Request failed: error sending request` or `connection refused`. The
// symptom is indistinguishable from "endpoint unreachable" in general —
// but the root cause is specific (container loopback != host loopback)
// and the fix is specific (use the host bridge, or point at
// `host.containers.internal` / `10.0.2.2` / the host's LAN IP).
//
// These helpers generate an actionable hint that we prepend to the raw
// reqwest error. We deliberately keep the hint best-effort (it's wrong
// to be wrong here would be worse than saying nothing), so we only fire
// when we're highly confident:
//   - request was sent to 127.0.0.1 / localhost / ::1 / 0.0.0.0, AND
//   - the error looks like a connect failure, AND
//   - we detect container-ish markers in the filesystem.

/// Return an actionable hint if the reqwest error was a connect failure
/// to `localhost` / `127.0.0.1` / `::1` and we can detect that we're
/// running inside a sandbox container. Returns `None` in the common
/// case (no hint available / conditions not met) so the caller can
/// decide whether to append it to the raw error.
pub(crate) fn sandbox_localhost_hint(endpoint: &str, err: &reqwest::Error) -> Option<String> {
    // 1. Endpoint points at loopback (or 0.0.0.0, which is semantically
    //    loopback for outbound connections from a container).
    let lower = endpoint.to_lowercase();
    let loopback = [
        "http://localhost",
        "https://localhost",
        "http://127.0.0.1",
        "https://127.0.0.1",
        "http://[::1]",
        "https://[::1]",
        "http://0.0.0.0",
        "https://0.0.0.0",
    ];
    if !loopback.iter().any(|p| lower.starts_with(p)) {
        return None;
    }

    // 2. Error is a connection-level failure. `reqwest::Error` exposes
    //    `is_connect()`; anything else (timeout, decode, etc.) means the
    //    service answered — just not the way we wanted — which is a
    //    different problem.
    if !err.is_connect() {
        return None;
    }

    // 3. We're inside a sandbox. Fast heuristic: check for the standard
    //    marker files. We avoid parsing `/proc/1/cgroup` because the
    //    format varies across cgroup v1/v2 and container runtimes, and
    //    false negatives are fine (we just won't add a hint).
    if !running_in_sandbox() {
        return None;
    }

    Some(format!(
        "endpoint '{}' points at sandbox-local loopback, not the host's loopback. \
         From inside x0ne-runtime's sandbox, the host's Ollama (or any other \
         localhost service) is reachable via:\n\
          • set `OLLAMA_HOST=http://host.containers.internal:<port>` (Podman/Docker), OR\n\
          • set `OLLAMA_HOST=http://<host-LAN-ip>:<port>` (Incus — `incus network show incusbr0`), OR\n\
          • run the orchestrator's LLM on the host bridge (config: `bridge.mode=\"stdio\"`).",
        endpoint,
    ))
}

/// Detect whether this process is inside a container. Cheap, heuristic,
/// false-negative-safe: false positives would produce a misleading hint.
/// We check the three well-known marker files that cover Docker, Podman,
/// and distroless-style images:
/// * `/.dockerenv` — Docker.
/// * `/run/.containerenv` — Podman / libpod.
/// * `/run/systemd/container` — systemd-in-container, which covers Incus
///   and systemd-nspawn setups.
fn running_in_sandbox() -> bool {
    std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
        || std::path::Path::new("/run/systemd/container").exists()
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    #[test]
    fn resolve_none_or_empty_returns_none() {
        assert!(resolve_auth(None, LOCAL).unwrap().is_none());
        assert!(resolve_auth(Some(""), LOCAL).unwrap().is_none());
    }

    const LOCAL: &str = "http://127.0.0.1:11434";

    #[test]
    fn resolve_literal_returns_token_verbatim() {
        assert_eq!(
            resolve_auth(Some("sk-ant-literal-123"), "https://api.anthropic.com").unwrap(),
            Some("sk-ant-literal-123".to_string())
        );
        assert_eq!(
            resolve_auth(Some("local-token"), LOCAL).unwrap(),
            Some("local-token".to_string())
        );
    }

    #[test]
    fn resolve_env_ref_reads_runtime_env() {
        // SAFETY: tests in this module are not run in parallel against each
        // other for the same var; we use a unique name per test.
        std::env::set_var("X0NE_TEST_AUTH_KEY_RESOLVES", "secret-token");
        let r = resolve_auth(Some("$ENV:X0NE_TEST_AUTH_KEY_RESOLVES"), LOCAL).unwrap();
        assert_eq!(r, Some("secret-token".to_string()));
        std::env::remove_var("X0NE_TEST_AUTH_KEY_RESOLVES");
    }

    #[test]
    fn resolve_env_ref_missing_returns_none() {
        std::env::remove_var("X0NE_TEST_AUTH_KEY_MISSING");
        // Missing env var is *not* an error — we silently skip auth so the
        // same JSON works on auth'd and non-auth'd endpoints.
        assert!(resolve_auth(Some("$ENV:X0NE_TEST_AUTH_KEY_MISSING"), LOCAL)
            .unwrap()
            .is_none());
    }

    #[test]
    fn resolve_env_ref_blank_name_errors() {
        let e = resolve_auth(Some("$ENV:"), LOCAL).unwrap_err();
        assert!(e.contains("requires a variable name"));
    }

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    const VENDOR_ENV: &[(&str, &str)] = &[
        ("ANTHROPIC_API_KEY", "sk-ant-api03-FAKE-abcd"),
        ("OPENAI_API_KEY", "sk-proj-FAKE-1014-wxyz"),
        ("OLLAMA_API_KEY", "ollama-FAKE-1014-key"),
        ("MY_LLM_KEY", "named-FAKE-1014-key"),
    ];

    /// Review of the pre-release build (S1): the model named the variable and the host, and
    /// the key went there. A vendor's variable now goes to its hosts only.
    #[test]
    fn a_vendor_variable_reaches_its_own_hosts_only() {
        let e = env(VENDOR_ENV);
        for (var, home) in [
            ("ANTHROPIC_API_KEY", "https://api.anthropic.com"),
            ("OPENAI_API_KEY", "https://api.openai.com/v1"),
            ("OLLAMA_API_KEY", "https://ollama.com"),
        ] {
            let spec = format!("$ENV:{var}");
            assert!(
                resolve_auth_from(Some(&spec), home, &e).unwrap().is_some(),
                "{var} to {home}"
            );
            for away in [
                LOCAL,
                "http://127.0.0.1:4000/v1",
                "https://evil.example/v1",
                "https://evil.example\\@api.openai.com",
                home.replace("https://", "http://").as_str(),
            ] {
                let msg = resolve_auth_from(Some(&spec), away, &e).unwrap_err();
                assert!(msg.contains("auth refused"), "{var} to {away}: {msg}");
                assert!(!msg.contains("FAKE"), "the key shows: {msg}");
            }
        }
    }

    /// A key that holds `sk-ant-` goes to Anthropic only, whatever names it.
    #[test]
    fn an_anthropic_key_goes_to_anthropic_only_whatever_names_it() {
        let e = env(&[("MY_ANT", "sk-ant-api03-FAKE-abcd")]);
        for spec in [
            "$ENV:MY_ANT",
            "sk-ant-api03-FAKE-abcd",
            "Bearer sk-ant-api03-FAKE-abcd",
            "\u{feff}sk-ant-api03-FAKE-abcd",
        ] {
            assert!(
                resolve_auth_from(Some(spec), LOCAL, &e).is_err(),
                "{spec:?}"
            );
            assert!(
                resolve_auth_from(Some(spec), "https://api.openai.com", &e).is_err(),
                "{spec:?}"
            );
        }
        assert!(resolve_auth_from(Some("$ENV:MY_ANT"), "https://api.anthropic.com", &e).is_ok());
    }

    /// A variable outside the table is bound to no host: the operator's
    /// prompt (`readonly_net` keeps it for an action with `auth`) decides.
    #[test]
    fn a_variable_outside_the_table_follows_the_endpoint() {
        let e = env(VENDOR_ENV);
        assert_eq!(
            resolve_auth_from(Some("$ENV:MY_LLM_KEY"), LOCAL, &e).unwrap(),
            Some("named-FAKE-1014-key".to_string())
        );
        // A vendor host still takes its own format only.
        assert!(resolve_auth_from(Some("$ENV:MY_LLM_KEY"), "https://api.openai.com", &e).is_err());
    }

    #[test]
    fn apply_auth_anthropic_uses_x_api_key() {
        // Build a dummy request and inspect its headers via reqwest::Request.
        let client = reqwest::Client::new();
        let b = client.post("https://api.anthropic.com/v1/messages");
        let req = apply_auth(b, "https://api.anthropic.com", "key-123")
            .build()
            .unwrap();
        let h = req.headers();
        assert_eq!(
            h.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("key-123")
        );
        assert_eq!(
            h.get("anthropic-version").and_then(|v| v.to_str().ok()),
            Some("2023-06-01")
        );
        assert!(
            h.get("Authorization").is_none(),
            "Anthropic must not get Bearer"
        );
    }

    // Host-based detection — `is_anthropic_endpoint` must accept the
    // canonical API host (any case) AND any subdomain of anthropic.com,
    // but reject any URL where `anthropic.com` appears only in the path
    // or query (the trick the original E2E test used to leaned on, which
    // would be a misclassification trap if left in).
    #[test]
    fn is_anthropic_endpoint_matches_canonical_host() {
        assert!(is_anthropic_endpoint("https://api.anthropic.com"));
        assert!(is_anthropic_endpoint(
            "https://api.anthropic.com/v1/messages"
        ));
        assert!(is_anthropic_endpoint(
            "https://API.ANTHROPIC.COM/v1/messages"
        ));
        assert!(is_anthropic_endpoint("http://api.anthropic.com:443"));
    }

    #[test]
    fn is_anthropic_endpoint_matches_subdomains() {
        // Any *.anthropic.com host is also accepted (regional shards,
        // testing subdomains, …).
        assert!(is_anthropic_endpoint("https://gateway.anthropic.com"));
        assert!(is_anthropic_endpoint(
            "https://eu.api.anthropic.com/v1/messages"
        ));
    }

    #[test]
    fn is_anthropic_endpoint_rejects_loopback_and_path_smuggling() {
        // Loopback with `anthropic.com` in the path is NOT Anthropic —
        // it's the agent talking to its own sandbox. Mis-classifying
        // here would route a private call as Anthropic and (worse)
        // surface a forwarded `x-api-key` to whatever process is
        // listening on that loopback port.
        assert!(!is_anthropic_endpoint(
            "http://127.0.0.1:7800/anthropic.com"
        ));
        assert!(!is_anthropic_endpoint("http://localhost/v1/anthropic.com"));
        // Look-alike domains must NOT match.
        assert!(!is_anthropic_endpoint("https://anthropic.com.evil.example"));
        assert!(!is_anthropic_endpoint("https://notanthropic.com"));
        // Non-URL / garbage falls through to OpenAI-compat.
        assert!(!is_anthropic_endpoint("not a url at all"));
        assert!(!is_anthropic_endpoint(""));
    }

    // Body shape — pure-function tests against `build_llm_body`. These
    // replace the previous E2E test that planted `anthropic.com` in a
    // URL path (incorrectly accepted by the old substring check, now
    // correctly rejected by the host-aware helper).
    #[test]
    fn build_llm_body_anthropic_shape() {
        let body = build_llm_body(
            "anthropic",
            "claude-sonnet-4-6",
            "say hi",
            Some("be terse"),
            Some(0.2),
            Some(64),
        );
        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["max_tokens"], 64);
        assert_eq!(body["system"], "be terse");
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "say hi");
        // Anthropic must NOT carry the OpenAI-compat `messages[].role=system`
        // entry — system goes at the top level. Defensive assertion in case
        // somebody refactors the helper.
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn build_llm_body_anthropic_default_max_tokens() {
        // max_tokens is REQUIRED by Anthropic — when the caller didn't
        // supply one, the helper must fill in a sensible default rather
        // than emit a body Anthropic will 400.
        let body = build_llm_body("anthropic", "claude-x", "hi", None, None, None);
        assert_eq!(body["max_tokens"], 1024);
        assert!(body.get("system").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn build_llm_body_openai_shape() {
        let body = build_llm_body(
            "openai",
            "gpt-4",
            "say hi",
            Some("be terse"),
            Some(0.5),
            Some(128),
        );
        // OpenAI puts system inside messages[].
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "be terse");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["max_tokens"], 128);
        // Anthropic's top-level `system` field MUST NOT appear here.
        assert!(body.get("system").is_none());
    }

    #[test]
    fn build_llm_url_per_flavor() {
        assert_eq!(
            build_llm_url("anthropic", "https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            build_llm_url("ollama", "http://localhost:11434/"),
            "http://localhost:11434/api/generate"
        );
        assert_eq!(
            build_llm_url("openai", "https://api.openai.com"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    // Regression test for the false-positive that the original code
    // produced: a loopback URL whose path contains `anthropic.com` must
    // NOT route through the Anthropic flavour. We stand up a TCP listener
    // on 127.0.0.1 and confirm the bytes off the wire are OpenAI-compat
    // shape with `Authorization: Bearer …`, not Anthropic headers.
    #[tokio::test]
    async fn llm_call_loopback_with_anthropic_in_path_is_openai_flavor() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            // Minimal OpenAI-compat response — we don't care about the
            // body, only that the request side classifies correctly.
            let body = r#"{"choices":[{"message":{"content":"ok"}}],"usage":{"total_tokens":1}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body,
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
            req
        });

        std::env::set_var("X0NE_TEST_PATH_SMUGGLE_KEY", "key-xyz");
        let cfg = crate::config::AppConfig::defaults();
        // Same URL the OLD test used — `anthropic.com` smuggled into the
        // path. Under the host-aware classifier this MUST route as OpenAI.
        let endpoint = format!("http://127.0.0.1:{}/anthropic.com", port);
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            llm_call(
                &endpoint,
                "claude-test",
                "hi",
                None,
                None,
                None,
                Some("$ENV:X0NE_TEST_PATH_SMUGGLE_KEY"),
                &cfg,
            ),
        )
        .await;

        let req_bytes = server.await.unwrap();
        // OpenAI-compat URL — confirms the flavor picker used host, not path.
        assert!(
            req_bytes.contains("POST /anthropic.com/v1/chat/completions"),
            "expected OpenAI-compat URL on loopback, got: {req_bytes}"
        );
        // OpenAI-compat headers — Bearer, NOT x-api-key.
        assert!(
            req_bytes
                .to_lowercase()
                .contains("authorization: bearer key-xyz"),
            "expected Bearer auth on loopback, got: {req_bytes}"
        );
        assert!(
            !req_bytes.to_lowercase().contains("x-api-key"),
            "x-api-key must NOT leak to a loopback service: {req_bytes}"
        );
        std::env::remove_var("X0NE_TEST_PATH_SMUGGLE_KEY");
    }

    #[test]
    fn apply_auth_openai_compat_uses_bearer() {
        let client = reqwest::Client::new();
        let b = client.post("https://api.openai.com/v1/chat/completions");
        let req = apply_auth(b, "https://api.openai.com", "sk-openai")
            .build()
            .unwrap();
        let h = req.headers();
        assert_eq!(
            h.get("Authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer sk-openai")
        );
        assert!(
            h.get("x-api-key").is_none(),
            "OpenAI must not get x-api-key"
        );
    }
}

#[cfg(test)]
mod sandbox_hint_tests {
    use super::*;

    /// Non-loopback endpoint → no hint (hint only applies to localhost).
    #[tokio::test]
    async fn no_hint_for_remote_endpoint() {
        // Build an error by trying to reach a dead remote.
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_millis(100))
            .build()
            .unwrap();
        let r = client.get("http://10.255.255.1:1/").send().await;
        if let Err(e) = r {
            assert!(sandbox_localhost_hint("http://example.com:11434", &e).is_none());
        }
    }

    /// Non-connect error (URL parse failure isn't a connect error) → no hint.
    #[tokio::test]
    async fn no_hint_when_not_connect_error() {
        // Invalid URL produces a non-connect error path via reqwest.
        let client = reqwest::Client::new();
        let r = client.get("http://").send().await;
        if let Err(e) = r {
            // Should be a builder/url error, not a connect error.
            assert!(sandbox_localhost_hint("http://127.0.0.1:11434", &e).is_none());
        }
    }
}
