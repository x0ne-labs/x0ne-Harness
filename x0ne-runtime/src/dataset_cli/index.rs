// `x0ne-dataset index` — build the RAG index over the trajectory corpus
// produced by `x0ne-dataset build`. Defaults to Ollama embeddings when a
// compatible model is found (`nomic-embed-text`, `mxbai-embed`, `all-minilm`)
// — otherwise falls back to pure-Rust TF-IDF. `--tfidf` forces the fallback.

use anyhow::Result;
use x0ne_dataset::{build_index, dataset_dir, Backend};

pub fn run(force_tfidf: bool, min_quality: u32) -> Result<()> {
    let (meta, n) = build_index(min_quality, force_tfidf)?;
    eprintln!("\n[index] === SUMMARY ===");
    match &meta.backend {
        Backend::TfIdf => {
            eprintln!(
                "backend: TF-IDF (zero deps, keyword overlap)\nvocab size: {}",
                meta.vocab_size.unwrap_or(0)
            );
        }
        Backend::Ollama { model, dim } => {
            eprintln!("backend: Ollama ({})\nembedding dim: {}", model, dim);
        }
    }
    eprintln!("indexed: {} trajectories (quality >= {})", n, min_quality);
    eprintln!(
        "files: {}/index_meta.json + {}/index.jsonl",
        dataset_dir().display(),
        dataset_dir().display()
    );
    Ok(())
}
