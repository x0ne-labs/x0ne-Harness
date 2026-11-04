// `x0ne-dataset` — unified CLI for the X0NE training-data pipeline.
//
// Replaces the six prior bins (build / index / query / export / eval /
// schema) with a single binary dispatching via clap subcommands. Cuts the
// release build from six link steps to one and gives the user a coherent
// `x0ne-dataset --help` surface.

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "x0ne-dataset",
    version,
    about = "X0NE trajectory dataset toolkit — build / index / query / export / eval / schema",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Extract trajectories from ~/.x0ne/sessions/ into trajectories.jsonl (incremental by default)
    Build {
        /// Force a full rebuild (ignore the .last_build_ts marker)
        #[arg(long)]
        full: bool,
    },
    /// Build the RAG embedding index over high-quality trajectories
    Index {
        /// Force the TF-IDF backend even if an ollama embed model is available
        #[arg(long)]
        tfidf: bool,
        /// Minimum quality_score to include (default 60 = ok+gold buckets)
        #[arg(long, default_value_t = 60)]
        min_quality: u32,
    },
    /// Retrieve the top-K most-similar past trajectories for an intent
    Query {
        /// Intent (free-form text — all positional args are joined)
        intent: Vec<String>,
        /// Number of recipes to return
        #[arg(long, default_value_t = 5)]
        k: usize,
        /// Emit JSON instead of human-readable summary
        #[arg(long)]
        json: bool,
    },
    /// Export trajectories to training-ready ChatML or DPO format
    Export {
        #[command(subcommand)]
        fmt: ExportFmt,
    },
    /// Score a model's structural fidelity on a held-out set
    Eval {
        /// Model name (matches what x0ne-orchestrator routes via its bridge)
        #[arg(long, default_value = "ollama/gemma4:31b")]
        model: String,
        /// Number of trajectories to hold out (always the most-recent N)
        #[arg(long, default_value_t = 20)]
        holdout: usize,
        /// Also estimate per-token loss via ollama /api/generate
        #[arg(long)]
        estimate_loss: bool,
    },
    /// Emit ~/.x0ne/dataset/action_schema.json + action.gbnf from the MCP tool registry
    Schema,
}

#[derive(Subcommand)]
enum ExportFmt {
    /// ChatML supervised fine-tuning pairs (axolotl/unsloth/TRL)
    Sft {
        #[arg(long, default_value_t = 60)]
        min_quality: u32,
        /// Keep only outcome==success trajectories
        #[arg(long)]
        only_success: bool,
    },
    /// Auto-mined DPO preference pairs (chosen=success retry, rejected=initial fail)
    Dpo {
        #[arg(long, default_value_t = 60)]
        min_quality: u32,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Build { full } => x0ne_runtime::dataset_cli::build::run(full),
        Cmd::Index { tfidf, min_quality } => {
            x0ne_runtime::dataset_cli::index::run(tfidf, min_quality)
        }
        Cmd::Query { intent, k, json } => {
            let intent_str = intent.join(" ");
            if intent_str.trim().is_empty() {
                anyhow::bail!(
                    "`x0ne-dataset query` requires an intent (e.g. \"make me a fun website\")"
                );
            }
            x0ne_runtime::dataset_cli::query::run(&intent_str, k, json)
        }
        Cmd::Export { fmt } => match fmt {
            ExportFmt::Sft {
                min_quality,
                only_success,
            } => x0ne_runtime::dataset_cli::export::run_sft(min_quality, only_success),
            ExportFmt::Dpo { min_quality } => {
                x0ne_runtime::dataset_cli::export::run_dpo(min_quality)
            }
        },
        Cmd::Eval {
            model,
            holdout,
            estimate_loss,
        } => x0ne_runtime::dataset_cli::eval::run(model, holdout, estimate_loss),
        Cmd::Schema => x0ne_runtime::dataset_cli::schema::run(),
    }
}
