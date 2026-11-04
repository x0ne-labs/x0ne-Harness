//! IR → core WASM module. About twenty instructions.
//!
//! **Structural decision no. 2 of the spec**: the regex lives on the host side
//! and capabilities go through indices, so replacement is a *host function*.
//! The v0 module is a loop that calls `fs_list` then, per id, the verb's
//! effect. The WASM layer is the **vehicle** of the capability model, not a
//! compute engine — this is intentional, and it will become an engine when the
//! verbs grow. The innovation is the derivation (imports + manifest →
//! policy), not the code generation.
//!
//! What this emitter **is not**: a source of truth about capabilities. It is
//! outside the TCB. What it produces is re-read by `crate::read`, on the bytes,
//! and proven by `x0ne_runtime::asm_verify` against the actual policy. A lying
//! emitter gains nothing but a later refusal.

use crate::abi::MODULE_NAMESPACE;
use crate::diag::{Refusal, RuleId};
use crate::ir::{Filter, Program, Verb};
use crate::limits::MANIFEST_VERSION;
use crate::manifest::{Capability, Manifest, ManifestEntry, SECTION_NAME};
use wasm_encoder::{
    BlockType, CodeSection, CustomSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, Instruction, MemArg, MemorySection, MemoryType, Module,
    TypeSection, ValType,
};

/// Address of the id buffer in the module's memory. Buffer at 0.
const IDS_PTR: i32 = 0;
/// Capacity of the id buffer: `MAX_FILES` ids of 4 bytes.
const IDS_CAP: i32 = (crate::limits::MAX_FILES * 4) as i32;
/// One wasm page.
const PAGE: u64 = 64 * 1024;

/// Which host functions this verb uses, in import order.
fn imports_for(verb: &Verb) -> Vec<&'static str> {
    match verb {
        Verb::Replace { .. } => vec!["fs_list", "fs_edit_buffered"],
        // The host applies the filter and counts during `fs_list`: it already
        // scans the content for `where lines match`. The module computes nothing.
        Verb::CountFiles | Verb::CountMatches => vec!["fs_list"],
        Verb::ReadLines(_) => vec!["fs_list", "fs_read"],
    }
}

/// 64 Ki pages. A module that reads no content only needs the id buffer;
/// `read lines` needs the id buffer **plus** a whole file. Sizing per verb
/// rather than at the maximum keeps `edit`/`count` modules at one page, which
/// is what the verifier sees most often.
///
/// SECURITY: undersizing does not produce a refusal but a **wrong result**:
/// the host cannot write the content into a memory that is too small, returns
/// `-E_2BIG`, and the program returns an empty table with no visible error.
fn pages_for(verb: &Verb) -> u64 {
    let bytes = match verb {
        Verb::ReadLines(_) => IDS_CAP as u64 + crate::limits::MAX_FILE_BYTES,
        Verb::Replace { .. } | Verb::CountFiles | Verb::CountMatches => IDS_CAP as u64,
    };
    bytes.div_ceil(PAGE).max(1)
}

/// Compiles a program into a core WASM module, manifest included.
///
/// `root` is the **absolute, canonical** project root; the caller resolved it
/// (`x0ne_runtime::asm_verify::project_root`). This crate does not touch the disk.
pub fn compile(program: &Program, root: &str) -> Result<Vec<u8>, Refusal> {
    if !root.starts_with('/') {
        return Err(Refusal::new(
            RuleId::PatternEscapesRoot,
            1,
            1,
            format!("project root `{root}`"),
            "the project root must be an absolute canonical path",
            "this is an X0NE internal error: report it, the program itself is fine",
        ));
    }
    // SECURITY: the capability comes from the **verb**, never from the pattern.
    // That is the derivation: `count` and `read` cannot claim write access,
    // whatever the pattern designates.
    let capability = match program.verb {
        Verb::Replace { .. } => Capability::ReadWrite,
        Verb::CountFiles | Verb::CountMatches | Verb::ReadLines(_) => Capability::Read,
    };
    let pattern = format!("{}/{}", root.trim_end_matches('/'), program.source.raw);

    let mut regexes = Vec::new();
    let mut templates = Vec::new();
    if let Some(Filter::Name(rx) | Filter::Lines(rx)) = &program.filter {
        regexes.push(rx.clone());
    }
    match &program.verb {
        Verb::Replace { find, template } => {
            regexes.push(find.clone());
            templates.push(template.clone());
        }
        Verb::ReadLines(rx) => regexes.push(rx.clone()),
        Verb::CountFiles | Verb::CountMatches => {}
    }

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        entries: vec![ManifestEntry {
            pattern,
            capability,
        }],
        regexes,
        templates,
    };
    assemble(
        imports_for(&program.verb),
        &manifest,
        effect_args(program),
        pages_for(&program.verb),
    )
}

/// The "pure" module: no import, no claim. Used by programs that touch
/// nothing, and by the test that holds the property.
pub fn compile_pure(_program: &Program) -> Result<Vec<u8>, Refusal> {
    let manifest = Manifest {
        version: MANIFEST_VERSION,
        entries: vec![],
        regexes: vec![],
        templates: vec![],
    };
    assemble(vec![], &manifest, None, 1)
}

/// The **two i32 arguments that follow the id** in the per-file call, when
/// the verb makes one.
///
/// The name does not say "index" because the two are not always indices, and
/// this is the only asymmetry of the skeleton:
///
/// - `Replace` → `(regex_idx, template_idx)`, two indices into the manifest
///   tables. If a `where` is present it takes regex index 0 and the verb's
///   regex follows — `compile` builds the manifest in that order.
/// - `ReadLines` → `(out_ptr, out_cap)`, an address and a capacity:
///   `fs_read(id, out_ptr, out_cap)`. The content is stored **after** the id
///   buffer, and the capacity is that of a whole file. Using `IDS_CAP`
///   (2,048 bytes) would return `-E_2BIG` on nearly every file, and the
///   program would return an empty table **with no visible error** — a wrong
///   result rather than a refusal.
fn effect_args(p: &Program) -> Option<(i32, i32)> {
    match &p.verb {
        Verb::Replace { .. } => Some((if p.filter.is_some() { 1 } else { 0 }, 0)),
        Verb::ReadLines(_) => Some((IDS_CAP, crate::limits::MAX_FILE_BYTES as i32)),
        Verb::CountFiles | Verb::CountMatches => None,
    }
}

fn assemble(
    imports: Vec<&'static str>,
    manifest: &Manifest,
    effect: Option<(i32, i32)>,
    pages: u64,
) -> Result<Vec<u8>, Refusal> {
    // SECURITY: `encode`, not `encode_unchecked`. A `Program` read back by serde
    // is **not** revalidated against the text rules (pinned in T2 by
    // `a_deserialized_program_is_not_revalidated_against_the_text_rules`): its
    // glob can climb. The emitter then refuses to write an artifact whose
    // manifest it knows will not read back. This is not the safeguard — the
    // safeguard is `read::read_module` on the bytes, plus the path-by-path
    // policy — it is the refusal that comes early and names the rule.
    let manifest_bytes = crate::manifest::encode(manifest).map_err(|e| {
        Refusal::new(
            RuleId::PatternEscapesRoot,
            1,
            1,
            e.to_string(),
            "a compiled program claims an absolute, canonical file pattern anchored at the \
             project root",
            "rewrite the `from files` pattern so that it stays inside the project: no leading \
             `/`, no `..` segment",
        )
    })?;

    let mut module = Module::new();

    // Types: (i32,i32,i32)->i32 for fs_list/fs_read/fs_edit_buffered, ()->()
    // for `run`. `emit` is not emitted in v0 (rendering is on the host side).
    let mut types = TypeSection::new();
    types
        .ty()
        .function([ValType::I32, ValType::I32, ValType::I32], [ValType::I32]);
    types.ty().function([], []);
    module.section(&types);

    let mut import_section = ImportSection::new();
    for name in &imports {
        import_section.import(MODULE_NAMESPACE, name, EntityType::Function(0));
    }
    module.section(&import_section);

    let mut funcs = FunctionSection::new();
    funcs.function(1); // `run`
    module.section(&funcs);

    // SECURITY: `minimum == maximum`. A memory bounded on both sides cannot
    // grow: `memory.grow` fails, and the module's memory budget is decided at
    // compile time, by the verb, not by the module at run time.
    let mut mems = MemorySection::new();
    mems.memory(MemoryType {
        minimum: pages,
        maximum: Some(pages),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&mems);

    let run_index = imports.len() as u32;
    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("run", ExportKind::Func, run_index);
    module.section(&exports);

    let mut code = CodeSection::new();
    code.function(&run_body(&imports, effect));
    module.section(&code);

    // The custom section comes last: its position does not change the
    // semantics, and placing it after the code keeps the program bytes
    // grouped for whoever reads the module by hand.
    module.section(&CustomSection {
        name: SECTION_NAME.into(),
        data: manifest_bytes.into(),
    });

    Ok(module.finish())
}

/// The skeleton: `n = fs_list(0, IDS_PTR, IDS_CAP)` then, for each id, the
/// verb's effect. Twenty instructions, no branching other than the loop.
fn run_body(imports: &[&'static str], effect: Option<(i32, i32)>) -> Function {
    // locals: $n (0), $i (1)
    let mut f = Function::new([(2, ValType::I32)]);
    if imports.is_empty() {
        f.instruction(&Instruction::End);
        return f;
    }

    f.instruction(&Instruction::I32Const(0)) // manifest_idx
        .instruction(&Instruction::I32Const(IDS_PTR))
        .instruction(&Instruction::I32Const(IDS_CAP))
        .instruction(&Instruction::Call(0)) // fs_list
        .instruction(&Instruction::LocalSet(0));

    f.instruction(&Instruction::Block(BlockType::Empty));
    // n < 0: the host refused (budget, capability, index) — exit, the error is
    // already recorded on the host side. The module has nothing to report.
    f.instruction(&Instruction::LocalGet(0))
        .instruction(&Instruction::I32Const(0))
        .instruction(&Instruction::I32LtS)
        .instruction(&Instruction::BrIf(0));

    if let (Some((arg1, arg2)), true) = (effect, imports.len() > 1) {
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(1))
            .instruction(&Instruction::LocalGet(0))
            .instruction(&Instruction::I32GeS)
            .instruction(&Instruction::BrIf(1)); // exits the block
                                                 // id = ids[i]
        f.instruction(&Instruction::LocalGet(1))
            .instruction(&Instruction::I32Const(4))
            .instruction(&Instruction::I32Mul)
            .instruction(&Instruction::I32Load(MemArg {
                offset: IDS_PTR as u64,
                align: 2,
                memory_index: 0,
            }))
            .instruction(&Instruction::I32Const(arg1))
            .instruction(&Instruction::I32Const(arg2))
            .instruction(&Instruction::Call(1)) // fs_edit_buffered or fs_read
            .instruction(&Instruction::Drop);
        f.instruction(&Instruction::LocalGet(1))
            .instruction(&Instruction::I32Const(1))
            .instruction(&Instruction::I32Add)
            .instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::Br(0))
            .instruction(&Instruction::End); // loop
    }

    f.instruction(&Instruction::End); // block
    f.instruction(&Instruction::End); // func
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, read};

    fn compiled(src: &str) -> Vec<u8> {
        let p = parse(src).expect("parse");
        compile(&p, "/w").expect("compile")
    }

    /// The module validates under MVP, exports what it must, has no `start`,
    /// and imports **only** what its verb uses.
    #[test]
    fn the_edit_skeleton_imports_only_what_the_verb_uses() {
        let bytes = compiled(
            "#!asm-ai/v0\nfrom files \"tests/**/*.rs\"\nedit replace /a(b)/ -> \"${1}\"\nemit diff\n",
        );
        assert!(bytes.len() < crate::limits::MAX_MODULE_BYTES);
        let facts = read::read_module(&bytes).expect("relecture");
        let names: Vec<&str> = facts.imports.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["fs_list", "fs_edit_buffered"]);
        assert!(!facts.has_start, "the start section is forbidden");
        assert!(facts.exports_memory && facts.exports_run);
        // The manifest travels inside the artifact and it is absolute.
        assert_eq!(facts.manifest.entries.len(), 1);
        assert_eq!(facts.manifest.entries[0].pattern, "/w/tests/**/*.rs");
        assert!(facts.manifest.entries[0].capability.may_write());
    }

    /// A `count` does not request write capability. The capability comes from
    /// the **verb**, not the pattern: that is the derivation, and it is the
    /// innovation the pitch claims.
    #[test]
    fn a_counting_program_never_claims_write_capability() {
        let bytes = compiled(
            "#!asm-ai/v0\nfrom files \"src/**/*.rs\"\nwhere lines match /unwrap/\ncount matches\nemit value\n",
        );
        let facts = read::read_module(&bytes).expect("relecture");
        assert!(!facts.manifest.entries[0].capability.may_write());
        let names: Vec<&str> = facts.imports.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["fs_list"]);
    }

    /// SECURITY: the read buffer is sized for a file, and the module's memory
    /// is sized for the buffer. Both together: a `read lines` on a normal file
    /// returns lines, not `-E_2BIG`.
    ///
    /// A failure here means an empty table returned as a success — a wrong
    /// result, which is worse than a refusal.
    #[test]
    fn a_reading_program_gets_a_buffer_that_fits_a_whole_file() {
        let bytes =
            compiled("#!asm-ai/v0\nfrom files \"docs/**/*.md\"\nread lines /^## /\nemit table\n");
        let facts = read::read_module(&bytes).expect("relecture");
        let names: Vec<&str> = facts.imports.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["fs_list", "fs_read"]);
        assert!(!facts.manifest.entries[0].capability.may_write());
        // 33 pages: the id buffer plus one file of MAX_FILE_BYTES.
        assert_eq!(facts.memory_pages, Some(33));
    }

    /// SECURITY: the "pure module" test — a program that asks for nothing
    /// imports nothing.
    ///
    /// The parent spec makes it a supporting scene: "the argument is not the
    /// 7 MB of Python, it is zero capability requested". The property is held
    /// by this test, never by design alone: a "pure" module that gained an
    /// import must make the suite fail.
    #[test]
    fn a_program_that_asks_for_nothing_imports_nothing() {
        let p = crate::ir::Program::new(
            crate::ir::Glob { raw: String::new() },
            None,
            crate::ir::Verb::CountFiles,
            crate::ir::Sink::Value,
        )
        .expect("count files -> emit value is a legal cell");
        let bytes = compile_pure(&p).expect("compile");
        let facts = read::read_module(&bytes).expect("relecture");
        assert!(facts.imports.is_empty(), "a pure module imports nothing");
        assert!(facts.manifest.entries.is_empty(), "and claims nothing");
    }

    /// SECURITY: the root the emitter receives must be absolute. A relative
    /// root would produce a relative manifest pattern — hence a claim that
    /// means two things depending on who reads it (invariant I-B), and a
    /// `decode` that would refuse it later with a message that helps nobody.
    /// The refusal comes here, and it says the error is X0NE's, not the
    /// program's.
    #[test]
    fn a_relative_project_root_is_refused_by_the_emitter() {
        let p = parse("#!asm-ai/v0\nfrom files \"s/*.rs\"\ncount files\nemit value\n").unwrap();
        let refusal = compile(&p, "relative/root").expect_err("a relative root is refused");
        assert_eq!(refusal.rule, crate::diag::RuleId::PatternEscapesRoot);
        assert!(compile(&p, "/w").is_ok(), "and the absolute one passes");
    }

    /// The memory is bounded on **both** sides: `minimum == maximum`, so
    /// `memory.grow` cannot give the module more than the verb justified. A
    /// module that could grow would decide at run time a budget that
    /// compilation had fixed.
    #[test]
    fn the_module_memory_cannot_grow() {
        let bytes = compiled("#!asm-ai/v0\nfrom files \"s/*.rs\"\ncount files\nemit value\n");
        let facts = read::read_module(&bytes).expect("relecture");
        assert_eq!(facts.memory_pages, Some(1));
        assert_eq!(
            facts.memory_maximum_pages,
            Some(1),
            "a memory without a maximum can grow"
        );
    }

    /// SECURITY: the manifest the emitter writes is the one the reader reads
    /// back — **same bytes**, a single section. Two `x0ne-manifest` sections
    /// in an artifact would leave the choice to the reader, and a choice is
    /// exactly what invariant I-B forbids.
    #[test]
    fn the_artifact_carries_exactly_one_manifest_section() {
        let bytes = compiled("#!asm-ai/v0\nfrom files \"s/*.rs\"\ncount files\nemit value\n");
        let sections = bytes
            .windows(crate::manifest::SECTION_NAME.len())
            .filter(|w| *w == crate::manifest::SECTION_NAME.as_bytes())
            .count();
        assert_eq!(sections, 1, "exactly one `x0ne-manifest` section");
    }

    /// SECURITY: a **deserialized** `Program` is not revalidated against the
    /// text rules — measured and pinned in T2 by
    /// `a_deserialized_program_is_not_revalidated_against_the_text_rules`. Its
    /// glob can therefore climb. The emitter then refuses to write the
    /// artifact: a manifest that would not read back has no reason to exist.
    ///
    /// This test does **not** prove the chain holds — the safeguard is
    /// `read::read_module` on the bytes plus the path-by-path policy, and
    /// corpus C proves it on a hand-forged module, without going through this
    /// emitter. It proves that the refusal comes early and names the rule.
    #[test]
    fn a_climbing_glob_from_a_deserialized_program_never_becomes_an_artifact() {
        for raw in ["../../etc/**", "s/../../etc/*"] {
            let json = format!(
                r#"{{"source":{{"raw":"{raw}"}},"filter":null,"verb":"CountFiles","sink":"Value"}}"#
            );
            let p: crate::ir::Program =
                serde_json::from_str(&json).expect("T2 does not revalidate the glob on reread");
            let refusal = compile(&p, "/w").expect_err("the emitter refuses `{raw}`");
            assert_eq!(refusal.rule, crate::diag::RuleId::PatternEscapesRoot);
        }
    }

    /// The manifest's regex order is the one the skeleton indexes. A `where`
    /// takes index 0 and the verb's regex follows; without `where`, the verb's
    /// regex is at 0. Swapping them would replace by the filter and filter by
    /// the replacement — a wrong diff, silently.
    #[test]
    fn the_verb_regex_index_follows_the_optional_filter() {
        let with = parse(
            "#!asm-ai/v0\nfrom files \"s/*.rs\"\nwhere lines match /keep/\nedit replace /a/ -> \"b\"\nemit diff\n",
        )
        .unwrap();
        assert_eq!(effect_args(&with), Some((1, 0)));
        let without =
            parse("#!asm-ai/v0\nfrom files \"s/*.rs\"\nedit replace /a/ -> \"b\"\nemit diff\n")
                .unwrap();
        assert_eq!(effect_args(&without), Some((0, 0)));

        // And the manifest does store the two regexes in that order.
        let bytes = compile(&with, "/w").unwrap();
        let facts = read::read_module(&bytes).unwrap();
        assert_eq!(facts.manifest.regexes[0].pattern, "keep");
        assert_eq!(facts.manifest.regexes[1].pattern, "a");
    }
}
