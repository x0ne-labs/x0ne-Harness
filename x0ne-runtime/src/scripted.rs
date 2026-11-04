//! Scripted user-defined functions ("scripted primitives" — Tier 1).
//!
//! ## What this is
//!
//! A user-defined function is a small piece of source code (currently
//! [Rhai](https://rhai.rs/)) that the agent persists and can call later as
//! if it were a built-in typed primitive. Functions are typed at the
//! boundary (JSON in, JSON out), capability-controlled at the host glue
//! (no FS, no net, no clock unless you opt in), and small enough that
//! cold-call latency is in the microseconds.
//!
//! ## Why
//!
//! X0NE ships ~100 typed primitives. Real workflows always need one more —
//! "slugify this", "extract the canonical hostname", "compute the Levenshtein
//! ratio". Today the agent has to either inline the logic in every action
//! (bloats prompts, drifts) or spawn a Python service (heavy, untyped).
//!
//! Scripted functions cover the gap: the agent emits one
//! `<action>{"type":"DefineFunction", …}</action>` and from then on the
//! function is a first-class typed primitive. It survives across sessions
//! (`~/.x0ne/functions/<name>.json`), can be promoted to an MCP tool the
//! orchestrator exposes to other agents (Claude Code, Codex, sibling
//! x0ne sessions), and stays small/safe by construction.
//!
//! ## What this is NOT
//!
//! - Not a sandbox for Turing-complete arbitrary code. Rhai has no async,
//!   no external IO, no clock unless we register them. This is the right
//!   cage for Tier 1.
//! - Not the WASM tier. That's Tier 2 (heavier modules, longer-running,
//!   pre-compiled). Tier 2 will live alongside this in a separate file
//!   (`scripted_wasm.rs`) when we wire Wasmtime.
//! - Not the existing `composite.rs`. That family glues *typed actions*
//!   into a parameterised pipeline; this family runs *real code*. Two
//!   distinct mechanisms; both are user-extensible primitives.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

// ── Storage layout ───────────────────────────────────────────────────────────

/// Root directory: `~/.x0ne/functions/` (or `$X0NE_HOME/functions/`).
pub fn root_dir() -> PathBuf {
    if let Ok(s) = std::env::var("X0NE_HOME") {
        return PathBuf::from(s).join("functions");
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".x0ne").join("functions")
}

pub fn registry_path() -> PathBuf {
    root_dir().join("registry.json")
}

// ── Types ────────────────────────────────────────────────────────────────────

/// Tier of the scripted primitive — drives which engine runs it.
///
/// Today only `Rhai` is implemented. `Wasm` is reserved for Tier 2 and
/// will round-trip through serde the same way (`tier: "wasm"`) — adding
/// it later is purely additive, no migration of existing functions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum FunctionTier {
    #[default]
    Rhai,
    Wasm,
}

/// Single typed parameter — name + a hint for the JSON shape we expect.
/// Used at call time to pick a sensible coercion path (string → Rhai
/// `ImmutableString`, number → `INT` / `FLOAT`, bool → `bool`, anything
/// else → `Dynamic` JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionParam {
    pub name: String,
    /// JSON schema-ish type name: "string", "number", "integer", "boolean",
    /// "array", "object", "any". Defaults to "any".
    #[serde(default = "default_param_type")]
    pub r#type: String,
}

fn default_param_type() -> String {
    "any".into()
}

/// A persisted scripted function. The on-disk shape (`registry.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptedFunction {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tier: FunctionTier,
    pub source: String,
    #[serde(default)]
    pub params: Vec<FunctionParam>,
    /// Hint for callers: "any" if not declared. Pure documentation —
    /// not enforced at runtime (Rhai returns whatever the script returns).
    #[serde(default = "default_param_type")]
    pub returns: String,
    /// SHA-256 of `source` for reproducibility / cache-key purposes.
    pub source_hash: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FunctionRegistry {
    #[serde(default)]
    pub functions: HashMap<String, ScriptedFunction>,
}

impl FunctionRegistry {
    pub fn list_sorted(&self) -> Vec<&ScriptedFunction> {
        let mut v: Vec<&ScriptedFunction> = self.functions.values().collect();
        v.sort_by_key(|f| std::cmp::Reverse(f.created_at));
        v
    }
}

// ── Locking + load/save ──────────────────────────────────────────────────────

fn lock_path() -> PathBuf {
    root_dir().join("registry.lock")
}

/// Same advisory-lock pattern as `x0ne_artifact::RegistryLock` — busy-wait
/// with stale-lock break-out so two processes can safely read/mutate the
/// shared registry.
pub struct RegistryLock {
    _file: fs::File,
    path: PathBuf,
}

impl RegistryLock {
    pub fn acquire() -> std::io::Result<Self> {
        let dir = root_dir();
        fs::create_dir_all(&dir)?;
        let path = lock_path();
        for _ in 0..200 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Self { _file: f, path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        }
        let _ = fs::remove_file(&path);
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let _ = writeln!(f, "{}", std::process::id());
        Ok(Self { _file: f, path })
    }
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn load_registry() -> std::io::Result<FunctionRegistry> {
    let path = registry_path();
    if !path.exists() {
        return Ok(FunctionRegistry::default());
    }
    let mut s = String::new();
    fs::File::open(&path)?.read_to_string(&mut s)?;
    if s.trim().is_empty() {
        return Ok(FunctionRegistry::default());
    }
    serde_json::from_str::<FunctionRegistry>(&s).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("function registry parse: {}", e),
        )
    })
}

pub fn save_registry(reg: &FunctionRegistry) -> std::io::Result<()> {
    fs::create_dir_all(root_dir())?;
    let path = registry_path();
    let tmp = path.with_extension("json.tmp");
    let s = serde_json::to_string_pretty(reg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    fs::write(&tmp, s)?;
    fs::rename(tmp, path)?;
    Ok(())
}

pub fn with_registry<R>(f: impl FnOnce(&mut FunctionRegistry) -> R) -> std::io::Result<R> {
    let _lock = RegistryLock::acquire()?;
    let mut reg = load_registry()?;
    let out = f(&mut reg);
    save_registry(&reg)?;
    Ok(out)
}

// ── Source hashing ───────────────────────────────────────────────────────────

/// SHA-256 of the source — used as both a reproducibility marker (same
/// source → same hash → same function semantics) and a cheap dedup key.
pub fn hash_source(src: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(src.as_bytes());
    let out = h.finalize();
    let mut hex = String::with_capacity(64);
    for b in out.iter() {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{:02x}", b);
    }
    hex
}

// ── Name validation ──────────────────────────────────────────────────────────

/// Allowed function name: snake_case, alphanumeric + underscore, must start
/// with a letter, 3..=64 chars. Constrained on purpose: these names appear
/// in MCP tools/list and surface to the LLM in tool catalogs.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.len() < 3 {
        return Err(format!("function name '{}' too short (min 3 chars)", name));
    }
    if name.len() > 64 {
        return Err(format!("function name '{}' too long (max 64 chars)", name));
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() {
        return Err(format!(
            "function name '{}' must start with a lowercase letter (snake_case)",
            name
        ));
    }
    for c in chars {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return Err(format!(
                "function name '{}' contains invalid character '{}': use a-z, 0-9, _",
                name, c
            ));
        }
    }
    Ok(())
}

// ── Rhai engine ──────────────────────────────────────────────────────────────
//
// We build a fresh engine per call — Rhai engines are cheap (~50µs setup)
// and a per-call engine guarantees zero state bleed between invocations,
// which is critical for the "scripted primitive is a pure function" contract.
// If the call rate ever justifies it, we can pool engines later — the
// API doesn't change.

const DEFAULT_MAX_OPS: u64 = 5_000_000;
const DEFAULT_MAX_DEPTH: usize = 64;
// Rhai caps no size by default. Each cap applies to one value, nested
// content included (BLOB bytes count as array items); map KEYS are not counted.
const MAX_STRING_BYTES: usize = 1 << 20;
const MAX_ARRAY_LEN: usize = 1 << 14;
const MAX_MAP_LEN: usize = 1 << 12;
// rhaiscript/rhai#730: the caps bound one value, not how many variables hold one.
const MAX_VARIABLES: usize = 256;
// A size check walks the whole value on each change: a push/pop loop at 65,535
// items took 4.4 s per 10,000 rounds under the op cap (debug, 2026-10-16).
const MAX_CALL_TIME: Duration = Duration::from_secs(2);
// What one call may write to stderr, which is `~/.x0ne/logs/core.log` behind
// the bridge; one message is cut at SCRIPT_LINE_BYTES.
const SCRIPT_OUTPUT_BYTES: usize = 16 * 1024;
const SCRIPT_LINE_BYTES: usize = 1024;

/// `print` and `debug` of one call. Rhai's defaults `println!` to fd 1, which
/// in stdio mode is the bridge: the orchestrator took the line for an answer.
struct ScriptOutput {
    left: AtomicUsize,
    dropped: AtomicBool,
}

impl ScriptOutput {
    fn emit(&self, kind: &str, text: &str) {
        if self.dropped.load(Ordering::Relaxed) {
            return;
        }
        let cut = text.floor_char_boundary(SCRIPT_LINE_BYTES);
        let more = if cut < text.len() { " [truncated]" } else { "" };
        // `{:?}` escapes newlines: one message is one line, never a forged one.
        let line = format!("x0ne-runtime: rhai {kind}: {:?}{more}\n", &text[..cut]);
        let fits = self
            .left
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |l| {
                l.checked_sub(line.len())
            })
            .is_ok();
        let mut err = std::io::stderr().lock();
        if fits {
            let _ = err.write_all(line.as_bytes());
        } else if !self.dropped.swap(true, Ordering::Relaxed) {
            let _ = writeln!(
                err,
                "x0ne-runtime: rhai output dropped past {SCRIPT_OUTPUT_BYTES} bytes for this call"
            );
        }
    }
}

/// Cap a script's ops, time, recursion, value sizes and output so an agent
/// can't lock or flood the runtime. Ops are Rhai's coarse instruction counter;
/// ~5M ≈ 100ms of arithmetic on 2024 silicon.
fn build_engine() -> rhai::Engine {
    let mut engine = rhai::Engine::new();
    engine.set_max_operations(DEFAULT_MAX_OPS);
    engine.set_max_call_levels(DEFAULT_MAX_DEPTH);
    // Lift Rhai's default expression-depth ceiling (32) so honest
    // primitives like Levenshtein — which build a deep ternary /
    // chained-method expression — compile without "Expression exceeds
    // maximum complexity". 64 keeps the parser safe but accommodates
    // realistic agent-authored scripts.
    engine.set_max_expr_depths(64, 64);
    engine.set_max_string_size(MAX_STRING_BYTES);
    engine.set_max_array_size(MAX_ARRAY_LEN);
    engine.set_max_map_size(MAX_MAP_LEN);
    engine.set_max_variables(MAX_VARIABLES);
    let started = Instant::now();
    engine.on_progress(move |ops| {
        (ops % 256 == 0 && started.elapsed() > MAX_CALL_TIME)
            .then(|| rhai::Dynamic::from(format!("time limit of {}s", MAX_CALL_TIME.as_secs())))
    });
    // Disable `eval` — runtime self-reflection is footgun-grade.
    engine.disable_symbol("eval");
    // `Engine::new()` installs a FileModuleResolver: `import` ran any `.rhai`
    // on disk. Refused at compile (so by DefineFunction), and nothing resolves.
    engine.disable_symbol("import");
    engine.set_module_resolver(rhai::module_resolvers::DummyModuleResolver::new());
    engine.set_max_modules(0);
    let out = Arc::new(ScriptOutput {
        left: AtomicUsize::new(SCRIPT_OUTPUT_BYTES),
        dropped: AtomicBool::new(false),
    });
    let print_out = Arc::clone(&out);
    engine.on_print(move |s| print_out.emit("print", s));
    engine.on_debug(move |s, _, _| out.emit("debug", s));
    engine
}

/// Validate that the source compiles. Called from `DefineFunction` before
/// persisting — much better to fail at registration than at first call.
pub fn validate_source(src: &str) -> Result<(), String> {
    let engine = build_engine();
    engine
        .compile(src)
        .map(|_| ())
        .map_err(|e| format!("rhai compile error: {}", e))
}

/// Result of running a function — the JSON value returned and the wall-clock
/// duration in microseconds.
#[derive(Debug, Clone, Serialize)]
pub struct FunctionCallResult {
    pub value: Value,
    pub duration_us: u64,
}

/// Run the function. The script is expected to define a top-level `run`
/// function whose arity matches the registered `params` length. Args are
/// passed in the order declared in `params`.
///
/// Errors are categorised so the executor can pick the right `ErrorKind`:
/// `ScriptError::Compile` → `InvalidInput` (bad source, registration bug),
/// `ScriptError::Runtime` → `ExecutionFailed` (script panicked / hit cap),
/// `ScriptError::Marshal` → `InvalidInput` (caller-side, wrong JSON shape).
pub fn call_function(
    func: &ScriptedFunction,
    args: &[Value],
) -> Result<FunctionCallResult, ScriptError> {
    let started = Instant::now();
    match func.tier {
        FunctionTier::Rhai => call_rhai(func, args),
        FunctionTier::Wasm => Err(ScriptError::Compile(
            "WASM tier not implemented yet — Tier 2 lands in a follow-up commit".into(),
        )),
    }
    .map(|value| FunctionCallResult {
        value,
        duration_us: started.elapsed().as_micros() as u64,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    #[error("compile: {0}")]
    Compile(String),
    #[error("runtime: {0}")]
    Runtime(String),
    #[error("marshal: {0}")]
    Marshal(String),
}

fn call_rhai(func: &ScriptedFunction, args: &[Value]) -> Result<Value, ScriptError> {
    let engine = build_engine();
    let ast = engine
        .compile(&func.source)
        .map_err(|e| ScriptError::Compile(format!("{}", e)))?;
    // Marshal each JSON arg into a Rhai Dynamic. We use the JSON-Dynamic
    // bridge `Engine::parse_json` for arrays/objects; for primitives we
    // construct the right Rhai type directly to keep types tight.
    let mut rhai_args: Vec<rhai::Dynamic> = Vec::with_capacity(args.len());
    for (i, v) in args.iter().enumerate() {
        let dynamic =
            json_to_dynamic(v).map_err(|e| ScriptError::Marshal(format!("arg #{}: {}", i, e)))?;
        rhai_args.push(dynamic);
    }
    // Call top-level `run` with positional args. Rhai's
    // `call_fn` requires a Scope — we build a fresh empty one per call.
    let mut scope = rhai::Scope::new();
    let out: rhai::Dynamic = engine
        .call_fn(&mut scope, &ast, "run", rhai_args)
        .map_err(|e| {
            // Rhai distinguishes parse-time and runtime errors via the
            // EvalAltResult variants, but for the executor we collapse:
            // anything that runs the script and fails is "Runtime".
            if let rhai::EvalAltResult::ErrorTerminated(why, _) = e.unwrap_inner() {
                return ScriptError::Runtime(format!("script stopped: {why}"));
            }
            ScriptError::Runtime(format!("{}", e))
        })?;
    dynamic_to_json(out).map_err(ScriptError::Marshal)
}

/// JSON value → Rhai `Dynamic`. We keep the conversion shallow + total —
/// for any JSON shape we either build the matching Rhai type or fall back
/// to a string representation (and let the script downcast). We never
/// silently drop data.
fn json_to_dynamic(v: &Value) -> Result<rhai::Dynamic, String> {
    match v {
        Value::Null => Ok(rhai::Dynamic::UNIT),
        Value::Bool(b) => Ok(rhai::Dynamic::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(rhai::Dynamic::from(i))
            } else if let Some(f) = n.as_f64() {
                Ok(rhai::Dynamic::from(f))
            } else {
                Err(format!("non-finite number: {}", n))
            }
        }
        Value::String(s) => Ok(rhai::Dynamic::from(s.clone())),
        Value::Array(arr) => {
            let mut out: rhai::Array = Vec::with_capacity(arr.len());
            for item in arr {
                out.push(json_to_dynamic(item)?);
            }
            Ok(rhai::Dynamic::from(out))
        }
        Value::Object(obj) => {
            let mut out: rhai::Map = rhai::Map::new();
            for (k, v) in obj {
                out.insert(k.as_str().into(), json_to_dynamic(v)?);
            }
            Ok(rhai::Dynamic::from(out))
        }
    }
}

/// Rhai `Dynamic` → JSON value. Rhai's `Dynamic::cast()` handles the
/// primitive cases; for arrays/maps we recurse. Anything we can't classify
/// becomes its `format!("{:?}", …)` string — no panics, no silent loss
/// (the script author sees the textual repr).
fn dynamic_to_json(d: rhai::Dynamic) -> Result<Value, String> {
    if d.is_unit() {
        return Ok(Value::Null);
    }
    if d.is_bool() {
        return Ok(Value::Bool(d.as_bool().unwrap()));
    }
    if d.is_int() {
        return Ok(Value::Number(d.as_int().unwrap().into()));
    }
    if d.is_float() {
        let f = d.as_float().unwrap();
        return serde_json::Number::from_f64(f)
            .map(Value::Number)
            .ok_or_else(|| format!("non-finite float: {}", f));
    }
    if d.is_string() {
        let s = d.into_immutable_string().map_err(|e| e.to_string())?;
        return Ok(Value::String(s.to_string()));
    }
    if d.is_array() {
        let arr: rhai::Array = d.cast();
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            out.push(dynamic_to_json(item)?);
        }
        return Ok(Value::Array(out));
    }
    if d.is_map() {
        let map: rhai::Map = d.cast();
        let mut out = serde_json::Map::with_capacity(map.len());
        for (k, v) in map {
            out.insert(k.to_string(), dynamic_to_json(v)?);
        }
        return Ok(Value::Object(out));
    }
    // Fallback for unknown types (custom Rhai types, etc.) — preserve as
    // string so the agent at least sees what it got.
    Ok(Value::String(format!("{:?}", d)))
}

/// Best-effort cleanup: removes a function by name. Returns true if the
/// entry existed and was removed.
pub fn delete_function(name: &str) -> std::io::Result<bool> {
    with_registry(|reg| reg.functions.remove(name).is_some())
}

/// Self-test — small smoke check exposed for `x0ne-runtime --self-test`.
/// Returns true if the engine round-trips a no-op `run(x) { x }` correctly.
pub fn self_check() -> bool {
    let func = ScriptedFunction {
        name: "selftest".into(),
        description: None,
        tier: FunctionTier::Rhai,
        source: "fn run(x) { x }".into(),
        params: vec![FunctionParam {
            name: "x".into(),
            r#type: "any".into(),
        }],
        returns: "any".into(),
        source_hash: hash_source("fn run(x) { x }"),
        created_at: Utc::now(),
    };
    let res = call_function(&func, &[Value::String("ok".into())]);
    if let Err(e) = &res {
        warn!("scripted::self_check failed: {}", e);
    }
    matches!(res, Ok(r) if r.value == Value::String("ok".into()))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    // Tests that mutate X0NE_HOME share process-global env state; serialise
    // them to avoid races under `cargo test` parallel execution.
    static X0NE_HOME_LOCK: Mutex<()> = Mutex::new(());

    fn make_func(source: &str) -> ScriptedFunction {
        ScriptedFunction {
            name: "f".into(),
            description: None,
            tier: FunctionTier::Rhai,
            source: source.into(),
            params: vec![],
            returns: "any".into(),
            source_hash: hash_source(source),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn rejects_invalid_names() {
        assert!(validate_name("ab").is_err(), "too short");
        assert!(validate_name(&"x".repeat(65)).is_err(), "too long");
        assert!(validate_name("Slug").is_err(), "uppercase first");
        assert!(validate_name("0name").is_err(), "digit first");
        assert!(validate_name("name-bad").is_err(), "hyphen");
        assert!(validate_name("name bad").is_err(), "space");
        assert!(validate_name("name").is_ok());
        assert!(validate_name("levenshtein_ratio").is_ok());
        assert!(validate_name("slug2").is_ok());
    }

    #[test]
    fn validate_source_compiles_good_rhai() {
        assert!(validate_source(r#"fn run(s) { s + "!" }"#).is_ok());
    }

    #[test]
    fn validate_source_rejects_bad_rhai() {
        // Unclosed brace — Rhai parser must reject.
        assert!(validate_source("fn run(s) { s + ").is_err());
    }

    #[test]
    fn call_returns_string() {
        let f = make_func(r#"fn run(s) { s + "!" }"#);
        let r = call_function(&f, &[json!("hi")]).expect("call ok");
        assert_eq!(r.value, json!("hi!"));
        assert!(r.duration_us < 100_000, "should be sub-100ms");
    }

    #[test]
    fn call_returns_int() {
        let f = make_func("fn run(a, b) { a + b }");
        let r = call_function(&f, &[json!(40), json!(2)]).expect("call ok");
        assert_eq!(r.value, json!(42));
    }

    #[test]
    fn call_returns_object() {
        let f = make_func(r#"fn run(name) { #{ "greeting": "hello, " + name, "len": name.len } }"#);
        let r = call_function(&f, &[json!("world")]).expect("call ok");
        let obj = r.value.as_object().expect("object");
        assert_eq!(obj.get("greeting").unwrap(), &json!("hello, world"));
        assert_eq!(obj.get("len").unwrap(), &json!(5));
    }

    #[test]
    fn call_handles_array_in_array_out() {
        let f = make_func("fn run(xs) { let sum = 0; for x in xs { sum += x } sum }");
        let r = call_function(&f, &[json!([1, 2, 3, 4])]).expect("call ok");
        assert_eq!(r.value, json!(10));
    }

    #[test]
    fn slugify_pattern_works() {
        // Documents the Rhai gotcha: `.replace()` on a String mutates in
        // place and returns Unit. The agent must bind the lowered string
        // to a `let`, mutate, and then return the binding. This test pins
        // the working idiom so README + system-prompt-hint stay in sync
        // with reality.
        let src = r#"fn run(s) { let t = s.to_lower(); t.replace(" ", "-"); t }"#;
        let f = make_func(src);
        let r = call_function(&f, &[json!("Hello X0NE World")]).expect("call ok");
        assert_eq!(r.value, json!("hello-x0ne-world"));
    }

    #[test]
    fn levenshtein_ratio_pattern_works() {
        // Sketch of a more involved pure function — the kind of utility
        // an agent will want to mint as a primitive when it's doing
        // dedup / typo detection.
        let src = r#"
            fn run(a, b) {
                let la = a.len; let lb = b.len;
                if la == 0 { return if lb == 0 { 1.0 } else { 0.0 } }
                if lb == 0 { return 0.0 }
                let m = la + 1; let n = lb + 1;
                let prev = []; let curr = [];
                for j in 0..n { prev.push(j) }
                for i in 1..m {
                    curr = [i];
                    for j in 1..n { curr.push(0) }
                    for j in 1..n {
                        let cost = if a[i-1] == b[j-1] { 0 } else { 1 };
                        let del = prev[j] + 1;
                        let ins = curr[j-1] + 1;
                        let sub = prev[j-1] + cost;
                        let m1 = if del < ins { del } else { ins };
                        let m2 = if m1 < sub { m1 } else { sub };
                        curr[j] = m2;
                    }
                    prev = curr;
                }
                let dist = prev[lb];
                let big = if la > lb { la } else { lb };
                1.0 - (dist.to_float() / big.to_float())
            }
        "#;
        let f = make_func(src);
        let r = call_function(&f, &[json!("kitten"), json!("sitting")]).expect("call ok");
        // Levenshtein("kitten","sitting") = 3, max len = 7 → 1 - 3/7 ≈ 0.571
        let v = r.value.as_f64().expect("number");
        assert!((v - 4.0 / 7.0).abs() < 0.01, "got {}", v);
    }

    #[test]
    fn call_caps_runaway_loop() {
        // The op-cap should kick in well before we burn unbounded CPU.
        let f = make_func("fn run() { loop { } }");
        let err = call_function(&f, &[]).expect_err("must fail");
        match err {
            ScriptError::Runtime(_) => {}
            other => panic!("expected Runtime cap error, got {:?}", other),
        }
    }

    #[test]
    fn registry_round_trips_through_disk() {
        let _g = X0NE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("X0NE_HOME", tmp.path());
        let f = ScriptedFunction {
            name: "demo".into(),
            description: Some("demo".into()),
            tier: FunctionTier::Rhai,
            source: r#"fn run() { "ok" }"#.into(),
            params: vec![],
            returns: "string".into(),
            source_hash: hash_source(r#"fn run() { "ok" }"#),
            created_at: Utc::now(),
        };
        with_registry(|r| {
            r.functions.insert(f.name.clone(), f.clone());
        })
        .unwrap();
        let back = load_registry().unwrap();
        assert!(back.functions.contains_key("demo"));
        assert_eq!(back.functions["demo"].source_hash, f.source_hash);
        std::env::remove_var("X0NE_HOME");
    }

    #[test]
    fn self_check_round_trips() {
        let _g = X0NE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("X0NE_HOME", tmp.path());
        assert!(self_check());
        std::env::remove_var("X0NE_HOME");
    }

    #[test]
    fn hash_source_is_deterministic() {
        let h1 = hash_source("hello");
        let h2 = hash_source("hello");
        let h3 = hash_source("hello\n");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert_eq!(h1.len(), 64); // sha256 hex
    }

    /// (H-11): Rhai caps no size by default, so a doubling loop or one
    /// `pad`/`blob` call allocated whatever it asked for, under the op cap.
    /// Each case stops near 16 MiB here; with no cap, the size is the script's.
    #[test]
    fn oversized_values_fail_with_an_error() {
        let cases = [
            (
                "doubled string",
                r#"fn run() { let s = "x"; while s.len() < 16777216 { s += s; } s.len() }"#,
            ),
            (
                "padded string",
                r#"fn run() { let s = ""; s.pad(16777216, 'x'); s.len() }"#,
            ),
            ("blob", "fn run() { let b = blob(16777216, 0x42); b.len() }"),
            (
                "doubled array",
                "fn run() { let a = [0]; while a.len() < 1048576 { a += a; } a.len() }",
            ),
            (
                "padded array",
                "fn run() { let a = []; a.pad(1048576, 0); a.len() }",
            ),
            (
                "pushed array",
                "fn run() { let a = []; for i in 0..200000 { a.push(i); } a.len() }",
            ),
            (
                "array of strings",
                r#"fn run() { let s = "x"; while s.len() < 524288 { s += s; } let a = []; for i in 0..32 { a.push(s + i); } a.len() }"#,
            ),
            (
                "map",
                r#"fn run() { let m = #{}; for i in 0..20000 { m["k" + i] = i; } m.len() }"#,
            ),
        ];
        let mut wrong = Vec::new();
        for (what, src) in cases {
            match call_function(&make_func(src), &[]) {
                Err(ScriptError::Runtime(m)) if m.contains("too large") => {}
                other => wrong.push(format!("{what}: {other:?}")),
            }
        }
        assert!(wrong.is_empty(), "no size error for:\n{}", wrong.join("\n"));
    }

    /// (H-11): `Engine::new()` installs a FileModuleResolver, so `import`
    /// ran a `.rhai` file planted on disk, code DefineFunction never saw.
    #[test]
    fn an_import_never_runs_code_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("planted.rhai"),
            r#"fn pwned() { "LOADED-FROM-DISK" }"#,
        )
        .unwrap();
        let stem = dir.path().join("planted");
        let src = format!(
            r#"fn run() {{ import "{}" as m; m::pwned() }}"#,
            stem.display()
        );
        let called = call_function(&make_func(&src), &[]);
        let validated = validate_source(&src);
        let engine = build_engine();
        let resolved = rhai::ModuleResolver::resolve(
            engine.module_resolver(),
            &engine,
            None,
            &stem.to_string_lossy(),
            rhai::Position::NONE,
        )
        .map(|_| "module loaded");
        assert!(
            called.is_err() && validated.is_err() && resolved.is_err(),
            "call: {called:?}\nDefineFunction check: {validated:?}\nresolver: {resolved:?}"
        );
    }

    /// Once caps are set, Rhai re-measures the whole array on each push
    /// (4.4 s per 10,000 rounds at 65,535 items, debug, 2026-10-16); these
    /// 400,000 rounds stay under the op cap.
    #[test]
    fn a_call_stops_at_the_time_limit() {
        let src = "fn run() { let a = []; a.pad(16383, 0); \
                   for i in 0..400000 { a.push(i); a.pop(); } a.len() }";
        let t = Instant::now();
        let r = call_function(&make_func(src), &[]);
        let took = t.elapsed();
        assert!(
            matches!(&r, Err(ScriptError::Runtime(m)) if m.contains("time limit")),
            "{r:?}"
        );
        assert!(took.as_secs() < 5, "{took:?}");
    }
}
