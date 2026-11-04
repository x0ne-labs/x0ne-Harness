//! Real LLM backend: Anthropic Messages API over reqwest (the workspace's
//! HTTP client, rustls). Configuration comes from the environment only.
//! Hard money cap: before each call the input is counted and the worst case
//! (input + max_tokens of output) must fit in the node's remaining budget,
//! so the tree's `sum(spent) <= root budget` holds for real dollars too.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::budget::Budget;
use crate::caps::{CapSet, Capability};
use crate::llm::{Decision, LlmBackend, LlmError, Observation, Usage};
use crate::node::{ModelTier, NodeId, NodeView, TaskSpec};
use crate::report::{bound, Report};

pub const DEFAULT_MODEL: &str = "claude-haiku-4-5-20251001";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
/// Smallest useful reply; below this the call is refused for budget.
pub const MIN_OUTPUT_TOKENS: u64 = 256;
const MAX_OBSERVATION_CHARS: usize = 6000;

/// Prices in nano-USD per token (= $/MTok x 1000), integers so costs are exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Price {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
}

impl Price {
    /// Haiku 4.5 list price: 1 $/MTok in, 5 out (cache write 1.25x, read 0.1x).
    pub const HAIKU_4_5: Price = Price {
        input: 1000,
        output: 5000,
        cache_write: 1250,
        cache_read: 100,
    };

    fn micro_usd(&self, input: u64, output: u64, cache_write: u64, cache_read: u64) -> u64 {
        let nano = input * self.input
            + output * self.output
            + cache_write * self.cache_write
            + cache_read * self.cache_read;
        nano.div_ceil(1000)
    }
}

/// Never printed, never serialised.
#[derive(Clone)]
struct ApiKey(String);

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey([REDACTED])")
    }
}

#[derive(Clone, Debug)]
pub struct AnthropicConfig {
    pub model: String,
    pub base_url: String,
    pub proxy: Option<String>,
    pub max_tokens: u64,
    pub timeout: Duration,
    /// Extra attempts on 429/5xx/529 and transport errors.
    pub retries: u32,
    pub retry_base: Duration,
    /// Deepest node allowed to spawn children is `max_depth - 1`.
    pub max_depth: Option<usize>,
    pub price: Price,
    api_key: ApiKey,
}

fn parse_price(v: &str, name: &str) -> Result<u64, String> {
    let x: f64 = v
        .trim()
        .parse()
        .map_err(|_| format!("{name} is not a number ($/MTok)"))?;
    if !(x.is_finite() && x >= 0.0) {
        return Err(format!("{name} must be >= 0"));
    }
    Ok((x * 1000.0).round() as u64)
}

impl AnthropicConfig {
    /// ANTHROPIC_API_KEY (required), X0NE_LLM_MODEL, ANTHROPIC_BASE_URL,
    /// HTTPS_PROXY / https_proxy, X0NE_LLM_PRICE_IN / X0NE_LLM_PRICE_OUT.
    pub fn from_env() -> Result<Self, String> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let nonempty = |k: &str| get(k).filter(|v| !v.trim().is_empty());
        let key = nonempty("ANTHROPIC_API_KEY").ok_or("ANTHROPIC_API_KEY is not set")?;
        let model = nonempty("X0NE_LLM_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let base_url = nonempty("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let proxy = nonempty("HTTPS_PROXY").or_else(|| nonempty("https_proxy"));
        let price = match (nonempty("X0NE_LLM_PRICE_IN"), nonempty("X0NE_LLM_PRICE_OUT")) {
            (Some(i), Some(o)) => {
                let input = parse_price(&i, "X0NE_LLM_PRICE_IN")?;
                Price {
                    input,
                    output: parse_price(&o, "X0NE_LLM_PRICE_OUT")?,
                    cache_write: input * 5 / 4,
                    cache_read: input / 10,
                }
            }
            _ if model.starts_with("claude-haiku-4-5") => Price::HAIKU_4_5,
            _ => {
                return Err(format!(
                    "no price known for model {model}: set X0NE_LLM_PRICE_IN and X0NE_LLM_PRICE_OUT ($/MTok)"
                ))
            }
        };
        Ok(AnthropicConfig {
            model,
            base_url,
            proxy,
            max_tokens: 2048,
            timeout: Duration::from_secs(120),
            retries: 2,
            retry_base: Duration::from_millis(800),
            max_depth: None,
            price,
            api_key: ApiKey(key),
        })
    }

    /// Replace the key (and any `sk-ant-` token) in text that leaves this module.
    pub fn scrub(&self, s: &str) -> String {
        let mut out = if self.api_key.0.is_empty() {
            s.to_string()
        } else {
            s.replace(&self.api_key.0, "[REDACTED]")
        };
        while let Some(i) = out.find("sk-ant-") {
            let tail = &out[i + 7..];
            let n = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                .unwrap_or(tail.len());
            out.replace_range(i..i + 7 + n, "[REDACTED]");
        }
        out
    }
}

/// Real spend as reported by the provider, over the whole tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Totals {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
    pub micro_usd: u64,
    pub refused_for_budget: u64,
    pub errors: u64,
}

struct Turn {
    content: Value,
    tool_use_id: String,
}

pub struct AnthropicLlm {
    cfg: AnthropicConfig,
    client: reqwest::Client,
    history: Mutex<HashMap<NodeId, Vec<Turn>>>,
    totals: Mutex<Totals>,
}

impl std::fmt::Debug for AnthropicLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AnthropicLlm {{ model: {}, base_url: {} }}",
            self.cfg.model, self.cfg.base_url
        )
    }
}

impl AnthropicLlm {
    pub fn new(cfg: AnthropicConfig) -> Result<Self, String> {
        let mut b = reqwest::Client::builder().timeout(cfg.timeout);
        b = match &cfg.proxy {
            Some(p) => b.proxy(reqwest::Proxy::all(p).map_err(|e| format!("proxy: {e}"))?),
            None => b.no_proxy(),
        };
        let client = b.build().map_err(|e| format!("http client: {e}"))?;
        Ok(AnthropicLlm {
            cfg,
            client,
            history: Mutex::new(HashMap::new()),
            totals: Mutex::new(Totals::default()),
        })
    }

    pub fn config(&self) -> &AnthropicConfig {
        &self.cfg
    }

    pub fn totals(&self) -> Totals {
        *self.totals.lock().expect("totals lock")
    }

    fn bump(&self, f: impl FnOnce(&mut Totals)) {
        f(&mut self.totals.lock().expect("totals lock"));
    }

    fn can_spawn(&self, view: &NodeView) -> bool {
        view.caps.has(Capability::Spawn)
            && self.cfg.max_depth.map(|m| view.depth < m).unwrap_or(true)
    }

    fn system(&self, view: &NodeView) -> String {
        let depth = match self.cfg.max_depth {
            Some(m) => format!(" (max depth {m})"),
            None => String::new(),
        };
        let spawn = if self.can_spawn(view) {
            "- spawn: delegate a clearly separable part to a child: a precise statement and a verify command (exit 0 = done, e.g. `python3 -m unittest -q`). The child starts from a copy of your files. Then call wait: you get the children's reports, and their verified files are merged into yours.\nDelegate only independent parts; do small things yourself.\n"
        } else {
            "You cannot spawn: do the work yourself.\n"
        };
        format!(
            "You are node {id} at depth {d}{depth} of a tree of coding sub-agents. You work in a private copy of your parent's files. Act only through tools, exactly one tool call per turn.\n\
- write/read: paths relative to your work dir.\n\
- exec: /bin/sh in your work dir. No network. python3 with the standard library only (no pip).\n\
{spawn}\
- report: when done and checked, one short paragraph. A verify command runs after you report; success is measured, not claimed.\n\
Cost: each turn re-reads the conversation (1 micro-USD per input token, 5 per output token with Haiku); a small leaf task takes 5 to 8 turns. Lend children enough and keep enough to wait and report.\n\
Keep code small and finish fast. Budget left: {usd} micro-USD, {a} actions.",
            id = view.id,
            d = view.depth,
            usd = view.remaining.micro_usd,
            a = view.remaining.actions,
        )
    }

    fn tools(&self, view: &NodeView) -> Value {
        let mut t = vec![
            json!({"name": "write", "description": "Create or overwrite a text file in your work dir.",
                "input_schema": {"type": "object", "properties": {
                    "path": {"type": "string"}, "content": {"type": "string"}},
                    "required": ["path", "content"], "additionalProperties": false}}),
            json!({"name": "read", "description": "Read a text file from your work dir.",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}},
                    "required": ["path"], "additionalProperties": false}}),
            json!({"name": "exec", "description": "Run a shell command in your work dir (no network).",
                "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}},
                    "required": ["cmd"], "additionalProperties": false}}),
            json!({"name": "wait", "description": "Wait for every spawned child; returns their reports and merges their verified files.",
                "input_schema": {"type": "object", "properties": {}, "additionalProperties": false}}),
            json!({"name": "report", "description": "Finish: summarise what you did.",
                "input_schema": {"type": "object", "properties": {"summary": {"type": "string"}},
                    "required": ["summary"], "additionalProperties": false}}),
        ];
        if self.can_spawn(view) {
            t.insert(0, json!({"name": "spawn", "description": "Delegate a sub-task to a child agent.",
                "input_schema": {"type": "object", "properties": {
                    "statement": {"type": "string", "description": "What the child must do, self-contained."},
                    "verify": {"type": "string", "description": "Shell command run in the child's dir after it reports; exit 0 = verified."},
                    "share_percent": {"type": "integer", "minimum": 5, "maximum": 60,
                        "description": "Share of your remaining budget lent to the child (default 25)."}},
                    "required": ["statement", "verify"], "additionalProperties": false}}));
        }
        Value::Array(t)
    }

    fn messages(&self, view: &NodeView, obs: &[Observation]) -> Result<Vec<Value>, String> {
        let hist = self.history.lock().expect("history lock");
        let turns = hist.get(&view.id).map(Vec::as_slice).unwrap_or(&[]);
        if turns.len() != obs.len() {
            return Err(format!(
                "history out of step: {} turns, {} observations",
                turns.len(),
                obs.len()
            ));
        }
        let mut m =
            vec![json!({"role": "user", "content": format!("Your task:\n{}", view.statement)})];
        for (t, o) in turns.iter().zip(obs) {
            let is_error = matches!(
                o,
                Observation::Denied { .. } | Observation::SpawnRejected { .. }
            );
            let text = bound(
                &serde_json::to_string(o).unwrap_or_default(),
                MAX_OBSERVATION_CHARS,
            );
            m.push(json!({"role": "assistant", "content": t.content}));
            m.push(json!({"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": t.tool_use_id,
                "content": self.cfg.scrub(&text), "is_error": is_error}]}));
        }
        Ok(m)
    }

    fn post(&self, path: &str, body: &Value) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{}", self.cfg.base_url, path))
            .header("x-api-key", &self.cfg.api_key.0)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .body(body.to_string())
    }

    /// Exact input count from the free endpoint; on failure, an upper bound:
    /// one token per body byte plus a margin for the server-side tool prompt.
    async fn input_upper_bound(&self, body: &Value) -> u64 {
        let mut b = body.clone();
        if let Some(o) = b.as_object_mut() {
            o.remove("max_tokens");
        }
        let counted = async {
            let r = self
                .post("/v1/messages/count_tokens", &b)
                .send()
                .await
                .ok()?;
            if !r.status().is_success() {
                return None;
            }
            r.json::<Value>().await.ok()?["input_tokens"].as_u64()
        }
        .await;
        match counted {
            Some(n) => n + n / 10 + 64,
            None => body.to_string().len() as u64 + 1500,
        }
    }

    fn api_error(&self, status: u16, text: &str) -> String {
        let (kind, msg) = serde_json::from_str::<Value>(text)
            .ok()
            .map(|v| {
                (
                    v["error"]["type"].as_str().unwrap_or("").to_string(),
                    v["error"]["message"].as_str().unwrap_or("").to_string(),
                )
            })
            .unwrap_or_default();
        let msg = if kind.is_empty() && msg.is_empty() {
            bound(text, 200)
        } else {
            format!("{kind}: {}", bound(&msg, 200))
        };
        self.cfg.scrub(&format!("anthropic HTTP {status}: {msg}"))
    }

    async fn send_with_retries(&self, body: &Value) -> Result<Value, String> {
        let mut last = String::new();
        for attempt in 0..=self.cfg.retries {
            if attempt > 0 {
                tokio::time::sleep(self.cfg.retry_base * 2u32.pow(attempt - 1)).await;
            }
            let resp = match self.post("/v1/messages", body).send().await {
                Ok(r) => r,
                Err(e) => {
                    last = self.cfg.scrub(&format!("anthropic transport: {e}"));
                    continue;
                }
            };
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            if (200..300).contains(&status) {
                return serde_json::from_str(&text)
                    .map_err(|e| format!("anthropic: bad JSON reply: {e}"));
            }
            last = self.api_error(status, &text);
            if !matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529) {
                break;
            }
        }
        Err(last)
    }

    fn to_decision(&self, view: &NodeView, name: &str, input: &Value) -> Result<Decision, String> {
        let s = |k: &str| -> Result<String, String> {
            input[k]
                .as_str()
                .map(|v| self.cfg.scrub(v))
                .ok_or_else(|| format!("tool {name}: missing string field {k}"))
        };
        Ok(match name {
            "write" => Decision::Write {
                path: s("path")?,
                content: s("content")?,
            },
            "read" => Decision::Read { path: s("path")? },
            "exec" => Decision::Exec { cmd: s("cmd")? },
            "wait" => Decision::Wait,
            "report" => Decision::Report(Report {
                summary: s("summary")?,
                claims_verified: false,
                as_node: None,
            }),
            "spawn" => {
                let share = input["share_percent"].as_u64().unwrap_or(25).clamp(5, 60);
                let r = view.remaining;
                let part = |x: u64| x * share / 100;
                let mut caps: Vec<Capability> = view.caps.iter().collect();
                if self
                    .cfg
                    .max_depth
                    .map(|m| view.depth + 1 >= m)
                    .unwrap_or(false)
                {
                    caps.retain(|c| *c != Capability::Spawn);
                }
                Decision::Spawn(TaskSpec {
                    statement: bound(&s("statement")?, 2000),
                    caps: CapSet::of(&caps),
                    budget: Budget {
                        tokens: part(r.tokens),
                        micro_usd: part(r.micro_usd),
                        ms: part(r.ms),
                        actions: part(r.actions).max(1),
                    },
                    verify: input["verify"].as_str().map(|v| self.cfg.scrub(v)),
                    model: ModelTier::Cheap,
                })
            }
            other => return Err(format!("unknown tool {other}")),
        })
    }
}

#[async_trait::async_trait]
impl LlmBackend for AnthropicLlm {
    async fn next(
        &self,
        view: &NodeView,
        obs: &[Observation],
    ) -> Result<(Decision, Usage), LlmError> {
        let messages = self.messages(view, obs)?;
        let mut body = json!({
            "model": self.cfg.model,
            "max_tokens": self.cfg.max_tokens,
            "system": self.system(view),
            "tools": self.tools(view),
            "tool_choice": {"type": "any", "disable_parallel_tool_use": true},
            "messages": messages,
        });

        // Worst case must fit in what this node has left, before any spend.
        let p = self.cfg.price;
        let r = view.remaining;
        let input = self.input_upper_bound(&body).await;
        let input_cost = p.micro_usd(input, 0, 0, 0);
        let afford_usd = r.micro_usd.saturating_sub(input_cost) * 1000 / p.output.max(1);
        let afford_tok = r.tokens.saturating_sub(input);
        let max_tokens = self.cfg.max_tokens.min(afford_usd).min(afford_tok);
        if input_cost > r.micro_usd || max_tokens < MIN_OUTPUT_TOKENS {
            self.bump(|t| t.refused_for_budget += 1);
            return Err(LlmError::from(format!(
                "budget: worst case of the next call ({input} input tokens + {MIN_OUTPUT_TOKENS} output) exceeds what is left ({} micro-USD, {} tokens)",
                r.micro_usd, r.tokens
            )));
        }
        body["max_tokens"] = json!(max_tokens);

        let reply = match self.send_with_retries(&body).await {
            Ok(v) => v,
            Err(e) => {
                self.bump(|t| t.errors += 1);
                return Err(LlmError::from(e));
            }
        };
        let u = &reply["usage"];
        let n = |k: &str| u[k].as_u64().unwrap_or(0);
        let (i, o, cw, cr) = (
            n("input_tokens"),
            n("output_tokens"),
            n("cache_creation_input_tokens"),
            n("cache_read_input_tokens"),
        );
        let usage = Usage {
            tokens: i + o + cw + cr,
            micro_usd: p.micro_usd(i, o, cw, cr),
        };
        self.bump(|t| {
            t.calls += 1;
            t.input_tokens += i;
            t.output_tokens += o;
            t.cache_write_tokens += cw;
            t.cache_read_tokens += cr;
            t.micro_usd += usage.micro_usd;
        });
        let fail = |msg: String| {
            self.bump(|t| t.errors += 1);
            LlmError {
                message: self.cfg.scrub(&msg),
                usage,
            }
        };
        if reply["stop_reason"] == "max_tokens" {
            return Err(fail("reply cut at max_tokens".into()));
        }
        let content: Value = serde_json::from_str(&self.cfg.scrub(&reply["content"].to_string()))
            .unwrap_or(Value::Null);
        let tu = content
            .as_array()
            .and_then(|a| a.iter().find(|b| b["type"] == "tool_use"))
            .cloned()
            .ok_or_else(|| {
                fail(format!(
                    "no tool call in reply (stop_reason {})",
                    reply["stop_reason"]
                ))
            })?;
        let name = tu["name"].as_str().unwrap_or("");
        let decision = self.to_decision(view, name, &tu["input"]).map_err(fail)?;
        let tool_use_id = tu["id"].as_str().unwrap_or("").to_string();
        self.history
            .lock()
            .expect("history lock")
            .entry(view.id.clone())
            .or_default()
            .push(Turn {
                content,
                tool_use_id,
            });
        Ok((decision, usage))
    }

    fn name(&self) -> &'static str {
        "anthropic"
    }
}
