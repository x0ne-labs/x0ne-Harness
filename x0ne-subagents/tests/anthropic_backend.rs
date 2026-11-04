//! AnthropicLlm against a local fake HTTP server. No network, no real key.
//! Proves: request shape, usage read and priced, usage debited from the tree,
//! a call refused before the network when the node cannot afford it, 4xx/5xx
//! surfaced cleanly, HTTPS_PROXY honoured, and the key in no journal,
//! report, error or Debug output.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::*;
use serde_json::{json, Value};
use x0ne_subagents::anthropic::{AnthropicConfig, AnthropicLlm};
use x0ne_subagents::budget::Budget;
use x0ne_subagents::caps::{CapSet, Capability};
use x0ne_subagents::llm::{Decision, LlmBackend, Observation, Usage};
use x0ne_subagents::mock_http::{MockServer, Recorded};
use x0ne_subagents::node::{ModelTier, NodeId, NodeState, NodeView};
use x0ne_subagents::tree::TreeConfig;

const KEY: &str = "sk-ant-test-KEY-0123456789abcdef";

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k: &str| m.get(k).cloned()
}

fn cfg_for(url: &str) -> AnthropicConfig {
    let mut c = AnthropicConfig::from_vars(vars(&[
        ("ANTHROPIC_API_KEY", KEY),
        ("ANTHROPIC_BASE_URL", url),
    ]))
    .expect("config");
    c.retry_base = std::time::Duration::from_millis(1);
    c
}

fn view(statement: &str, micro_usd: u64) -> NodeView {
    NodeView {
        id: NodeId::root(),
        depth: 0,
        statement: statement.into(),
        caps: CapSet::of(&[
            Capability::Read,
            Capability::Write,
            Capability::Exec,
            Capability::Spawn,
        ]),
        remaining: Budget {
            tokens: 1_000_000,
            micro_usd,
            ms: 60_000,
            actions: 50,
        },
        model: ModelTier::Cheap,
    }
}

fn tool_use(id: &str, name: &str, input: Value, input_tokens: u64, output_tokens: u64) -> String {
    json!({
        "id": "msg_test", "type": "message", "role": "assistant",
        "model": "claude-haiku-4-5-20251001",
        "content": [
            {"type": "text", "text": "ok"},
            {"type": "tool_use", "id": id, "name": name, "input": input}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens,
                  "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
    })
    .to_string()
}

fn count(n: u64) -> (u16, String) {
    (200, json!({"input_tokens": n}).to_string())
}

fn messages_calls(reqs: &[Recorded]) -> Vec<Recorded> {
    reqs.iter()
        .filter(|r| r.target.ends_with("/v1/messages"))
        .cloned()
        .collect()
}

#[tokio::test]
async fn request_is_messages_api_with_one_forced_tool_and_maps_to_a_decision() {
    let srv = MockServer::start(|r: &Recorded| {
        if r.target.ends_with("/count_tokens") {
            return count(900);
        }
        (
            200,
            tool_use("tu_1", "report", json!({"summary": "all done"}), 900, 40),
        )
    })
    .await
    .unwrap();
    let llm = AnthropicLlm::new(cfg_for(&srv.url)).unwrap();
    let (d, _) = llm.next(&view("write a tool", 200_000), &[]).await.unwrap();
    match d {
        Decision::Report(r) => {
            assert_eq!(r.summary, "all done");
            assert!(!r.claims_verified);
            assert!(r.as_node.is_none());
        }
        other => panic!("expected report, got {other:?}"),
    }
    let calls = messages_calls(&srv.requests());
    assert_eq!(calls.len(), 1);
    let r = &calls[0];
    assert_eq!(r.method, "POST");
    assert_eq!(r.header("x-api-key").as_deref(), Some(KEY));
    assert_eq!(r.header("anthropic-version").as_deref(), Some("2023-06-01"));
    assert!(r
        .header("content-type")
        .unwrap()
        .starts_with("application/json"));
    let b: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(b["model"], "claude-haiku-4-5-20251001");
    assert_eq!(b["tool_choice"]["type"], "any");
    assert_eq!(b["tool_choice"]["disable_parallel_tool_use"], true);
    let names: Vec<&str> = b["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for n in ["spawn", "exec", "write", "read", "wait", "report"] {
        assert!(names.contains(&n), "tool {n} missing: {names:?}");
    }
    assert!(b["system"].as_str().unwrap().contains("root"));
    assert_eq!(b["messages"][0]["role"], "user");
    assert!(b["messages"][0]["content"]
        .to_string()
        .contains("write a tool"));
    assert!(b["max_tokens"].as_u64().unwrap() >= 256);
}

#[tokio::test]
async fn history_replays_each_tool_use_with_its_observation() {
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = Arc::clone(&n);
    let srv = MockServer::start(move |r: &Recorded| {
        if r.target.ends_with("/count_tokens") {
            return count(500);
        }
        match n2.fetch_add(1, Ordering::SeqCst) {
            0 => (
                200,
                tool_use(
                    "tu_a",
                    "write",
                    json!({"path": "a.py", "content": "x=1\n"}),
                    500,
                    30,
                ),
            ),
            _ => (200, tool_use("tu_b", "wait", json!({}), 600, 10)),
        }
    })
    .await
    .unwrap();
    let llm = AnthropicLlm::new(cfg_for(&srv.url)).unwrap();
    let v = view("t", 200_000);
    let (d1, _) = llm.next(&v, &[]).await.unwrap();
    assert_eq!(
        d1,
        Decision::Write {
            path: "a.py".into(),
            content: "x=1\n".into()
        }
    );
    let obs = vec![Observation::Wrote {
        path: "a.py".into(),
    }];
    let (d2, _) = llm.next(&v, &obs).await.unwrap();
    assert_eq!(d2, Decision::Wait);
    let calls = messages_calls(&srv.requests());
    let b: Value = serde_json::from_slice(&calls[1].body).unwrap();
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1]["role"], "assistant");
    assert!(msgs[1]["content"].to_string().contains("tu_a"));
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
    assert_eq!(msgs[2]["content"][0]["tool_use_id"], "tu_a");
    assert!(msgs[2]["content"][0]["content"]
        .to_string()
        .contains("a.py"));
}

#[tokio::test]
async fn usage_is_read_and_priced_for_haiku() {
    let srv = MockServer::start(|r: &Recorded| {
        if r.target.ends_with("/count_tokens") {
            return count(1000);
        }
        let mut v: Value =
            serde_json::from_str(&tool_use("t", "wait", json!({}), 1000, 200)).unwrap();
        v["usage"]["cache_creation_input_tokens"] = json!(400);
        v["usage"]["cache_read_input_tokens"] = json!(1000);
        (200, v.to_string())
    })
    .await
    .unwrap();
    let llm = AnthropicLlm::new(cfg_for(&srv.url)).unwrap();
    let (_, u) = llm.next(&view("t", 200_000), &[]).await.unwrap();
    // Haiku 4.5: 1 $/MTok in, 5 out, 1.25 cache write, 0.10 cache read.
    // 1000*1 + 200*5 + 400*1.25 + 1000*0.1 = 2600 micro-USD.
    assert_eq!(
        u,
        Usage {
            tokens: 2600,
            micro_usd: 2600
        }
    );
    let t = llm.totals();
    assert_eq!(t.calls, 1);
    assert_eq!(t.input_tokens, 1000);
    assert_eq!(t.output_tokens, 200);
    assert_eq!(t.micro_usd, 2600);
}

#[tokio::test]
async fn a_call_the_node_cannot_afford_never_reaches_the_network() {
    let srv = MockServer::start(|r: &Recorded| {
        if r.target.ends_with("/count_tokens") {
            return count(3000);
        }
        (200, tool_use("t", "wait", json!({}), 3000, 10))
    })
    .await
    .unwrap();
    let llm = AnthropicLlm::new(cfg_for(&srv.url)).unwrap();
    // 3000 input tokens alone cost 3000 micro-USD: 2500 cannot pay for it.
    let err = llm.next(&view("t", 2500), &[]).await.unwrap_err();
    assert!(err.message.contains("budget"), "{}", err.message);
    assert_eq!(err.usage, Usage::default());
    assert!(messages_calls(&srv.requests()).is_empty());
    assert_eq!(llm.totals().refused_for_budget, 1);
}

#[tokio::test]
async fn max_tokens_is_clamped_to_what_the_node_can_afford() {
    let srv = MockServer::start(|r: &Recorded| {
        if r.target.ends_with("/count_tokens") {
            return count(1000);
        }
        (200, tool_use("t", "wait", json!({}), 1000, 10))
    })
    .await
    .unwrap();
    let llm = AnthropicLlm::new(cfg_for(&srv.url)).unwrap();
    llm.next(&view("t", 6000), &[]).await.unwrap();
    let b: Value = serde_json::from_slice(&messages_calls(&srv.requests())[0].body).unwrap();
    let mt = b["max_tokens"].as_u64().unwrap();
    // Worst case = reserved input + max_tokens * 5 must fit in 6000 micro-USD.
    assert!(mt >= 256 && mt * 5 + 1000 <= 6000, "max_tokens {mt}");
}

#[tokio::test]
async fn count_tokens_failure_falls_back_to_a_byte_bound() {
    let srv = MockServer::start(|r: &Recorded| {
        if r.target.ends_with("/count_tokens") {
            return (
                404,
                json!({"type":"error","error":{"type":"not_found_error","message":"no"}})
                    .to_string(),
            );
        }
        (200, tool_use("t", "wait", json!({}), 800, 10))
    })
    .await
    .unwrap();
    let llm = AnthropicLlm::new(cfg_for(&srv.url)).unwrap();
    let (d, u) = llm.next(&view("t", 200_000), &[]).await.unwrap();
    assert_eq!(d, Decision::Wait);
    assert_eq!(u.micro_usd, 800 + 50);
}

#[tokio::test]
async fn http_4xx_is_a_clean_error_and_is_not_retried() {
    let srv = MockServer::start(|r: &Recorded| {
        if r.target.ends_with("/count_tokens") {
            return count(100);
        }
        (
            400,
            json!({"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: bad"}})
                .to_string(),
        )
    })
    .await
    .unwrap();
    let llm = AnthropicLlm::new(cfg_for(&srv.url)).unwrap();
    let err = llm.next(&view("t", 200_000), &[]).await.unwrap_err();
    assert!(err.message.contains("400"), "{}", err.message);
    assert!(
        err.message.contains("invalid_request_error"),
        "{}",
        err.message
    );
    assert_eq!(err.usage, Usage::default());
    assert_eq!(messages_calls(&srv.requests()).len(), 1);
    assert_eq!(llm.totals().errors, 1);
}

#[tokio::test]
async fn http_5xx_is_retried_then_surfaced() {
    let srv = MockServer::start(|r: &Recorded| {
        if r.target.ends_with("/count_tokens") {
            return count(100);
        }
        (
            529,
            json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}})
                .to_string(),
        )
    })
    .await
    .unwrap();
    let cfg = cfg_for(&srv.url);
    let retries = cfg.retries as usize;
    let llm = AnthropicLlm::new(cfg).unwrap();
    let err = llm.next(&view("t", 200_000), &[]).await.unwrap_err();
    assert!(err.message.contains("529"), "{}", err.message);
    assert!(err.message.contains("overloaded_error"), "{}", err.message);
    assert_eq!(messages_calls(&srv.requests()).len(), 1 + retries);
}

#[tokio::test]
async fn a_reply_without_a_tool_call_is_an_error_that_still_carries_its_usage() {
    let srv = MockServer::start(|r: &Recorded| {
        if r.target.ends_with("/count_tokens") {
            return count(100);
        }
        (
            200,
            json!({"id":"m","type":"message","role":"assistant","model":"x",
                   "content":[{"type":"text","text":"I refuse to use tools"}],
                   "stop_reason":"end_turn",
                   "usage":{"input_tokens":100,"output_tokens":20}})
            .to_string(),
        )
    })
    .await
    .unwrap();
    let llm = AnthropicLlm::new(cfg_for(&srv.url)).unwrap();
    let err = llm.next(&view("t", 200_000), &[]).await.unwrap_err();
    assert!(err.message.contains("tool"), "{}", err.message);
    assert_eq!(err.usage.micro_usd, 100 + 20 * 5);
}

#[tokio::test]
async fn https_proxy_is_honoured() {
    let srv = MockServer::start(|r: &Recorded| {
        if r.target.ends_with("/count_tokens") {
            return count(100);
        }
        (200, tool_use("t", "wait", json!({}), 100, 10))
    })
    .await
    .unwrap();
    let cfg = AnthropicConfig::from_vars(vars(&[
        ("ANTHROPIC_API_KEY", KEY),
        ("ANTHROPIC_BASE_URL", "http://api.anthropic.invalid"),
        ("HTTPS_PROXY", &srv.url),
    ]))
    .unwrap();
    assert_eq!(cfg.proxy.as_deref(), Some(srv.url.as_str()));
    let llm = AnthropicLlm::new(cfg).unwrap();
    llm.next(&view("t", 200_000), &[]).await.unwrap();
    let calls = messages_calls(&srv.requests());
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0]
            .target
            .starts_with("http://api.anthropic.invalid/v1/messages"),
        "proxy saw {}",
        calls[0].target
    );
}

#[test]
fn config_reads_only_env_and_defaults_to_haiku() {
    let e = AnthropicConfig::from_vars(vars(&[])).unwrap_err();
    assert!(e.contains("ANTHROPIC_API_KEY"));
    let c = AnthropicConfig::from_vars(vars(&[("ANTHROPIC_API_KEY", KEY)])).unwrap();
    assert_eq!(c.model, "claude-haiku-4-5-20251001");
    assert_eq!(c.base_url, "https://api.anthropic.com");
    assert!(c.proxy.is_none());
    let c = AnthropicConfig::from_vars(vars(&[
        ("ANTHROPIC_API_KEY", KEY),
        ("X0NE_LLM_MODEL", "claude-haiku-4-5"),
        ("https_proxy", "http://nuit-proxy:8888"),
    ]))
    .unwrap();
    assert_eq!(c.model, "claude-haiku-4-5");
    assert_eq!(c.proxy.as_deref(), Some("http://nuit-proxy:8888"));
    let e = AnthropicConfig::from_vars(vars(&[
        ("ANTHROPIC_API_KEY", KEY),
        ("X0NE_LLM_MODEL", "some-unknown-model"),
    ]))
    .unwrap_err();
    assert!(e.contains("X0NE_LLM_PRICE_IN"), "{e}");
    assert!(!e.contains(KEY));
    let dbg = format!("{c:?}");
    assert!(!dbg.contains(KEY), "{dbg}");
}

/// A whole tree on the fake server: the real usage is debited from the
/// parent's budget, conservation holds, and the key appears nowhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_debits_real_usage_and_never_writes_the_key() {
    // Adversarial server: echoes the key back in a 401 for the child.
    let srv = MockServer::start(|r: &Recorded| {
        if r.target.ends_with("/count_tokens") {
            return count(1000);
        }
        let b: Value = serde_json::from_slice(&r.body).unwrap();
        let sys = b["system"].as_str().unwrap_or("").to_string();
        let turns = b["messages"].as_array().unwrap().len() / 2;
        if sys.contains("node root/2 ") {
            return (
                401,
                json!({"type":"error","error":{"type":"authentication_error",
                       "message": format!("invalid x-api-key {KEY}")}})
                .to_string(),
            );
        }
        if sys.contains("node root/1 ") {
            return (
                200,
                tool_use(
                    "c0",
                    "report",
                    json!({"summary": format!("child done, key {KEY}")}),
                    1000,
                    50,
                ),
            );
        }
        let (name, input) = match turns {
            0 => (
                "spawn",
                json!({"statement": "child one", "verify": "true", "share_percent": 25}),
            ),
            1 => (
                "spawn",
                json!({"statement": "child two", "verify": "true", "share_percent": 25}),
            ),
            2 => ("wait", json!({})),
            _ => ("report", json!({"summary": "root done"})),
        };
        (200, tool_use(&format!("r{turns}"), name, input, 1000, 100))
    })
    .await
    .unwrap();
    let fx = Fixture::new();
    let llm = Arc::new(AnthropicLlm::new(cfg_for(&srv.url)).unwrap());
    let tree = fx.tree_with(llm.clone(), TreeConfig::default());
    let mut spec = root_spec("build it", 64);
    spec.budget.micro_usd = 300_000;
    spec.budget.tokens = 1_000_000;
    let out = tree.run_root(spec).await.unwrap();
    assert!(out.verified, "{:?}", out.state);

    let snap = tree.snapshot();
    let root = &snap[&id("root")];
    // root: 4 calls of 1000 in + 100 out = 1500 each; child 1: 1000 + 250.
    assert_eq!(root.ledger.spent.micro_usd, 4 * 1500);
    assert_eq!(snap[&id("root/1")].ledger.spent.micro_usd, 1250);
    assert_eq!(snap[&id("root/2")].ledger.spent.micro_usd, 0);
    assert!(matches!(snap[&id("root/2")].state, NodeState::Failed(_)));
    assert_eq!(tree.total_spent().micro_usd, llm.totals().micro_usd);
    let l = root.ledger;
    assert_eq!(l.lent, Budget::default());
    assert_eq!(
        l.initial.micro_usd,
        l.remaining.micro_usd + l.spent.micro_usd + 1250
    );

    // The key: not in any journal, report, error, Debug or totals.
    for e in std::fs::read_dir(&fx.journal_dir).unwrap() {
        let s = std::fs::read_to_string(e.unwrap().path()).unwrap();
        assert!(!s.contains(KEY), "key leaked into a journal");
    }
    for (_, n) in snap {
        assert!(
            !format!("{n:?}").contains(KEY),
            "key leaked into a snapshot"
        );
    }
    assert!(!format!("{llm:?}").contains(KEY));
    assert!(!serde_json::to_string(&llm.totals()).unwrap().contains(KEY));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_depth_rejects_a_spawn_beyond_it() {
    use x0ne_subagents::llm::ScriptedLlm;
    let fx = Fixture::new();
    let script = |view: &NodeView, obs: &[Observation]| -> Decision {
        match turn(obs) {
            0 => Decision::Spawn(child_spec(view, &format!("below {}", view.id), 4)),
            1 => Decision::Wait,
            _ => report("done"),
        }
    };
    let cfg = TreeConfig {
        max_depth: Some(2),
        ..TreeConfig::default()
    };
    let tree = fx.tree(ScriptedLlm::new(script), cfg);
    tree.run_root(root_spec("deep", 256)).await.unwrap();
    let snap = tree.snapshot();
    assert_eq!(snap.keys().map(|k| k.depth()).max(), Some(2));
    let ev = tree.journal().read(&id("root/1/1")).unwrap();
    assert!(ev.iter().any(|e| e.event.kind == "spawn_rejected"
        && e.event.data["reason"].as_str().unwrap().contains("depth")));
}
