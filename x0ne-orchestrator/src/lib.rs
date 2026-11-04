// Public items of a library never raise `dead_code`, so no crate-wide allow is
// needed. Dead public items are found by building orchestrator + x0ne-code as
// one binary with `--force-warn dead_code` (done 2026-09-23).

pub mod acp;
pub mod api;
pub mod artifact_expose;
#[cfg(feature = "serve")]
pub mod artifact_local_proxy;
pub mod bridge;
pub mod canonical_json;
pub mod cc_runner;
pub mod cli_helper;
pub mod cloudmon;
pub mod confidential;
pub mod config;
pub mod connectors;
pub mod container_runtime;
pub mod conversation;
pub mod display;
pub mod display_sink;
pub mod engine_local;
pub mod fleet;
pub mod goal;
pub mod hooks;
pub mod hosts;
pub mod incus_actions;
pub mod native_tools;
pub mod parser;
pub mod playground;
pub mod policy_file;
pub mod project;
pub mod project_trust;
pub mod prompt;
pub mod prose;
pub mod pulse;
pub mod redact;
pub mod result_render;
#[cfg(feature = "serve")]
pub mod serve;
#[cfg(feature = "serve")]
pub mod serve_guard;
pub mod session;
#[cfg(feature = "serve")]
pub mod session_registry;
pub mod session_store;
pub mod setup;
pub mod setup_menu;
pub mod sysaicalls;
pub mod sysmon;
pub mod text_safety;
pub mod worlds_cli;
pub mod worlds_registry;
pub mod worlds_view;
pub mod worlds_workspace;

// MCP server (Phase 2: skeleton)
pub mod mcp_server;

// New modules from main.rs split
pub mod agent;
pub mod agent_loop;
pub mod injection_guard;
pub mod input_queue;
pub mod interrupt;
pub mod intro;
pub mod llm_quickstart;
pub mod llm_setup;
pub mod local_models;
pub mod loop_tracker;
pub mod matrix_mode;
pub mod model_keys;
pub mod sandbox_mode;
pub mod slash;
pub mod sticky_header;
pub mod terminal;
pub mod tui;

/// Source scans shared by the regression guards.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    /// `(path, code)` for every `.rs` file of this crate and of `x0ne-code`,
    /// minus `// …` lines and top-level `#[cfg(test)] mod … { … }` blocks.
    pub(crate) fn production_sources() -> Vec<(String, String)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        for dir in [root.join("src"), root.join("../x0ne-code/src")] {
            collect(&dir, &mut files);
        }
        assert!(files.len() > 50, "the scan found {} files", files.len());
        files
            .into_iter()
            .map(|p| {
                let src = std::fs::read_to_string(&p).unwrap();
                (p.display().to_string(), strip_tests(&src))
            })
            .collect()
    }

    fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                collect(&p, out);
            } else if p.extension().is_some_and(|e| e == "rs") {
                out.push(p);
            }
        }
    }

    fn strip_tests(src: &str) -> String {
        let lines: Vec<&str> = src.lines().collect();
        let is_mod = |l: &str| {
            let l = l
                .trim_start_matches("pub(crate) ")
                .trim_start_matches("pub ");
            l.starts_with("mod ")
        };
        let mut out = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            if lines[i] == "#[cfg(test)]" {
                // Past doc/comment/attribute lines to the gated item.
                let mut j = i + 1;
                while j < lines.len() && (lines[j].starts_with("//") || lines[j].starts_with("#["))
                {
                    j += 1;
                }
                if j < lines.len() && is_mod(lines[j]) {
                    // A `mod x;` ends here; a `mod x {` at its column-0 `}`.
                    if !lines[j].ends_with(';') {
                        while j < lines.len() && lines[j] != "}" {
                            j += 1;
                        }
                    }
                    i = j + 1;
                    continue;
                }
            }
            if !lines[i].trim_start().starts_with("//") {
                out.push(lines[i]);
            }
            i += 1;
        }
        out.join("\n")
    }
}
