//! asm-ai — the language the agent emits, and nothing else.
//!
//! The crate is **pure**: no `std::fs`, no policy, no wasmtime. It parses a
//! program into a typed IR (`parse`, `ir`), emits the core WASM module with its
//! `x0ne-manifest` section (`emit`), and reads the capability claims back from
//! the bytes (`read`). It does not know whether a path exists nor whether the
//! policy allows it — those two questions belong to `x0ne-runtime`
//! (`asm_verify`), which asks them of the `PolicyEngine` on concrete paths.
//! Nothing here executes a module: the host (plan T5) does not exist yet.
//!
//! Why this boundary: the compiler is **outside the TCB**. The manifest
//! it produces is a *claim*, not a proof. The chain holds because
//! a lying manifest can only claim patterns, and those
//! patterns all go through `evaluate`.

pub mod abi;
pub mod diag;
pub mod emit;
pub mod ir;
pub mod limits;
pub mod manifest;
pub mod parse;
pub mod read;

pub use parse::parse;

#[cfg(test)]
mod purity {
    /// The `std` modules a pure crate does not touch.
    const FORBIDDEN_MODULES: [&str; 4] = ["fs", "net", "process", "env"];

    /// The macros that read the disk **at compile time**. They are outside the
    /// "`std::` something" model: a scan that only looks for module
    /// paths does not see them, and the file still ends up in the
    /// binary.
    const FORBIDDEN_MACROS: [&str; 2] = ["include_str!", "include_bytes!"];

    /// Returns the first forbidden use found in production **code**.
    ///
    /// Four filters or normalizations, and each one cost a round-trip —
    /// the count is what says how fragile this scan is by nature:
    ///
    /// 1. **comment lines** — this file's doc-comment *states*
    ///    the prohibition ("no `std::fs`") and was being refused by it. The
    ///    filter applies only to lines that *start* with a marker:
    ///    a code line with a trailing comment is still scanned;
    /// 2. the **test module** — tests are allowed to read the disk
    ///    (this one does). The cut is on `#[cfg(test)]\nmod `, the shape of the
    ///    module, and not on the bare attribute: cutting on the attribute let
    ///    a whole file through as soon as a `#[cfg(test)] fn` sat near the top;
    /// 3. the **grouped** form — `use std::{fs, process};` does not contain the
    ///    substring "std::fs". The group is tokenized, multi-line included;
    /// 4. **whitespace around `::`** — `use std :: fs;` compiles, and so does
    ///    `std\n    ::process`. Measured: an `fs :: read_to_string` in
    ///    production left the test at 18/18 green. They are normalized before
    ///    analysis.
    ///
    /// SECURITY: this test is the only guard of the TCB boundary on the crate side,
    /// and it works on **text**, not on an AST — it catches what a
    /// human would write, not what an adversary would disguise (an alias `use
    /// std::fs as truc;` ahead of a `truc::read`, a macro that rebuilds
    /// the path). It is not a sieve to be bypassed by accident, it is a
    /// net against ordinary drift. The positive checks further down
    /// exist so that an overly broad filter cannot empty it silently.
    fn forbidden_use(src: &str) -> Option<&'static str> {
        // The repository convention is a single test module, at the end of the
        // file. The scan checks it rather than assuming it: two modules
        // would make the production code between them invisible.
        assert!(
            src.matches("#[cfg(test)]\nmod ").count() <= 1,
            "one test module per file — otherwise the purity scan leaves the \
             code between two modules in the dark"
        );
        let production = src.split("#[cfg(test)]\nmod ").next().unwrap_or("");
        // Code lines are joined back: a `use std::{\n fs,\n};` spans
        // several lines, and a line-by-line scan would never see the group.
        let code: Vec<&str> = production
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with('*')
            })
            .collect();
        let code = squeeze_path_separators(&code.join("\n"));

        if let Some(m) = FORBIDDEN_MACROS.into_iter().find(|m| code.contains(m)) {
            return Some(m);
        }

        let mut rest = code.as_str();
        while let Some(i) = rest.find("std::") {
            rest = &rest[i + "std::".len()..];
            for name in imported_names(rest) {
                if let Some(f) = FORBIDDEN_MODULES.into_iter().find(|f| *f == name) {
                    // The returned name is the full path: it is what the
                    // panic message shows, and "fs" alone says nothing.
                    return Some(match f {
                        "fs" => "std::fs",
                        "net" => "std::net",
                        "process" => "std::process",
                        _ => "std::env",
                    });
                }
            }
        }
        None
    }

    /// Glues the `::`: `std :: fs` and `std\n    ::process` become
    /// `std::fs` and `std::process`. Rust accepts whitespace around the
    /// path separator; the scan looks for a substring.
    fn squeeze_path_separators(code: &str) -> String {
        let mut out = String::with_capacity(code.len());
        let mut rest = code;
        while let Some(i) = rest.find("::") {
            out.push_str(rest[..i].trim_end());
            out.push_str("::");
            rest = rest[i + 2..].trim_start();
        }
        out.push_str(rest);
        out
    }

    /// What is named right after a `std::`: either an identifier, or the
    /// first identifiers of a group `{a, b::c}`.
    fn imported_names(after: &str) -> Vec<&str> {
        fn ident(s: &str) -> &str {
            let s = s.trim_start();
            let end = s
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(s.len());
            &s[..end]
        }
        match after.trim_start().strip_prefix('{') {
            Some(group) => {
                let end = group.find('}').unwrap_or(group.len());
                group[..end].split(',').map(ident).collect()
            }
            None => vec![ident(after)],
        }
    }

    const FORBIDDEN_DEPS: [&str; 3] = ["wasmtime", "x0ne-runtime", "x0ne_runtime"];

    /// Returns the first forbidden dependency declared in a **real**
    /// dependency section of the manifest.
    ///
    /// Does not sweep the whole manifest: `description` says precisely "no
    /// wasmtime", and a literal reading refused itself on its own promise.
    /// A section counts when its header names `dependencies` without being
    /// `dev-dependencies` — which catches `[dependencies]` as well as
    /// `[build-dependencies]`, `[target.'cfg(unix)'.dependencies]` and the
    /// table form `[dependencies.wasmtime]`, whose header is itself scanned.
    fn forbidden_dependency(manifest: &str) -> Option<&'static str> {
        let mut in_deps = false;
        for line in manifest.lines() {
            let t = line.trim();
            // A **comment** line is not a declaration. Measured on
            // 2026-08-15 (T3): the comment explaining why
            // `wasm-encoder` is pinned to the version wasmtime pulls
            // names wasmtime, and made the scan fail on its own
            // justification — the same mistake as `description` in `[package]`,
            // one level lower. The filter applies only to lines that
            // *start* with `#`: `wasmtime = "36" # pourquoi` is still seen.
            if t.starts_with('#') {
                continue;
            }
            if t.starts_with('[') {
                in_deps = t.contains("dependencies") && !t.contains("dev-dependencies");
            }
            if in_deps {
                if let Some(f) = FORBIDDEN_DEPS.into_iter().find(|f| t.contains(f)) {
                    return Some(f);
                }
            }
        }
        None
    }

    /// The positive check of the manifest scanner. It states the only thing that
    /// matters: prose passes, the dependency does not — under the four
    /// forms a dependency section can take.
    #[test]
    fn the_manifest_scanner_separates_prose_from_dependencies() {
        let prose = "[package]\ndescription = \"Pure: no wasmtime, no x0ne-runtime.\"\n\
                     [dependencies]\nserde = \"1\"\n[dev-dependencies]\nwat = \"1\"\n";
        assert_eq!(forbidden_dependency(prose), None);
        assert_eq!(
            forbidden_dependency("[dependencies]\nwasmtime = \"=36.0.13\"\n"),
            Some("wasmtime")
        );
        assert_eq!(
            forbidden_dependency("[dependencies.wasmtime]\nversion = \"36\"\n"),
            Some("wasmtime")
        );
        assert_eq!(
            forbidden_dependency("[build-dependencies]\nx0ne-runtime = { path = \"..\" }\n"),
            Some("x0ne-runtime")
        );
        assert_eq!(
            forbidden_dependency("[target.'cfg(unix)'.dependencies]\nx0ne_runtime = \"0\"\n"),
            Some("x0ne_runtime")
        );

        // A comment that *names* wasmtime to say why another
        // crate is pinned is not a dependency. Measured on 2026-08-15:
        // this is what made `the_crate_stays_pure` fail in T3.
        assert_eq!(
            forbidden_dependency(
                "[dependencies]\n# pinned to what wasmtime 36.0.13 already pulls\n\
                 wasm-encoder = \"=0.236.0\"\n"
            ),
            None
        );
        // But a **trailing** comment hides nothing: the line
        // remains a declaration, and it is still scanned.
        assert_eq!(
            forbidden_dependency("[dependencies]\nwasmtime = \"=36.0.13\" # why not\n"),
            Some("wasmtime")
        );
    }

    /// The positive check of the scanner. Without it, `the_crate_stays_pure` can
    /// no longer fail the day an overly broad filter swallows the whole file — and
    /// a test that cannot fail is the failure mode this repository has already
    /// met four times under the name "dead scaffolding".
    ///
    /// The last two cases are the evasions **measured** by the review of
    /// 2026-08-15: the original literal scan returned both of them green.
    #[test]
    fn the_purity_scanner_still_sees_a_real_use() {
        let seen = |src: &str| forbidden_use(src).map(|f| f.to_string());
        let seen = |src: &str| seen(src).unwrap_or_default();

        assert_eq!(
            seen("//! neither `std::fs`, nor policy\npub fn f() {}\n"),
            ""
        );
        assert_eq!(
            seen("use std::fs::read_to_string;\npub fn f() {}\n"),
            "std::fs"
        );
        assert_eq!(
            seen("pub fn f() {}\n#[cfg(test)]\nmod t {\n use std::fs;\n}\n"),
            "",
            "the test module may read the disk"
        );
        assert_eq!(seen("    let k = std::env::var(\"X\");\n"), "std::env");

        // Evasion 1 (measured): the grouped form does not contain "std::fs".
        assert_eq!(
            seen("use std::{fs, process};\npub fn f() { fs::metadata(\"x\"); }\n"),
            "std::fs"
        );
        assert_eq!(
            seen("use std::{\n    collections::HashMap,\n    process::Command,\n};\n"),
            "std::process",
            "a multi-line group too"
        );
        assert_eq!(
            seen("use std::{collections::HashMap, fmt::Debug};\n"),
            "",
            "a group of allowed modules stays allowed"
        );

        // Evasion 2 (measured): a `#[cfg(test)] fn` at the top of the file cut
        // the whole file with the old split on the bare attribute.
        assert_eq!(
            seen("#[cfg(test)]\nfn only_in_tests() {}\nuse std::fs::read_to_string;\n"),
            "std::fs"
        );

        // Evasion 3 (measured by the re-review): `use std :: fs;` compiles, and the
        // scan only looked for the glued substring. The code below read
        // /etc/hostname in production, 18/18 green.
        assert_eq!(
            seen("use std :: fs;\nfn f() { fs :: read_to_string(\"/etc/hostname\"); }\n"),
            "std::fs",
            "whitespace around `::` hides nothing"
        );
        assert_eq!(
            seen("use std\n    ::process::Command;\n"),
            "std::process",
            "nor does a `::` carried to the next line"
        );

        // `include_str!` reads the disk **at compile time**: outside the
        // "std:: something" model, hence invisible to a scan that only looks
        // for module paths.
        assert_eq!(
            seen("const D: &str = include_str!(\"/etc/hostname\");\n"),
            "include_str!",
            "a disk read at compile time is still a disk read"
        );
        assert_eq!(
            seen("const D: &[u8] = include_bytes!(\"/etc/hostname\");\n"),
            "include_bytes!"
        );
    }

    /// Purity is not an intention, it is a verifiable property.
    /// A crate that "was pure" and one day opens a file silently moves
    /// the TCB boundary: the verifier would start
    /// re-reading the disk between the proof and the execution, which invariant I-B
    /// of the spec forbids ("verifier and host read the SAME manifest,
    /// extracted from the SAME bytes").
    ///
    /// SECURITY: the test reads **two** things — the code and the `Cargo.toml`.
    /// Forbidding `std::fs` in the sources says nothing about a crate that
    /// would gain `wasmtime` or `x0ne-runtime` as a dependency: purity is lost
    /// through a `use` as much as through a manifest line, and the second
    /// form is the one a code review sees least. `x0ne-runtime` in
    /// particular would bring the policy, so the question "is this path
    /// allowed?" would land in the crate whose role is precisely not
    /// to know how to answer it.
    #[test]
    fn the_crate_stays_pure() {
        // The walk is **recursive**. A `src/emit/mod.rs` added by T3 would be
        // invisible to a flat `read_dir`, and `seen` would keep counting
        // three files with nothing signalling the blind spot.
        fn walk(dir: &std::path::Path, seen: &mut usize) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, seen);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                *seen += 1;
                let src = std::fs::read_to_string(&path).unwrap();
                if let Some(forbidden) = forbidden_use(&src) {
                    panic!(
                        "{} uses {forbidden} — x0ne-asm must stay pure",
                        path.display()
                    );
                }
            }
        }

        let mut seen = 0usize;
        walk(
            std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")),
            &mut seen,
        );
        // A `read_dir` that returns nothing would pass the test without checking anything:
        // it is the most common form of a test that cannot fail.
        assert!(seen >= 4, "expected at least lib/ir/parse/diag, saw {seen}");

        // And the manifest. `[dev-dependencies]` is allowed to name an engine
        // (corpus C will need `wat`); the real dependency sections
        // are not.
        let manifest =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
        if let Some(forbidden) = forbidden_dependency(&manifest) {
            panic!(
                "x0ne-asm gained a dependency on {forbidden}: the crate is no longer \
                 pure, and the verifier could no longer be tested without an engine or \
                 a policy"
            );
        }
    }
}
