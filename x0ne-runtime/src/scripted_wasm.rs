//! asm-ai host: wasmtime engine, the four host functions, the id table, budgets.
//!
//! Compiled only under `asm-exec` (off by default). Nothing in this module
//! instantiates or runs a module: that is T6, and it will need an explicit
//! opt-in on top of the feature. Buffers, not writes: `fs_edit_buffered`
//! keeps new contents host-side; the disk is never touched here.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;

use wasmtime::{Caller, Config, Linker, Strategy};
use x0ne_asm::abi::MODULE_NAMESPACE;
use x0ne_asm::ir::{RegexLit, Template, TemplatePart};
use x0ne_asm::limits::{
    DFA_SIZE_LIMIT, E_2BIG, E_BAD_INDEX, E_BUDGET, E_CAPABILITY, E_INTERNAL, E_UTF8,
    MAX_BYTES_BUFFERED, MAX_BYTES_READ_TOTAL, MAX_EMIT_BYTES, MAX_FILE_BYTES, REGEX_PATTERN_CHARS,
    REGEX_SIZE_LIMIT,
};
use x0ne_asm::manifest::Capability;

use crate::executor::confine::ConfinedPath;

/// SECURITY: the key tying human-granted authority (`ResourceIdentity::Module`,
/// orchestrator side) to a run. One implementation per crate; re-exported,
/// not rewritten (`scripted.rs`).
pub use crate::scripted::hash_source;

/// Fuel per run. ~20 instructions per file; 512 files × 64 leaves two orders
/// of magnitude while bounding a runaway loop.
pub const RUN_FUEL: u64 = 4_000_000;
/// Epoch window in milliseconds. Without async the epoch traps: wanted in v0.
pub const EPOCH_MILLIS: u64 = 250;
/// Wall-clock cap: EPOCH_TICKS × EPOCH_MILLIS = 2 s.
pub const EPOCH_TICKS: u64 = 8;

/// SECURITY: post-MVP proposals explicitly disabled on the engine. One
/// `c.wasm_*(false)` call per name in `engine_config`, same order.
pub const DISABLED_ON_THE_ENGINE: &[&str] = &[
    "MULTI_VALUE",
    "BULK_MEMORY",
    "SIMD",
    "RELAXED_SIMD",
    "TAIL_CALL",
    "MULTI_MEMORY",
    "MEMORY64",
    "EXTENDED_CONST",
    "CUSTOM_PAGE_SIZES",
    "WIDE_ARITHMETIC",
    "STACK_SWITCHING",
    "SHARED_EVERYTHING_THREADS",
    "EXCEPTIONS",
];

/// SECURITY: proposals this build of wasmtime does not expose at all: no
/// `Config` knob (MVP or baseline), or behind a cargo feature we do not
/// compile (`gc`, `threads`, `component-model`). Not compiling is stronger
/// than `false`; a cfg-gated line that never compiles would read as a
/// protection without being one. The module is validated under MVP before it
/// reaches the engine, so none of these forms ever gets here.
pub const NOT_SETTABLE_ON_THE_ENGINE: &[&str] = &[
    // Baseline: no knob, ever (wasmtime 36.0.13 src/config.rs, read 2026-10-06).
    // LEGACY_EXCEPTIONS: the knob exists but is deprecated, "internal usage
    // with the spec testsuite, may be removed without warning"; not relied on.
    "LEGACY_EXCEPTIONS",
    "MUTABLE_GLOBAL",
    "SATURATING_FLOAT_TO_INT",
    "SIGN_EXTENSION",
    "FLOATS",
    "MEMORY_CONTROL",
    "GC_TYPES",
    "CALL_INDIRECT_OVERLONG",
    "BULK_MEMORY_OPT",
    // Behind `gc`, not compiled (reference types included, in this version).
    "REFERENCE_TYPES",
    "FUNCTION_REFERENCES",
    "GC",
    // Behind `threads`, not compiled.
    "THREADS",
    // Behind `component-model` / `component-model-async`, not compiled.
    "COMPONENT_MODEL",
    "CM_VALUES",
    "CM_NESTED_NAMES",
    "CM_ERROR_CONTEXT",
    "CM_FIXED_SIZE_LIST",
    "CM_GC",
    "CM_ASYNC",
    "CM_ASYNC_STACKFUL",
    "CM_ASYNC_BUILTINS",
];

/// The engine configuration: one, derived from the same intent as
/// `x0ne_asm::abi::WASM_FEATURES`.
pub fn engine_config() -> Config {
    let mut c = Config::new();
    // SECURITY: Cranelift explicitly. CVE-2026-34987 targets Winch; a security
    // decision is not left to an upstream default.
    c.strategy(Strategy::Cranelift);
    c.consume_fuel(true);
    c.epoch_interruption(true);
    // One call per name of DISABLED_ON_THE_ENGINE, same order.
    c.wasm_multi_value(false);
    c.wasm_bulk_memory(false);
    c.wasm_simd(false);
    c.wasm_relaxed_simd(false);
    c.wasm_tail_call(false);
    c.wasm_multi_memory(false);
    c.wasm_memory64(false);
    c.wasm_extended_const(false);
    c.wasm_custom_page_sizes(false);
    c.wasm_wide_arithmetic(false);
    c.wasm_stack_switching(false);
    c.wasm_shared_everything_threads(false);
    c.wasm_exceptions(false);
    c
}

/// What went wrong before or during a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    Regex { pattern: String, reason: String },
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Regex { pattern, reason } => write!(f, "regex /{pattern}/: {reason}"),
        }
    }
}

impl std::error::Error for RunError {}

/// A file opened to the program. The index in `HostState::files` IS the id.
#[derive(Debug)]
pub struct FileSlot {
    path: ConfinedPath,
    /// SECURITY: the manifest entry this file came from. It carries the
    /// capability; not the path, not a prefix.
    manifest_idx: usize,
    capability: Capability,
}

impl FileSlot {
    pub fn new(path: ConfinedPath, manifest_idx: usize, capability: Capability) -> Self {
        Self {
            path,
            manifest_idx,
            capability,
        }
    }
}

/// What was observed of a file when it was read. Compared at commit, before
/// any write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Witness {
    pub dev: u64,
    pub ino: u64,
    pub sha256: [u8; 32],
}

pub struct HostState {
    files: Vec<FileSlot>,
    /// Buffered content by id. Absent = unchanged.
    buffers: HashMap<usize, Vec<u8>>,
    /// Witness by id, taken at first read.
    witnesses: HashMap<usize, Witness>,
    /// Original content by id: the diff and the commit need it.
    originals: HashMap<usize, Vec<u8>>,
    emitted: Vec<u8>,
    bytes_read: u64,
    bytes_buffered: u64,
    /// Matched but non-UTF-8 files: skipped and reported, never edited.
    skipped: Vec<String>,
    regexes: Vec<regex::Regex>,
    templates: Vec<Template>,
    replacements: usize,
}

impl HostState {
    pub fn new(files: Vec<FileSlot>, regexes: Vec<regex::Regex>, templates: Vec<Template>) -> Self {
        Self {
            files,
            buffers: HashMap::new(),
            witnesses: HashMap::new(),
            originals: HashMap::new(),
            emitted: Vec::new(),
            bytes_read: 0,
            bytes_buffered: 0,
            skipped: Vec::new(),
            regexes,
            templates,
            replacements: 0,
        }
    }

    /// SECURITY: the only door into the id table. Dense (direct index),
    /// monotone (never reordered), bounded (`files.len()`), never recycled.
    fn slot(&self, id: usize) -> Result<&FileSlot, i32> {
        self.files.get(id).ok_or(-E_BAD_INDEX)
    }

    /// SECURITY: asks the capability of this id's manifest entry.
    fn check_write(&self, id: usize) -> Result<&FileSlot, i32> {
        let slot = self.slot(id)?;
        if !slot.capability.may_write() {
            tracing::warn!(
                target: "x0ne-runtime::asm",
                path = %slot.path.display(),
                manifest_idx = slot.manifest_idx,
                "a program tried to write through a read-only id"
            );
            return Err(-E_CAPABILITY);
        }
        Ok(slot)
    }

    /// Reads the file once, takes the witness, keeps the original.
    fn load(&mut self, id: usize) -> Result<Vec<u8>, i32> {
        if let Some(bytes) = self.originals.get(&id) {
            return Ok(bytes.clone());
        }
        let slot = self.slot(id)?;
        let meta = std::fs::metadata(slot.path.as_path()).map_err(|_| -E_INTERNAL)?;
        if meta.len() > MAX_FILE_BYTES {
            return Err(-E_BUDGET);
        }
        if self.bytes_read + meta.len() > MAX_BYTES_READ_TOTAL {
            return Err(-E_BUDGET);
        }
        let bytes = slot.path.read_bytes().map_err(|_| -E_INTERNAL)?;
        // SECURITY: the witness is taken at read time, not at commit. A witness
        // taken at commit and compared at commit proves nothing.
        let witness = Witness {
            dev: meta.dev(),
            ino: meta.ino(),
            sha256: sha256_of(&bytes),
        };
        self.bytes_read += bytes.len() as u64;
        self.witnesses.insert(id, witness);
        self.originals.insert(id, bytes.clone());
        Ok(bytes)
    }

    pub fn buffers(&self) -> &HashMap<usize, Vec<u8>> {
        &self.buffers
    }

    pub fn originals(&self) -> &HashMap<usize, Vec<u8>> {
        &self.originals
    }

    pub fn witnesses(&self) -> &HashMap<usize, Witness> {
        &self.witnesses
    }

    pub fn emitted(&self) -> &[u8] {
        &self.emitted
    }

    pub fn skipped(&self) -> &[String] {
        &self.skipped
    }

    pub fn replacements(&self) -> usize {
        self.replacements
    }

    pub fn path_of(&self, id: usize) -> Option<&ConfinedPath> {
        self.files.get(id).map(|f| &f.path)
    }
}

fn sha256_of(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

fn write_mem(caller: &mut Caller<'_, HostState>, ptr: i32, data: &[u8]) -> Result<(), i32> {
    if ptr < 0 {
        return Err(-E_2BIG);
    }
    let mem = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or(-E_INTERNAL)?;
    mem.write(caller, ptr as usize, data).map_err(|_| -E_2BIG)
}

fn read_mem(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> Result<Vec<u8>, i32> {
    if ptr < 0 || len < 0 || len as usize > MAX_EMIT_BYTES {
        return Err(-E_2BIG);
    }
    let mem = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or(-E_INTERNAL)?;
    let mut buf = vec![0u8; len as usize];
    mem.read(caller, ptr as usize, &mut buf)
        .map_err(|_| -E_2BIG)?;
    Ok(buf)
}

/// Defines only what the module imports: defining more would offer a
/// capability the verifier never saw.
pub fn link(linker: &mut Linker<HostState>, needed: &[&str]) -> anyhow::Result<()> {
    if needed.contains(&"fs_list") {
        linker.func_wrap(
            MODULE_NAMESPACE,
            "fs_list",
            |mut caller: Caller<'_, HostState>,
             manifest_idx: i32,
             out_ptr: i32,
             out_cap: i32|
             -> i32 {
                if manifest_idx < 0 {
                    return -E_BAD_INDEX;
                }
                let ids: Vec<u32> = caller
                    .data()
                    .files
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.manifest_idx == manifest_idx as usize)
                    .map(|(i, _)| i as u32)
                    .collect();
                let needed_bytes = ids.len() * 4;
                if out_cap < 0 || needed_bytes > out_cap as usize {
                    return -E_2BIG;
                }
                let mut buf = Vec::with_capacity(needed_bytes);
                for id in &ids {
                    buf.extend_from_slice(&id.to_le_bytes());
                }
                match write_mem(&mut caller, out_ptr, &buf) {
                    Ok(()) => ids.len() as i32,
                    Err(e) => e,
                }
            },
        )?;
    }
    if needed.contains(&"fs_read") {
        linker.func_wrap(
            MODULE_NAMESPACE,
            "fs_read",
            |mut caller: Caller<'_, HostState>, id: i32, out_ptr: i32, out_cap: i32| -> i32 {
                if id < 0 || out_cap < 0 {
                    return -E_BAD_INDEX;
                }
                let bytes = match caller.data_mut().load(id as usize) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                if bytes.len() > out_cap as usize {
                    return -E_2BIG;
                }
                match write_mem(&mut caller, out_ptr, &bytes) {
                    Ok(()) => bytes.len() as i32,
                    Err(e) => e,
                }
            },
        )?;
    }
    if needed.contains(&"fs_edit_buffered") {
        linker.func_wrap(
            MODULE_NAMESPACE,
            "fs_edit_buffered",
            |mut caller: Caller<'_, HostState>,
             id: i32,
             regex_idx: i32,
             template_idx: i32|
             -> i32 {
                if id < 0 || regex_idx < 0 || template_idx < 0 {
                    return -E_BAD_INDEX;
                }
                let (id, rx, tpl) = (id as usize, regex_idx as usize, template_idx as usize);
                // SECURITY: capability first, on this id's manifest entry.
                if let Err(e) = caller.data().check_write(id) {
                    return e;
                }
                if caller.data().regexes.get(rx).is_none()
                    || caller.data().templates.get(tpl).is_none()
                {
                    return -E_BAD_INDEX;
                }
                let bytes = match caller.data_mut().load(id) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                // Non-UTF-8 is skipped and reported, never edited, never a crash.
                let Ok(text) = String::from_utf8(bytes) else {
                    let path = caller.data().files[id].path.display();
                    caller.data_mut().skipped.push(path);
                    return -E_UTF8;
                };
                let st = caller.data_mut();
                let re = st.regexes[rx].clone();
                let template = st.templates[tpl].clone();
                let mut count = 0usize;
                // Line endings and the final newline survive byte for byte:
                // only matched ranges change.
                let out = re.replace_all(&text, |caps: &regex::Captures| {
                    count += 1;
                    expand(&template, caps)
                });
                let new_bytes = out.into_owned().into_bytes();
                if st.bytes_buffered + new_bytes.len() as u64 > MAX_BYTES_BUFFERED {
                    return -E_BUDGET;
                }
                st.bytes_buffered += new_bytes.len() as u64;
                st.replacements += count;
                st.buffers.insert(id, new_bytes);
                count as i32
            },
        )?;
    }
    if needed.contains(&"emit") {
        linker.func_wrap(
            MODULE_NAMESPACE,
            "emit",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i32 {
                let bytes = match read_mem(&mut caller, ptr, len) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                let st = caller.data_mut();
                if st.emitted.len() + bytes.len() > MAX_EMIT_BYTES {
                    return -E_BUDGET;
                }
                st.emitted.extend_from_slice(&bytes);
                0
            },
        )?;
    }
    Ok(())
}

/// `${n}` → group n. Indices were validated at compile time (R10), so an
/// absent group here means "did not participate": empty, like `regex`.
pub fn expand(template: &Template, caps: &regex::Captures) -> String {
    let mut out = String::new();
    for part in &template.parts {
        match part {
            TemplatePart::Literal(s) => out.push_str(s),
            TemplatePart::Group(n) => {
                out.push_str(caps.get(*n as usize).map(|m| m.as_str()).unwrap_or(""))
            }
        }
    }
    out
}

/// SECURITY: the regex is the most general point of the language. Bounded
/// three times (pattern length, compiled size, DFA size) and compiled here
/// from the manifest: the module only ever sees an index.
pub fn compile_regex(lit: &RegexLit) -> Result<regex::Regex, RunError> {
    if lit.pattern.chars().count() > REGEX_PATTERN_CHARS {
        return Err(RunError::Regex {
            pattern: lit.pattern.clone(),
            reason: format!("pattern is longer than {REGEX_PATTERN_CHARS} characters"),
        });
    }
    regex::RegexBuilder::new(&lit.pattern)
        .case_insensitive(lit.case_insensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(DFA_SIZE_LIMIT)
        .build()
        .map_err(|e| RunError::Regex {
            pattern: lit.pattern.clone(),
            reason: e.to_string(),
        })
}

#[cfg(test)]
impl HostState {
    fn for_test(n: usize) -> Self {
        Self::for_test_with((0..n).map(|i| (Capability::ReadWrite, i)).collect())
    }

    fn for_test_with(spec: Vec<(Capability, usize)>) -> Self {
        let dir = std::env::temp_dir();
        let files = spec
            .into_iter()
            .map(|(capability, manifest_idx)| FileSlot {
                path: ConfinedPath::for_test(dir.join("x")),
                manifest_idx,
                capability,
            })
            .collect();
        Self::new(files, Vec::new(), Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SECURITY: the verifier and the engine must accept the same language.
    /// A wasmparser update that adds a proposal fails this until someone
    /// decides what the engine does with it.
    #[test]
    fn every_post_mvp_proposal_is_accounted_for_on_both_sides() {
        let mut unresolved = Vec::new();
        for (name, flag) in wasmparser::WasmFeatures::all().iter_names() {
            if x0ne_asm::abi::WASM_FEATURES.contains(flag) {
                continue;
            }
            if DISABLED_ON_THE_ENGINE.contains(&name) || NOT_SETTABLE_ON_THE_ENGINE.contains(&name)
            {
                continue;
            }
            unresolved.push(name);
        }
        assert!(
            unresolved.is_empty(),
            "proposals refused by the verifier with no engine decision: {unresolved:?}"
        );
    }

    /// One `c.wasm_*(false)` per name, same order: the list is the code.
    #[test]
    fn the_two_engine_lists_do_not_overlap() {
        for name in DISABLED_ON_THE_ENGINE {
            assert!(
                !NOT_SETTABLE_ON_THE_ENGINE.contains(name),
                "{name} is in both lists"
            );
        }
    }

    /// Cranelift explicitly: a security decision is not left to an upstream
    /// default. Fuel and epochs are on, or the budgets below mean nothing.
    #[test]
    fn the_engine_is_built_on_cranelift_with_fuel_and_epochs() {
        let cfg = engine_config();
        let engine = wasmtime::Engine::new(&cfg).expect("engine builds");
        let mut store = wasmtime::Store::new(&engine, ());
        store.set_fuel(1_000).expect("fuel is enabled");
        store.set_epoch_deadline(1);
    }

    /// SECURITY: a forged id is contained, not prevented. The table is dense,
    /// monotone, bounded, never recycled: out of range is `-E_BAD_INDEX`,
    /// never a neighbour.
    #[test]
    fn a_forged_file_id_lands_out_of_bounds_not_on_a_neighbour() {
        let st = HostState::for_test(2);
        assert!(st.slot(0).is_ok());
        assert!(st.slot(1).is_ok());
        assert_eq!(st.slot(2).unwrap_err(), -E_BAD_INDEX);
        assert_eq!(st.slot(usize::MAX).unwrap_err(), -E_BAD_INDEX);
        assert_eq!(st.slot(-1isize as usize).unwrap_err(), -E_BAD_INDEX);
    }

    /// SECURITY: write capability is checked on the manifest entry that
    /// produced the id, never on a path prefix.
    #[test]
    fn writing_through_a_read_only_id_is_refused_by_its_own_manifest_entry() {
        let st = HostState::for_test_with(vec![(Capability::Read, 0), (Capability::ReadWrite, 1)]);
        assert_eq!(st.check_write(0).unwrap_err(), -E_CAPABILITY);
        assert!(st.check_write(1).is_ok());
    }

    /// Only the imports the module declares get a definition.
    #[test]
    fn link_defines_only_what_the_module_imports() {
        let engine = wasmtime::Engine::new(&engine_config()).unwrap();
        let mut linker: Linker<HostState> = Linker::new(&engine);
        link(&mut linker, &["emit"]).unwrap();
        let mut store = wasmtime::Store::new(&engine, HostState::for_test(0));
        assert!(linker.get(&mut store, MODULE_NAMESPACE, "emit").is_some());
        assert!(linker
            .get(&mut store, MODULE_NAMESPACE, "fs_read")
            .is_none());
    }

    /// The pattern is bounded before the builder, then by size, then by DFA.
    #[test]
    fn a_pattern_over_the_character_bound_is_refused_before_compiling() {
        let lit = RegexLit {
            pattern: "a".repeat(REGEX_PATTERN_CHARS + 1),
            case_insensitive: false,
        };
        assert!(matches!(compile_regex(&lit), Err(RunError::Regex { .. })));
        let ok = RegexLit {
            pattern: "fn (\\w+)".into(),
            case_insensitive: true,
        };
        assert!(compile_regex(&ok).unwrap().is_match("FN main"));
    }

    /// `${n}` for a group that did not participate expands to the empty string.
    #[test]
    fn a_non_participating_group_expands_to_nothing() {
        let re = regex::Regex::new("(a)|(b)").unwrap();
        let caps = re.captures("b").unwrap();
        let t = Template {
            parts: vec![
                TemplatePart::Literal("<".into()),
                TemplatePart::Group(1),
                TemplatePart::Group(2),
                TemplatePart::Literal(">".into()),
            ],
        };
        assert_eq!(expand(&t, &caps), "<b>");
    }
}
