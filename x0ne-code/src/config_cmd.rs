//! `x0ne config inspect` — the EFFECTIVE policy, as the runtime sees it.
//!
//! # Why this command exists
//!
//! This repository mass-produces one class of defect: a policy file
//! declares one thing and the runtime gets another, because a SECOND
//! setting decides before it. The command makes that gap
//! visible without launching an agent.
//!
//! SECURITY: everything it prints comes from the production path, never
//! from a local reconstruction — an « inspect » that recomputed the merge
//! could agree with itself and be wrong, which is exactly the error it
//! exists to detect. Concretely:
//!
//! * the file is found and read by [`AppConfig::find_and_load_with_source`],
//!   the runtime's own search — not by a convention copied here;
//! * the action switches come out of `cfg.actions`, that is AFTER the
//!   merge of `[actions]` with the defaults (`config.rs:675`), which is the
//!   map `PolicyEngine::new` receives in production;
//! * the roots are canonicalised by `confine::normalize_roots`, the
//!   function that the engine and the executor both call, and the
//!   entries it drops are named by `confine::unresolvable_roots`;
//! * the full exemption is read by `x0ne_runtime::policy::is_always_allowed`,
//!   the only public reader of `ALWAYS_ALLOW`, the very one `evaluate`
//!   calls;
//! * « does this runtime accept a grant? » is answered by a real
//!   [`PolicyEngine`] built like `x0ne-runtime/src/main.rs:158`.
//!
//! # What the command CANNOT say, and does not claim to know
//!
//! Session grants live in memory in the runtime process
//! (`policy.rs:235`: `SessionGrants`, emptied on exit, not inherited by a
//! `--resume`). An `x0ne config inspect` is another process: it has no
//! way to see them. It therefore returns `session_grants: null` and says why,
//! rather than an empty array that would read « no grant was given ».
//!
//! Nor does it give the final verdict for an action: the `readonly` /
//! `confirm` modes and the desktop gate decide BEFORE the per-action
//! switch (`policy.rs:524-544`), and they depend on a real action, not
//! on a name. What is printed per action is the switch stage, and the
//! gates above it are printed separately, in the order they apply.

use std::path::Path;

use anyhow::{Context, Result};
use colored::Colorize;
use serde_json::{json, Map, Value};

use x0ne_runtime::config::{AppConfig, ConfigSource};
use x0ne_runtime::executor::confine;
use x0ne_runtime::policy::PolicyEngine;

/// Fields whose name is enough to determine that they are not printed.
const SECRETISH: &[&str] = &["token", "secret", "key", "password"];

/// `<set>` / `<unset>` for a field whose NAME says it is a secret,
/// `Null` for the others — that is « there is nothing to redact here ».
///
/// SECURITY: the decision is made on the name, never on the value. A token
/// can be any string; what distinguishes it from a `host` is what its
/// field is called. Corollary: a future secret field named without
/// any of these words would be printed in clear. That is what the test
/// `a_secret_is_reported_as_set_never_printed` guards.
fn redact(name: &str, present: bool) -> Value {
    if SECRETISH
        .iter()
        .any(|s| name.to_ascii_lowercase().contains(s))
    {
        json!(if present { "<set>" } else { "<unset>" })
    } else {
        Value::Null
    }
}

/// A JSON table whose every field goes through [`redact`] before entering.
fn redacted_table(fields: Vec<(&str, Value)>) -> Value {
    let mut out = Map::new();
    for (name, clear) in fields {
        let present = !clear.is_null() && clear != json!("");
        let value = match redact(name, present) {
            Value::Null => clear,
            hidden => hidden,
        };
        out.insert(name.to_string(), value);
    }
    Value::Object(out)
}

/// The short name of the file's provenance.
///
/// SECURITY: exhaustive match, no `_` arm. Provenance is security
/// data — the 2026-08-15 audit came down entirely to the fact that
/// nobody could say which file a runtime obeyed — and a new
/// [`ConfigSource`] variant must break compilation here rather than
/// be printed under another one's name.
fn source_kind(source: &ConfigSource) -> &'static str {
    match source {
        ConfigSource::Explicit(_) => "explicit",
        ConfigSource::Cwd(_) => "working_directory",
        ConfigSource::Xdg(_) => "xdg",
        ConfigSource::LegacyXdg(_) => "xdg_legacy",
    }
}

/// The state of ONE action's switch, at the stage where it applies.
///
/// SECURITY: the arm order is that of `evaluate_with_config`. The exemption
/// comes first because it comes first there: the SendInput defect came from the
/// reverse, an `[actions] SendInput = false` readable in the file and without
/// effect. Here both are printed side by side — `switch` says what the
/// file declares, `resolved` what the engine makes of it.
fn resolved_state(dispensed: bool, switch: Option<bool>, default_effect: &str) -> &'static str {
    if dispensed {
        return "dispensed_before_the_switch";
    }
    match switch {
        Some(true) => "enabled",
        Some(false) => "disabled",
        // `evaluate` only tests equality with "deny": everything else lets
        // through (`policy.rs:556`). Do not invent a third case.
        None if default_effect == "deny" => "absent_default_deny",
        None => "absent_default_allow",
    }
}

/// The effective policy, as stable JSON.
///
/// Keys are sorted: `serde_json::Map` is a `BTreeMap` by default in
/// this repository, so two runs on the same configuration return the same
/// text, byte for byte. It is a script output.
///
/// `path` is the file actually loaded, or `None` for the compiled
/// defaults (fail-closed, `default_effect = "deny"`).
pub fn effective_policy_json(cfg: &AppConfig, path: Option<&str>) -> Value {
    x0ne_orchestrator::canonical_json::canonical(&build_policy_json(cfg, path))
}

fn build_policy_json(cfg: &AppConfig, path: Option<&str>) -> Value {
    // Built as in production (`x0ne-runtime/src/main.rs:158`): it is
    // the one queried about the grant gate, not a copy of the rule.
    let engine = PolicyEngine::new(cfg.clone(), cfg.actions.clone());

    let mut actions = Map::new();
    let mut enabled_by_file: Vec<String> = Vec::new();
    for name in x0ne_actions::all_type_names() {
        let dispensed = x0ne_runtime::policy::is_always_allowed(name);
        let switch = cfg.actions.get(*name).copied();
        if switch == Some(true) {
            enabled_by_file.push((*name).to_string());
        }
        actions.insert(
            (*name).to_string(),
            json!({
                "switch": switch,
                "resolved": resolved_state(dispensed, switch, &cfg.policy.default_effect),
                "dispensed": dispensed,
                // Side-effect class, generated at compile time from the
                // exhaustive match of `side_effect_class` — not a list kept
                // by hand here.
                "class": x0ne_actions::action_meta(name).map(|m| m.class),
            }),
        );
    }

    let roots = |configured: &Vec<String>| {
        json!({
            "configured": configured,
            "canonical": confine::normalize_roots(configured)
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
            // What `normalize_roots` silently drops. Field report
            // of 2026-08-16: `allowed_read_paths = ["~/code"]` on
            // a machine without `~/code` refused every read, and the operator
            // was looking for an empty list he did not have.
            "dropped": confine::unresolvable_roots(configured),
        })
    };

    let deny_patterns: Vec<Value> = cfg
        .policy
        .deny_path_patterns
        .iter()
        .map(|p| {
            json!({
                "pattern": p,
                // `AppConfig::load` refuses a pattern that does not compile
                // (`apply_deny_floor`, 2026-11-04). A config built in code skips
                // that check, and `PolicyEngine::new` would drop the pattern.
                "compiles": glob::Pattern::new(p).is_ok(),
            })
        })
        .collect();

    let http = cfg.runtime.http.as_ref().map(|h| {
        redacted_table(vec![
            ("host", json!(h.host)),
            ("port", json!(h.port)),
            ("auth_token", json!(h.auth_token)),
            ("allowed_origins", json!(h.allowed_origins)),
            ("allowed_hosts", json!(h.allowed_hosts)),
        ])
    });

    json!({
        "loaded_from": path,
        "runtime": {
            "mode": cfg.runtime.mode,
            "http": http,
        },
        // Serialised from `PolicyConfig` itself: a key added to the schema
        // appears here without anyone thinking of it, and no existing key
        // can be missing because someone forgot to copy it.
        "policy": serde_json::to_value(&cfg.policy).unwrap_or(Value::Null),
        "gates": {
            "order": [
                "always_allow",
                "desktop_enabled",
                "policy_mode",
                "action_switch",
                "policy_default_effect",
            ],
            "desktop_enabled": cfg.desktop.enabled,
            "policy_mode": cfg.policy.mode,
        },
        "actions": Value::Object(actions),
        "roots": {
            "read": roots(&cfg.policy.allowed_read_paths),
            "write": roots(&cfg.policy.allowed_write_paths),
        },
        "deny_patterns": deny_patterns,
        // What the FILE grants. A grant persisted by
        // `policy_file::persist_grant` is, by construction, one more line
        // in these same lists: it is not marked, so nothing here
        // can say which one came from a prompt and which was written by hand.
        "file_grants": {
            "commands": cfg.policy.allowed_commands,
            "read_paths": cfg.policy.allowed_read_paths,
            "write_paths": cfg.policy.allowed_write_paths,
            "actions_enabled": enabled_by_file,
        },
        // SECURITY: `null`, not `[]`. See the module notes: this process
        // has no way to see the runtime's memory, and « I don't know »
        // is not « there are none ».
        "session_grants": Value::Null,
        "session_grants_source": "not_a_live_session",
        "accepts_session_grants": engine.accepts_grants(),
        "warnings": cfg.policy_warnings(),
    })
}

/// The grants that a session journal RECORDS.
///
/// SECURITY: this is not the state of a runtime, and the `session_grants_source`
/// key says so (`journal:<id>`). A grant lives in memory in the process of the
/// runtime that received it; that one is dead, and a `--resume` inherits
/// none of them (`agent/session.rs:1075`). What this list answers is « what did I
/// approve during that session », not « what is in force ».
/// The journal is written best-effort (`let _ = journal.log_policy_grant(…)`), so
/// an empty list is not proof of absence either.
fn journal_grants(dir: &Path, id: &str) -> Result<Vec<Value>> {
    use x0ne_orchestrator::session_store as store;

    // SECURITY: the argument becomes a path. Validated, never repaired.
    let id = store::validate_session_id(id).map_err(|e| anyhow::anyhow!("{e}"))?;
    let path = store::journal_path(dir, id);
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "cannot read {} — `x0ne sessions` lists what there is",
            path.display()
        )
    })?;

    let mut out = Vec::new();
    for line in raw.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            // A line truncated by a crash does not invalidate the others —
            // same policy as `session_store::parse_journal`.
            continue;
        };
        if event.get("type").and_then(Value::as_str) != Some("policy_grant") {
            continue;
        }
        out.push(json!({
            "kind": event.get("kind").cloned().unwrap_or(Value::Null),
            "value": event.get("value").cloned().unwrap_or(Value::Null),
            "action_type": event.get("action_type").cloned().unwrap_or(Value::Null),
            "persist": event.get("persist").cloned().unwrap_or(Value::Null),
            "persisted": event.get("persisted").cloned().unwrap_or(Value::Null),
            "ts": event.get("ts").cloned().unwrap_or(Value::Null),
        }));
    }
    Ok(out)
}

/// `x0ne config inspect` — prints the effective policy.
pub fn inspect(json_out: bool, session: Option<&str>) -> Result<()> {
    // The runtime's search, not a copied convention: `--config`, the
    // inherited XDG and the current-directory opt-in change the answer, and
    // that answer is precisely what one came here for.
    let (cfg, source) = AppConfig::find_and_load_with_source(None)?;
    let path = source.path().map(|p| p.display().to_string());
    let mut value = effective_policy_json(&cfg, path.as_deref());
    value["config_source"] = json!(source_kind(&source));

    if let Some(id) = session {
        let dir = x0ne_orchestrator::session_store::sessions_dir();
        let grants = journal_grants(&dir, id)?;
        value["session_grants"] = Value::Array(grants);
        value["session_grants_source"] = json!(format!("journal:{id}"));
    }

    if json_out {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print!("{}", render_text(&value));
    }
    Ok(())
}

// ── Rendu humain ──────────────────────────────────────────────────────────────

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|x| match x.as_str() {
                    Some(s) => s.to_string(),
                    None => x.to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A list line, truncated and sanitised.
///
/// SECURITY: everything that comes from a journal has passed through a model. `x0ne
/// sessions` sanitises for the same reason — a grant value carrying
/// an ANSI sequence would repaint the operator's terminal at the precise moment
/// he is checking his policy.
fn safe(s: &str) -> String {
    x0ne_orchestrator::text_safety::snippet(s, 160)
}

fn render_roots(out: &mut String, label: &str, v: &Value) {
    let configured = strs(&v["configured"]);
    let canonical = strs(&v["canonical"]);
    let dropped = strs(&v["dropped"]);
    out.push_str(&format!(
        "  {:<16}{} configured, {} resolved\n",
        label,
        configured.len(),
        canonical.len()
    ));
    for c in &canonical {
        out.push_str(&format!("  {:<16}{}\n", "", safe(c)));
    }
    for d in &dropped {
        out.push_str(&format!(
            "  {:<16}{} {}\n",
            "",
            safe(d),
            "does not resolve — dropped, it protects and permits nothing".truecolor(220, 180, 0)
        ));
    }
}

fn render_text(v: &Value) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\n  {}\n\n",
        "effective policy — what the runtime would load on this host".bold()
    ));

    let loaded = match v["loaded_from"].as_str() {
        Some(p) => safe(p),
        None => "<compiled-in defaults, fail-closed: default_effect=deny>".to_string(),
    };
    out.push_str(&format!("  {:<16}{}\n", "loaded from", loaded));
    if let Some(kind) = v["config_source"].as_str() {
        out.push_str(&format!("  {:<16}{}\n", "provenance", kind));
    }
    out.push_str(&format!(
        "  {:<16}{}   (session grants accepted: {})\n",
        "runtime mode",
        v["runtime"]["mode"].as_str().unwrap_or("?"),
        if v["accepts_session_grants"] == json!(true) {
            "yes"
        } else {
            "no"
        }
    ));
    if let Some(http) = v["runtime"]["http"].as_object() {
        out.push_str(&format!(
            "  {:<16}{}:{}  auth_token {}\n",
            "http",
            http["host"].as_str().unwrap_or("?"),
            http["port"],
            http["auth_token"].as_str().unwrap_or("?"),
        ));
    }
    out.push_str(&format!(
        "  {:<16}{}\n",
        "policy mode",
        v["policy"]["mode"].as_str().unwrap_or("?")
    ));
    out.push_str(&format!(
        "  {:<16}{}\n",
        "default effect",
        v["policy"]["default_effect"].as_str().unwrap_or("?")
    ));
    out.push_str(&format!(
        "  {:<16}{}\n",
        "desktop",
        if v["gates"]["desktop_enabled"] == json!(true) {
            "enabled"
        } else {
            "disabled"
        }
    ));

    out.push('\n');
    for w in strs(&v["warnings"]) {
        out.push_str(&format!("  {} {}\n", "!".truecolor(220, 180, 0), safe(&w)));
    }

    out.push('\n');
    render_roots(&mut out, "read roots", &v["roots"]["read"]);
    render_roots(&mut out, "write roots", &v["roots"]["write"]);

    out.push('\n');
    let patterns = v["deny_patterns"].as_array().cloned().unwrap_or_default();
    out.push_str(&format!("  {:<16}{}\n", "deny patterns", patterns.len()));
    for p in &patterns {
        let text = p["pattern"].as_str().unwrap_or("?");
        let broken = p["compiles"] != json!(true);
        out.push_str(&format!(
            "  {:<16}{}{}\n",
            "",
            safe(text),
            if broken {
                "   does not compile — dropped, this path is not protected"
                    .truecolor(220, 60, 60)
                    .to_string()
            } else {
                String::new()
            }
        ));
    }

    // ── Actions ───────────────────────────────────────────────────────────
    out.push('\n');
    let empty = Map::new();
    let actions = v["actions"].as_object().unwrap_or(&empty);
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    let mut dispensed: Vec<&str> = Vec::new();
    let mut enabled: Vec<&str> = Vec::new();
    for (name, a) in actions {
        let state = a["resolved"].as_str().unwrap_or("?");
        *counts.entry(state).or_default() += 1;
        match state {
            "dispensed_before_the_switch" => dispensed.push(name),
            "enabled" => enabled.push(name),
            _ => {}
        }
    }
    out.push_str(&format!("  {:<16}{} known\n", "actions", actions.len()));
    for (state, n) in &counts {
        out.push_str(&format!("  {:<16}{:>4}  {}\n", "", n, state));
    }
    if !dispensed.is_empty() {
        out.push_str(&format!(
            "  {:<16}{}\n",
            "",
            format!(
                "dispensed from the whole gate, whatever [actions] says: {}",
                dispensed.join(", ")
            )
            .truecolor(220, 180, 0)
        ));
    }
    if !enabled.is_empty() {
        out.push_str(&format!("  {:<16}{}\n", "enabled", enabled.join(", ")));
    }

    // ── Grants ────────────────────────────────────────────────────────────
    out.push('\n');
    let commands = strs(&v["file_grants"]["commands"]);
    out.push_str(&format!(
        "  {:<16}{}\n",
        "commands",
        if commands.is_empty() {
            "<empty — no command will run>".to_string()
        } else {
            commands
                .iter()
                .map(|c| safe(c))
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));

    match v["session_grants"].as_array() {
        None => out.push_str(&format!(
            "  {:<16}{}\n",
            "session grants",
            "unknown — they live in the running runtime's memory, not in any file"
        )),
        Some(grants) if grants.is_empty() => out.push_str(&format!(
            "  {:<16}{}\n",
            "session grants",
            format!(
                "{} recorded nothing (best-effort journal: absence is not proof)",
                v["session_grants_source"].as_str().unwrap_or("?")
            )
        )),
        Some(grants) => {
            out.push_str(&format!(
                "  {:<16}{} recorded by {} — granted then, in force nowhere now\n",
                "session grants",
                grants.len(),
                v["session_grants_source"].as_str().unwrap_or("?")
            ));
            for g in grants {
                let line = format!(
                    "  {:<16}{} {} {}",
                    "",
                    safe(g["kind"].as_str().unwrap_or("?")),
                    safe(g["value"].as_str().unwrap_or("?")),
                    if g["persist"] == json!(true) {
                        "(persisted to the policy file)"
                    } else {
                        ""
                    }
                );
                out.push_str(line.trim_end());
                out.push('\n');
            }
        }
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The minimal v0.3 skeleton, FIXED size. No value is derived
    /// from a constant of the code under test.
    const MINIMAL: &str = r#"
[policy]
default_effect = "deny"
max_read_bytes = 1024
max_list_entries = 10
allowed_commands = []
allowed_read_paths = []
allowed_write_paths = []
deny_path_patterns = []
"#;

    fn load(toml: &str) -> (AppConfig, tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x0ne-runtime.toml");
        std::fs::write(&path, toml).unwrap();
        // SECURITY: `AppConfig::load`, never `toml::from_str`. A parse tests
        // serde; it misses the `[actions]` merge and the refusal of an unknown
        // key, that is everything the command exists to show.
        let cfg = AppConfig::load(&path).unwrap();
        let shown = path.display().to_string();
        (cfg, dir, shown)
    }

    /// Without a live session, grants are `null` — never an empty array.
    ///
    /// SECURITY: an empty array reads « no grant was given ». That is
    /// a claim this command is not in a position to make: session
    /// grants live in memory in the runtime process
    /// (`policy.rs:235`). Saying « I don't know » and saying « there are none »
    /// are not the same sentence.
    #[test]
    fn without_a_live_session_the_grants_are_null_and_say_why() {
        let cfg = AppConfig::defaults();
        let v = effective_policy_json(&cfg, None);
        assert!(v["session_grants"].is_null());
        assert_eq!(v["session_grants_source"], "not_a_live_session");
    }

    /// The HTTP authentication token is never printed.
    ///
    /// A failure would mean that a diagnostic command — the one people copy
    /// into a bug report — publishes the secret that protects the runtime API.
    #[test]
    fn a_secret_is_reported_as_set_never_printed() {
        let toml = format!(
            "\n[runtime]\nmode = \"http\"\n\n[runtime.http]\nauth_token = \
             \"s3cr3t-do-not-print\"\n{MINIMAL}"
        );
        let (cfg, _dir, path) = load(&toml);

        let printed = effective_policy_json(&cfg, Some(&path)).to_string();
        assert!(
            !printed.contains("s3cr3t-do-not-print"),
            "the token leaked: {printed}"
        );
        assert!(
            printed.contains("\"<set>\""),
            "the token must be reported as present"
        );
    }

    /// A missing token reads `<unset>`, not `<set>` nor nothing.
    ///
    /// A failure would mean that an open HTTP API reads as a protected
    /// API — redaction must not disguise absence as presence.
    #[test]
    fn a_missing_secret_is_reported_as_unset() {
        let toml =
            format!("\n[runtime]\nmode = \"http\"\n\n[runtime.http]\nport = 7878\n{MINIMAL}");
        let (cfg, _dir, path) = load(&toml);
        let v = effective_policy_json(&cfg, Some(&path));
        assert_eq!(v["runtime"]["http"]["auth_token"], "<unset>");
    }

    /// The 92 actions are printed with their RESOLVED state.
    ///
    /// A failure would mean that the tool meant to make visible « what the
    /// file declares is not what it gets » only prints what the
    /// file declares — and so serves no purpose.
    #[test]
    fn every_action_appears_with_its_resolved_state() {
        let cfg = AppConfig::defaults();
        let v = effective_policy_json(&cfg, None);
        let actions = v["actions"].as_object().expect("an actions object");
        for name in x0ne_actions::all_type_names() {
            assert!(
                actions.contains_key(*name),
                "{name} missing from the output"
            );
            assert!(
                actions[*name]["resolved"].is_string(),
                "{name} has no resolved state"
            );
        }
    }

    /// An action that the file switches off but the exemption lets through is
    /// printed as such: `switch = false`, `resolved = dispensed`.
    ///
    /// A failure would mean that the tool reprints the file instead of the
    /// decision — that is, it would have shown `SendInput = false` as
    /// « disabled » during the months when `ALWAYS_ALLOW` let it through.
    /// The action name comes from `is_always_allowed`, not from a copy
    /// of the list: if the exemption changes, this test follows without lying.
    #[test]
    fn an_action_dispensed_by_the_engine_says_so_even_when_the_file_disables_it() {
        let dispensed = x0ne_actions::all_type_names()
            .iter()
            .find(|n| x0ne_runtime::policy::is_always_allowed(n))
            .expect("the engine exempts at least one action");
        let toml = format!("{MINIMAL}\n[actions]\n{dispensed} = false\n");
        let (cfg, _dir, path) = load(&toml);

        let v = effective_policy_json(&cfg, Some(&path));
        assert_eq!(v["actions"][*dispensed]["switch"], json!(false));
        assert_eq!(
            v["actions"][*dispensed]["resolved"],
            "dispensed_before_the_switch"
        );
    }

    /// A root that does not resolve is NAMED, not merely absent.
    ///
    /// A failure would mean that the operator rereads a written, readable list
    /// while the runtime keeps nothing of it — the field report of
    /// 2026-08-16, where a nonexistent `~/code` refused every read.
    #[test]
    fn a_root_that_does_not_resolve_is_named_as_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let ghost = dir.path().join("ghost-never-created");
        let toml = format!(
            "\n[policy]\ndefault_effect = \"deny\"\nmax_read_bytes = 1024\n\
             max_list_entries = 10\nallowed_commands = []\n\
             allowed_read_paths = [\"{}\", \"{}\"]\nallowed_write_paths = []\n\
             deny_path_patterns = []\n",
            real.display(),
            ghost.display()
        );
        let (cfg, _d, path) = load(&toml);

        let v = effective_policy_json(&cfg, Some(&path));
        let read = &v["roots"]["read"];
        assert_eq!(read["configured"].as_array().unwrap().len(), 2);
        assert_eq!(read["canonical"].as_array().unwrap().len(), 1);
        assert_eq!(
            read["dropped"].as_array().unwrap().len(),
            1,
            "the phantom root must be named: {read}"
        );
    }

    /// A deny pattern that does not compile is named: by the load, which
    /// refuses the file, and by the report, for a config built in code.
    ///
    /// A failure would mean that a badly written `deny_path_patterns` — dropped by
    /// the `filter_map(…ok())` of `PolicyEngine::new` — still reads
    /// as a protection in place.
    #[test]
    fn a_deny_pattern_that_does_not_compile_is_named() {
        let toml = "\n[policy]\ndefault_effect = \"deny\"\nmax_read_bytes = 1024\n\
                    max_list_entries = 10\nallowed_commands = []\n\
                    allowed_read_paths = []\nallowed_write_paths = []\n\
                    deny_path_patterns = [\"**/.ssh/**\", \"[unclosed\"]\n";
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x0ne-runtime.toml");
        std::fs::write(&file, toml).unwrap();
        let refused = format!("{:#}", AppConfig::load(&file).unwrap_err());
        assert!(
            refused.contains("\"[unclosed\""),
            "the load refusal must name the pattern: {refused}"
        );

        let mut cfg = AppConfig::defaults();
        cfg.policy.deny_path_patterns = vec!["**/.ssh/**".into(), "[unclosed".into()];
        let v = effective_policy_json(&cfg, None);
        let pats = v["deny_patterns"].as_array().unwrap();
        assert_eq!(pats[0]["compiles"], json!(true));
        assert_eq!(
            pats[1]["compiles"],
            json!(false),
            "an invalid pattern must report itself invalid: {v}"
        );
    }

    /// The JSON output is stable: same bytes for the same configuration.
    ///
    /// A failure would mean that a script comparing it from one run to
    /// the next sees a difference where the policy has not moved.
    #[test]
    fn the_json_output_is_byte_stable() {
        let cfg = AppConfig::defaults();
        let a = serde_json::to_string_pretty(&effective_policy_json(&cfg, None)).unwrap();
        let b = serde_json::to_string_pretty(&effective_policy_json(&cfg, None)).unwrap();
        assert_eq!(a, b);
        let value = effective_policy_json(&cfg, None);
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted, "top-level keys must be sorted");
    }

    /// Every configuration provenance has a name, and the match is exhaustive.
    ///
    /// A COMPILATION failure is the point: a new `ConfigSource` variant
    /// must force a decision, not be printed under another one's
    /// name.
    #[test]
    fn every_config_source_has_its_own_name() {
        use std::path::PathBuf;
        let names = [
            source_kind(&ConfigSource::Explicit(PathBuf::from("/a"))),
            source_kind(&ConfigSource::Cwd(PathBuf::from("/a"))),
            source_kind(&ConfigSource::Xdg(PathBuf::from("/a"))),
            source_kind(&ConfigSource::LegacyXdg(PathBuf::from("/a"))),
        ];
        let unique: std::collections::BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(unique.len(), names.len(), "two sources carry the same name");
    }

    /// Grants read back from a journal are that journal's, and the label
    /// says where they come from.
    ///
    /// SECURITY: a failure that made them appear under
    /// `not_a_live_session` — or the reverse — would present a historical
    /// record as the state of a running runtime.
    #[test]
    fn the_grants_read_from_a_journal_are_labelled_as_coming_from_it() {
        let dir = tempfile::tempdir().unwrap();
        let id = "2026-08-16_10-00-00";
        std::fs::write(
            dir.path().join(format!("{id}.jsonl")),
            "{\"type\":\"user\",\"text\":\"hi\"}\n\
             {\"type\":\"policy_grant\",\"kind\":\"command\",\"value\":\"rg\",\
             \"persist\":false,\"persisted\":null,\"action_type\":\"RunCommand\"}\n\
             not json at all\n",
        )
        .unwrap();

        let grants = journal_grants(dir.path(), id).unwrap();
        assert_eq!(grants.len(), 1, "only one line is a grant: {grants:?}");
        assert_eq!(grants[0]["kind"], "command");
        assert_eq!(grants[0]["value"], "rg");
    }

    /// A session identifier that is not an identifier is refused
    /// before it becomes a path.
    ///
    /// SECURITY: a failure would make `--session ../../etc/passwd` an arbitrary
    /// file read in a tool the operator runs without thinking about it.
    #[test]
    fn a_session_id_that_is_not_one_is_refused_before_it_becomes_a_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(journal_grants(dir.path(), "../../etc/passwd").is_err());
        assert!(journal_grants(dir.path(), "").is_err());
    }

    /// The human rendering names the loaded file and the state of the gates.
    ///
    /// A failure would mean that the text mode — the one an operator reads —
    /// keeps quiet about what the JSON says, and that the two outputs no longer
    /// describe the same policy.
    #[test]
    fn the_text_rendering_names_the_file_and_the_gates() {
        let (cfg, _d, path) = load(MINIMAL);
        let mut v = effective_policy_json(&cfg, Some(&path));
        v["config_source"] = json!("explicit");
        let text = render_text(&v);
        assert!(text.contains(&path), "the loaded file must be named");
        assert!(text.contains("default effect"));
        assert!(text.contains("deny"));
        assert!(
            text.contains("session grants"),
            "the absence of session grants must be stated"
        );
    }

    /// Without a file, the rendering says « compiled defaults » rather than nothing.
    ///
    /// A failure would let an operator believe he is reading his file when
    /// none was found — the exact blind spot of the 2026-08-15 audit.
    #[test]
    fn with_no_file_the_text_says_the_defaults_are_compiled_in() {
        let cfg = AppConfig::defaults();
        let text = render_text(&effective_policy_json(&cfg, None));
        assert!(text.contains("compiled-in defaults"), "{text}");
    }
}
