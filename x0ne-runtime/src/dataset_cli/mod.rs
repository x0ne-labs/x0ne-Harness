// dataset_cli — six subcommand handlers behind one binary (`x0ne-dataset`).
//
// Each module mirrors what used to be a standalone bin in `src/bin/`. The
// migration was lossless: the old `x0ne-dataset-build`, `x0ne-dataset-index`,
// `x0ne-dataset-query`, `x0ne-dataset-export`, `x0ne-eval`, and
// `x0ne-export-schema` binaries were replaced by a single `x0ne-dataset` CLI
// dispatched via clap. Build time is now 1 link step instead of 6 — see
// `x0ne-runtime/src/bin/dataset.rs` for the entry point.

pub mod build;
pub mod eval;
pub mod export;
pub mod index;
pub mod query;
pub mod schema;
