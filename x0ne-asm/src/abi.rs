//! The host ABI, frozen. A single import namespace, four functions, two
//! required exports, `start` section forbidden.
//!
//! ```wat
//! (import "x0ne" "fs_list"          (func (param i32 i32 i32) (result i32)))
//! ;;  manifest_idx, out_ptr, out_cap -> count>=0 | -ERRNO   (ids as LE u32)
//! (import "x0ne" "fs_read"          (func (param i32 i32 i32) (result i32)))
//! ;;  id, out_ptr, out_cap          -> bytes_written | -E_2BIG | -ERRNO
//! (import "x0ne" "fs_edit_buffered" (func (param i32 i32 i32) (result i32)))
//! ;;  id, regex_idx, template_idx   -> n_replacements>=0 | -ERRNO
//! (import "x0ne" "emit"             (func (param i32 i32) (result i32)))
//! ;;  ptr, len                      -> 0 | -ERRNO
//! (export "memory" (memory 0))  ;; required, checked
//! (export "run"    (func))      ;; single entry point
//! ```
//!
//! **No path string ever enters the module — only integers
//! (indices, ids) travel through it.** This is invariant I-A, and it is what makes
//! the data section harmless (I-C): it may contain pseudo-paths,
//! nobody will ever read them as paths.

use crate::limits::MAX_IMPORTS;

/// The only import module. An import coming from elsewhere is refused without
/// examination: there is no second provider.
pub const MODULE_NAMESPACE: &str = "x0ne";

/// The *meaning* of a parameter. All are `i32` in the wasm; this type says what
/// the host does with it, and that distinction is what the I-A test checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamRole {
    /// An index into a host table (manifest, ids, regex, templates).
    Index,
    /// Address of a buffer that the **host fills**.
    OutPtr,
    /// Capacity of that buffer, in bytes.
    OutCap,
    /// Address of bytes that the **host reads** from the module's memory.
    InPtr,
    /// Length of those bytes.
    InLen,
}

/// What a host function grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapKind {
    /// Gives access to a file. Never takes `InPtr`/`InLen` (I-A).
    Capability,
    /// Grants nothing: returns module-controlled bytes to the human,
    /// bounded by `MAX_EMIT_BYTES` and labelled as such when rendered.
    BytesOut,
}

pub struct AbiFunc {
    pub module: &'static str,
    pub name: &'static str,
    pub params: &'static [ParamRole],
    pub returns_i32: bool,
    pub cap: CapKind,
}

impl AbiFunc {
    /// The wasm parameters are all `i32`: the arity is enough to describe the
    /// signature.
    pub fn arity(&self) -> usize {
        self.params.len()
    }
}

/// SECURITY: the table is the contract between the verifier and the host. It is
/// frozen by `the_import_signature_table_is_frozen`.
pub const ABI: &[AbiFunc] = &[
    AbiFunc {
        module: MODULE_NAMESPACE,
        name: "fs_list",
        params: &[ParamRole::Index, ParamRole::OutPtr, ParamRole::OutCap],
        returns_i32: true,
        cap: CapKind::Capability,
    },
    AbiFunc {
        module: MODULE_NAMESPACE,
        name: "fs_read",
        params: &[ParamRole::Index, ParamRole::OutPtr, ParamRole::OutCap],
        returns_i32: true,
        cap: CapKind::Capability,
    },
    AbiFunc {
        module: MODULE_NAMESPACE,
        name: "fs_edit_buffered",
        params: &[ParamRole::Index, ParamRole::Index, ParamRole::Index],
        returns_i32: true,
        cap: CapKind::Capability,
    },
    AbiFunc {
        module: MODULE_NAMESPACE,
        name: "emit",
        params: &[ParamRole::InPtr, ParamRole::InLen],
        returns_i32: true,
        cap: CapKind::BytesOut,
    },
];

pub fn lookup(module: &str, name: &str) -> Option<&'static AbiFunc> {
    ABI.iter().find(|f| f.module == module && f.name == name)
}

/// SECURITY: **a single** definition of the wasm feature set, shared
/// by the validator (here) and by `wasmtime::Config` (T5).
///
/// A module validated under one semantics and executed under another is bug C4
/// of the previous review, one level lower: the verifier would prove
/// properties of a language the engine does not execute. MVP only — asm-ai
/// emits no SIMD, no threads, no GC, no tail-call, and the skeleton module is
/// twenty instructions.
///
/// **What this constant does not hold yet, and what will hold it:** equality
/// with the engine configuration is not testable here, because
/// `x0ne-asm` is pure and does not know wasmtime. The test that compares this
/// value to the one in `wasmtime::Config` arrives with the host, in **T5**. Until then,
/// "a single constant" is held by the fact that there is only one to write,
/// not by a measurement.
pub const WASM_FEATURES: wasmparser::WasmFeatures = wasmparser::WasmFeatures::MVP;

const _: () = assert!(ABI.len() <= MAX_IMPORTS);

#[cfg(test)]
mod tests {
    use super::*;

    /// SECURITY: invariant I-A — **no capability-bearing host function
    /// takes a string**. All its parameters are *indices*.
    ///
    /// This is what makes the module's data section harmless: it may
    /// contain anything, including pseudo-paths (invariant I-C),
    /// because no capability function accepts an input pointer.
    /// The only import that reads module memory is `emit`, and `emit`
    /// grants nothing: it returns bytes to the human, bounded and labelled.
    ///
    /// A failure here — adding an `fs_open(path_ptr, path_len)`, for example —
    /// means the module chooses its own targets, and the proven manifest
    /// stops describing what is going to happen. It is this test, even more
    /// than the "pure module" test, that holds the property.
    #[test]
    fn no_capability_bearing_host_function_takes_a_pointer() {
        for f in ABI {
            if f.cap != CapKind::Capability {
                continue;
            }
            for (i, role) in f.params.iter().enumerate() {
                assert!(
                    !matches!(role, ParamRole::InPtr | ParamRole::InLen),
                    "{}::{} parameter {i} has role {role:?}: a capability function \
                     never reads module memory",
                    f.module,
                    f.name
                );
            }
        }
    }

    /// SECURITY: the whole table, frozen. Adding, removing or re-typing an import
    /// fails the suite — deliberately, because the ABI is the boundary of the
    /// TCB and it must not move inadvertently.
    #[test]
    fn the_import_signature_table_is_frozen() {
        let rendered: Vec<String> = ABI
            .iter()
            .map(|f| {
                format!(
                    "{}::{}({}) -> {} [{:?}]",
                    f.module,
                    f.name,
                    f.params
                        .iter()
                        .map(|p| format!("{p:?}"))
                        .collect::<Vec<_>>()
                        .join(","),
                    if f.returns_i32 { "i32" } else { "()" },
                    f.cap
                )
            })
            .collect();
        assert_eq!(
            rendered,
            vec![
                "x0ne::fs_list(Index,OutPtr,OutCap) -> i32 [Capability]",
                "x0ne::fs_read(Index,OutPtr,OutCap) -> i32 [Capability]",
                "x0ne::fs_edit_buffered(Index,Index,Index) -> i32 [Capability]",
                "x0ne::emit(InPtr,InLen) -> i32 [BytesOut]",
            ],
            "the ABI changed — if intended, update this table AND re-read \
             `no_capability_bearing_host_function_takes_a_pointer`"
        );
        assert!(ABI.len() <= crate::limits::MAX_IMPORTS);
    }
}
