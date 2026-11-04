//! Artifact → facts. **On the module bytes, never on the IR.**
//!
//! Reading the imports on the compiled artifact takes the compiler out of the TCB for
//! the critical question: CVE-2026-34971 showed that a Cranelift
//! miscompilation produces a complete escape, and a proof is not built on an
//! unverified compiler.
//!
//! This module refuses **everything that is not exactly the expected shape**:
//! four possible host functions and not one more, two exports and not one
//! more, a memory bounded on both sides, a manifest whose
//! write claims **match the imports**. Every "or else we
//! document that wasmtime will refuse" was decided the other way: the refusal
//! here is narrower, it comes earlier, and its message is written to be read.

use crate::abi::{self, WASM_FEATURES};
use crate::limits::{MAX_IMPORTS, MAX_MODULE_BYTES};
use crate::manifest::{decode, Manifest, ManifestError, SECTION_NAME};
use wasmparser::{Parser, Payload, TypeRef, ValType, Validator};

/// The name of the host function that grants writing. It is **the** pivot of the
/// I-1 cross-check: a write manifest entry without this import (or
/// the reverse) describes a program that is not the artifact's.
pub const EDIT_IMPORT: &str = "fs_edit_buffered";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    Func,
    Memory,
    Global,
    Table,
    Tag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRef {
    pub module: String,
    pub name: String,
    pub kind: ImportKind,
    /// The **complete** signature, rendered `(i32,i32)->(i32)`. Arity alone
    /// let `fs_list(i64,i64,i64)->i64` through: same number of parameters,
    /// different types, refusal pushed back to the wasmtime link — that is,
    /// after the human gate. `None` for non-Func kinds.
    pub signature: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModuleFacts {
    pub imports: Vec<ImportRef>,
    pub manifest: Manifest,
    pub has_start: bool,
    pub exports_memory: bool,
    pub exports_run: bool,
    /// Pages declared by the memory section. `None` if the module defines
    /// none. Read because a module whose memory is too small for its
    /// own read buffer returns empty results without an error — see
    /// `emit::pages_for`.
    pub memory_pages: Option<u64>,
    /// Declared growth ceiling. SECURITY: `read_module` requires it to be
    /// **equal** to `memory_pages`; the field stays public because the host (T5)
    /// sizes its buffers on it.
    pub memory_maximum_pages: Option<u64>,
    /// Does the module import the write function? SECURITY: this is what
    /// the manifest must corroborate, and it is what the verifier re-reads so
    /// that "what the artifact asks for" and "what the gate announces" are
    /// the same fact.
    pub imports_edit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    TooLarge {
        bytes: usize,
        max: usize,
    },
    Invalid {
        reason: String,
    },
    TooManyImports {
        found: usize,
        max: usize,
    },
    ImportOutsideAbi {
        module: String,
        name: String,
        kind: ImportKind,
    },
    /// SECURITY: the complete signature, not the arity. See `ImportRef::signature`.
    ImportSignature {
        name: String,
        found: String,
        expected: String,
    },
    StartSection,
    MissingExport {
        name: &'static str,
    },
    /// SECURITY: one more export is one more surface. Wasmtime would not
    /// call it — the host only calls `run` — but "the engine will not
    /// use it" is not a property of the artifact, it is a property
    /// of code yet to be written. Refusing is narrower, and it is checkable here.
    UnexpectedExport {
        name: String,
    },
    /// SECURITY: `run` must be `() -> ()`. A `run` that takes a parameter
    /// would be called with a value nobody decided.
    RunSignature {
        found: String,
    },
    /// SECURITY: a memory without a maximum, or whose maximum exceeds the
    /// minimum, can **grow**. The module's memory budget would then be
    /// decided by the module at run time, and not by the verb at compile time.
    /// `emit` never produces that; a forged artifact does.
    UnboundedMemory {
        pages: Option<u64>,
        maximum: Option<u64>,
    },
    /// SECURITY: invariant I-1 — the manifest and the import section must
    /// **say the same thing**. The manifest alone decided what the gate
    /// announces; an artifact importing `fs_edit_buffered` with a read-only
    /// manifest therefore made the gate announce "read" for a module that
    /// asks for editing, and a write manifest without the import made it
    /// open write tokens for a module that will never write.
    ///
    /// The product's thesis is "imports **and** manifest → policy". Without this
    /// cross-check, only the second half was true.
    CapabilityMismatch {
        manifest_claims_write: bool,
        imports_edit: bool,
    },
    NoManifest,
    /// Two `x0ne-manifest` sections: the reader would have to *choose*, and a
    /// choice is what invariant I-B forbids.
    DuplicateManifest,
    Manifest(ManifestError),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::TooLarge { bytes, max } => {
                write!(f, "module is {bytes} bytes, the limit is {max}")
            }
            ReadError::Invalid { reason } => write!(f, "module does not validate: {reason}"),
            ReadError::TooManyImports { found, max } => {
                write!(f, "module declares {found} imports, the limit is {max}")
            }
            ReadError::ImportOutsideAbi { module, name, kind } => write!(
                f,
                "module imports {kind:?} `{module}::{name}`, which is not part of the asm-ai host ABI"
            ),
            ReadError::ImportSignature {
                name,
                found,
                expected,
            } => write!(
                f,
                "`{name}` is imported as {found}, the host ABI declares {expected}"
            ),
            ReadError::StartSection => write!(
                f,
                "module has a start section; asm-ai modules run through the `run` export only"
            ),
            ReadError::MissingExport { name } => write!(f, "module does not export `{name}`"),
            ReadError::UnexpectedExport { name } => write!(
                f,
                "module exports `{name}`; an asm-ai module exports `memory` and `run`, and nothing else"
            ),
            ReadError::RunSignature { found } => write!(
                f,
                "`run` is exported as {found}; asm-ai calls it as () -> ()"
            ),
            ReadError::UnboundedMemory { pages, maximum } => write!(
                f,
                "module memory is declared min {pages:?} max {maximum:?}; an asm-ai module \
                 declares an equal minimum and maximum so that it cannot grow"
            ),
            ReadError::CapabilityMismatch {
                manifest_claims_write,
                imports_edit,
            } => write!(
                f,
                "the manifest and the import section disagree: the manifest claims write access \
                 on at least one pattern = {manifest_claims_write}, while the module imports \
                 `x0ne::{EDIT_IMPORT}` = {imports_edit}. A module's capabilities are derived from \
                 both, and both must say the same thing"
            ),
            ReadError::NoManifest => write!(f, "module has no `{SECTION_NAME}` custom section"),
            ReadError::DuplicateManifest => write!(
                f,
                "module carries more than one `{SECTION_NAME}` custom section; there must be \
                 exactly one claim to prove"
            ),
            ReadError::Manifest(e) => write!(f, "{e}"),
        }
    }
}

/// `(i32,i32)->(i32)`. A rendering, not a type: it exists only to be
/// compared and displayed. Reference types all render as `ref`, which loses
/// nothing under `WASM_FEATURES = MVP`: the validator refuses them first.
fn render_sig(params: &[ValType], results: &[ValType]) -> String {
    fn one(v: &ValType) -> &'static str {
        match v {
            ValType::I32 => "i32",
            ValType::I64 => "i64",
            ValType::F32 => "f32",
            ValType::F64 => "f64",
            ValType::V128 => "v128",
            ValType::Ref(_) => "ref",
        }
    }
    let j = |xs: &[ValType]| xs.iter().map(one).collect::<Vec<_>>().join(",");
    format!("({})->({})", j(params), j(results))
}

/// What the ABI expects of a function: all its parameters are `i32`
/// (invariant I-A: they are indices), and it returns an `i32` or nothing.
fn abi_sig(f: &abi::AbiFunc) -> String {
    let params = vec![ValType::I32; f.arity()];
    let results = if f.returns_i32 {
        vec![ValType::I32]
    } else {
        vec![]
    };
    render_sig(&params, &results)
}

pub fn read_module(bytes: &[u8]) -> Result<ModuleFacts, ReadError> {
    if bytes.len() > MAX_MODULE_BYTES {
        return Err(ReadError::TooLarge {
            bytes: bytes.len(),
            max: MAX_MODULE_BYTES,
        });
    }
    // SECURITY: validate BEFORE reading the sections. A parser that walks an
    // unvalidated module reads structures whose consistency nobody has
    // checked, and the validator is what makes the rest of the file readable.
    Validator::new_with_features(WASM_FEATURES)
        .validate_all(bytes)
        .map_err(|e| ReadError::Invalid {
            reason: e.to_string(),
        })?;

    let mut imports: Vec<ImportRef> = Vec::new();
    let mut manifest: Option<Manifest> = None;
    let mut has_start = false;
    let mut exports_memory = false;
    let mut exports_run = false;
    let mut memory_pages: Option<u64> = None;
    let mut memory_maximum_pages: Option<u64> = None;
    // Rendered signature per type index.
    let mut type_sigs: Vec<String> = Vec::new();
    // Type indices of the **imported** functions, in order: they are the
    // first indices of the function space.
    let mut imported_func_types: Vec<u32> = Vec::new();
    // Then those the module defines.
    let mut defined_func_types: Vec<u32> = Vec::new();
    // The exports, kept to be cross-checked in one pass after the walk.
    let mut exports: Vec<(String, wasmparser::ExternalKind, u32)> = Vec::new();

    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|e| ReadError::Invalid {
            reason: e.to_string(),
        })? {
            Payload::TypeSection(reader) => {
                for group in reader {
                    let group = group.map_err(|e| ReadError::Invalid {
                        reason: e.to_string(),
                    })?;
                    for ty in group.into_types() {
                        let ft = ty.unwrap_func();
                        type_sigs.push(render_sig(ft.params(), ft.results()));
                    }
                }
            }
            Payload::ImportSection(reader) => {
                for import in reader {
                    let import = import.map_err(|e| ReadError::Invalid {
                        reason: e.to_string(),
                    })?;
                    // SECURITY: **all kinds**, not only functions.
                    // An imported memory or table is a capability that
                    // nobody derived.
                    let (kind, signature) = match import.ty {
                        TypeRef::Func(i) => {
                            imported_func_types.push(i);
                            (ImportKind::Func, type_sigs.get(i as usize).cloned())
                        }
                        TypeRef::Memory(_) => (ImportKind::Memory, None),
                        TypeRef::Global(_) => (ImportKind::Global, None),
                        TypeRef::Table(_) => (ImportKind::Table, None),
                        TypeRef::Tag(_) => (ImportKind::Tag, None),
                    };
                    imports.push(ImportRef {
                        module: import.module.to_string(),
                        name: import.name.to_string(),
                        kind,
                        signature,
                    });
                }
            }
            Payload::FunctionSection(reader) => {
                for ty in reader {
                    defined_func_types.push(ty.map_err(|e| ReadError::Invalid {
                        reason: e.to_string(),
                    })?);
                }
            }
            Payload::StartSection { .. } => has_start = true,
            Payload::MemorySection(reader) => {
                for m in reader {
                    let m = m.map_err(|e| ReadError::Invalid {
                        reason: e.to_string(),
                    })?;
                    // The first one is enough: `MULTI_MEMORY` is outside MVP, so the
                    // validator has already refused a module that declares two.
                    if memory_pages.is_none() {
                        memory_pages = Some(m.initial);
                        memory_maximum_pages = m.maximum;
                    }
                }
            }
            Payload::ExportSection(reader) => {
                for e in reader {
                    let e = e.map_err(|er| ReadError::Invalid {
                        reason: er.to_string(),
                    })?;
                    match (e.name, e.kind) {
                        ("memory", wasmparser::ExternalKind::Memory) => exports_memory = true,
                        ("run", wasmparser::ExternalKind::Func) => exports_run = true,
                        _ => {}
                    }
                    exports.push((e.name.to_string(), e.kind, e.index));
                }
            }
            Payload::CustomSection(c) if c.name() == SECTION_NAME => {
                // SECURITY: exactly one. Two sections would leave the
                // reader the choice of which claim it proves, while
                // the host could retain the other.
                if manifest.is_some() {
                    return Err(ReadError::DuplicateManifest);
                }
                manifest = Some(decode(c.data()).map_err(ReadError::Manifest)?);
            }
            _ => {}
        }
    }

    if imports.len() > MAX_IMPORTS {
        return Err(ReadError::TooManyImports {
            found: imports.len(),
            max: MAX_IMPORTS,
        });
    }
    for i in &imports {
        // SECURITY: kind first. `lookup` only knows functions;
        // a memory named `x0ne::fs_list` would find an entry there and
        // pass the signature check through the back door.
        if i.kind != ImportKind::Func {
            return Err(ReadError::ImportOutsideAbi {
                module: i.module.clone(),
                name: i.name.clone(),
                kind: i.kind,
            });
        }
        let f = abi::lookup(&i.module, &i.name).ok_or_else(|| ReadError::ImportOutsideAbi {
            module: i.module.clone(),
            name: i.name.clone(),
            kind: i.kind,
        })?;
        // SECURITY: the **whole** signature. Wasmtime will refuse again at link:
        // defence in depth, not substitution. But a refusal at link comes
        // after the human gate, hence after someone has been shown a diff
        // computed on a module that will not instantiate.
        let expected = abi_sig(f);
        let found = i.signature.clone().unwrap_or_else(|| "?".to_string());
        if found != expected {
            return Err(ReadError::ImportSignature {
                name: i.name.clone(),
                found,
                expected,
            });
        }
    }
    if has_start {
        return Err(ReadError::StartSection);
    }
    if !exports_memory {
        return Err(ReadError::MissingExport { name: "memory" });
    }
    if !exports_run {
        return Err(ReadError::MissingExport { name: "run" });
    }
    // SECURITY: and **nothing else**. See `ReadError::UnexpectedExport`.
    for (name, kind, index) in &exports {
        match (name.as_str(), kind) {
            ("memory", wasmparser::ExternalKind::Memory) => {}
            ("run", wasmparser::ExternalKind::Func) => {
                // The function space: imported ones first, defined ones
                // next. This is what gives the type of `run`.
                let ty = match (*index as usize).checked_sub(imported_func_types.len()) {
                    None => imported_func_types.get(*index as usize).copied(),
                    Some(local) => defined_func_types.get(local).copied(),
                };
                let sig = ty
                    .and_then(|t| type_sigs.get(t as usize).cloned())
                    .unwrap_or_else(|| "?".to_string());
                if sig != "()->()" {
                    return Err(ReadError::RunSignature { found: sig });
                }
            }
            _ => {
                return Err(ReadError::UnexpectedExport {
                    name: name.to_string(),
                })
            }
        }
    }
    // SECURITY: bounded on both sides, and to the same pages. See
    // `ReadError::UnboundedMemory`.
    if memory_pages.is_none() || memory_maximum_pages != memory_pages {
        return Err(ReadError::UnboundedMemory {
            pages: memory_pages,
            maximum: memory_maximum_pages,
        });
    }

    let manifest = manifest.ok_or(ReadError::NoManifest)?;

    // SECURITY: invariant I-1, the cross-check. See `ReadError::CapabilityMismatch`.
    let imports_edit = imports.iter().any(|i| i.name == EDIT_IMPORT);
    let manifest_claims_write = manifest.entries.iter().any(|e| e.capability.may_write());
    if imports_edit != manifest_claims_write {
        return Err(ReadError::CapabilityMismatch {
            manifest_claims_write,
            imports_edit,
        });
    }

    Ok(ModuleFacts {
        imports,
        manifest,
        has_start,
        exports_memory,
        exports_run,
        memory_pages,
        memory_maximum_pages,
        imports_edit,
    })
}
