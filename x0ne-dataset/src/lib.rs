// Dataset module — RAG over the typed-action trajectory corpus produced by
// `x0ne-dataset-build`. Two backends:
//
//   • `Backend::TfIdf` — pure-Rust, zero deps, instant. Default. Quality is
//     keyword-overlap (no semantic match between e.g. "build a webapp" and
//     "create a website" without shared tokens). Good enough for ~1k docs of
//     short goals.
//   • `Backend::Ollama` — POST /api/embeddings on the local Ollama with
//     nomic-embed-text (or any embed-capable model). Real semantic similarity.
//     Used when the model is reachable; falls back to TfIdf otherwise.
//
// Files written:
//   ~/.x0ne/dataset/index_meta.json   — backend config + vocab/idf
//   ~/.x0ne/dataset/index.jsonl       — one line per trajectory: {id, goal,
//                                       vec, summary, outcome, score, ...}
//
// Retrieval: load both, embed query through the same backend, compute cosine
// similarity vs every doc, return top-K. Linear scan is fine at this scale
// (~1ms for 500 docs of ~256 dims).

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Backend {
    TfIdf,
    Ollama { model: String, dim: usize },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexMeta {
    pub backend: Backend,
    pub n_docs: usize,
    pub vocab_size: Option<usize>,
    /// Term-id → idf weight (TfIdf only).
    pub idf: Option<Vec<f32>>,
    /// Term-string → term-id (TfIdf only).
    pub vocab: Option<BTreeMap<String, u32>>,
}

/// One line of `index.jsonl`. Vec encoding depends on backend:
///   - TfIdf: sparse `Vec<[term_id, weight]>` → stored as `vec_sparse`
///   - Ollama: dense `Vec<f32>` → stored as `vec_dense`
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DocEntry {
    pub id: String,
    pub goal: String,
    pub summary: String,
    pub outcome: String,
    pub quality_score: u32,
    pub num_actions: usize,
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vec_sparse: Option<Vec<(u32, f32)>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vec_dense: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub id: String,
    pub goal: String,
    pub summary: String,
    pub outcome: String,
    pub similarity: f32,
    pub num_actions: usize,
    pub tools: Vec<String>,
}

// ── SFT system prompts ───────────────────────────────────────────────────────

/// Default SFT system prompt used by `x0ne-dataset-export --format=sft`.
/// Kept here (not in the bin) so it can be iterated without a cargo rebuild
/// — and so RAG / eval / agent-loop callers can reuse the exact same prompt
/// the model was trained on. Override at runtime by writing a custom prompt
/// to `~/.x0ne/prompts/sft_system.txt` (read via [`sft_system_prompt`]).
pub const DEFAULT_SFT_SYSTEM_PROMPT: &str = "You are an X0NE agent. Emit a JSON Pipeline of typed Actions to accomplish the user's goal. Each Action is one object with a `type` field (PascalCase, e.g. \"ReadFile\", \"WriteFile\", \"ArtifactCreate\", \"Pipeline\", \"SshExec\") plus its parameters. Wrap multi-step plans in {\"type\":\"Pipeline\",\"steps\":[...]}. Prefer typed primitives over RunCommand. For one-shot factual answers that mix prose with a deterministic computation, emit a single {\"type\":\"RenderTemplate\",\"template\":\"…{{tool(args)}}…\"} — the runtime substitutes the placeholder by calling the tool, no second LLM turn needed. Example: user asks \"how many r in strawberry?\" → emit {\"type\":\"RenderTemplate\",\"template\":\"There are {{str_count(\\\"strawberry\\\",\\\"r\\\")}} r in strawberry.\"}. Only emit valid JSON — no prose.";

/// Resolve the active SFT system prompt: returns the contents of
/// `~/.x0ne/prompts/sft_system.txt` if it exists and is non-empty, otherwise
/// the compiled-in [`DEFAULT_SFT_SYSTEM_PROMPT`]. Letting users iterate the
/// prompt without rebuilding is critical for the train→eval→tune loop —
/// every prompt tweak would otherwise be a cargo build away.
pub fn sft_system_prompt() -> String {
    let p = x0ne_home().join("prompts/sft_system.txt");
    if let Ok(s) = std::fs::read_to_string(&p) {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    DEFAULT_SFT_SYSTEM_PROMPT.to_string()
}

// ── Misc helpers shared by training-pipeline bins ────────────────────────────

/// snake_case → PascalCase. Empty segments are skipped.
/// Used by `x0ne-export-schema` to map MCP tool names back to Action variants.
pub fn pascalize(snake: &str) -> String {
    snake
        .split('_')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut chars = s.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect()
}

/// Wrap a sequence of action JSON values into a single assistant payload —
/// either the bare action (single-step) or `{"type":"Pipeline","steps":[...]}`.
/// Used by `x0ne-dataset-export` to build the `assistant` ChatML message.
pub fn build_assistant_payload(steps: &[Value]) -> String {
    if steps.len() == 1 {
        return steps[0].to_string();
    }
    serde_json::json!({"type": "Pipeline", "steps": steps}).to_string()
}

/// Lightweight structural validator: action is an object with a string `type`,
/// and when that type matches a strict (non-catchall) anyOf branch, the
/// branch's `required` fields are all present. Used by `x0ne-eval` for
/// reproducible per-row schema scoring without a full JSON Schema lib.
pub fn schema_valid_quick(
    action: &Value,
    schema: &Value,
    known: &std::collections::HashSet<String>,
) -> bool {
    let obj = match action.as_object() {
        Some(o) => o,
        None => return false,
    };
    let t = match obj.get("type").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return false,
    };
    if !known.contains(t) {
        return true;
    }
    if let Some(arr) = schema.get("anyOf").and_then(|v| v.as_array()) {
        for branch in arr {
            let const_t = branch
                .get("properties")
                .and_then(|p| p.get("type"))
                .and_then(|x| x.get("const"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if const_t != t {
                continue;
            }
            let req = branch
                .get("required")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for r in req {
                if let Some(name) = r.as_str() {
                    if !obj.contains_key(name) {
                        return false;
                    }
                }
            }
            return true;
        }
    }
    true
}

/// Extract the first concrete Action from an assistant payload. Strips the
/// wrapping Pipeline if present. Returns None when the payload isn't valid
/// JSON or has no first step. Used by `x0ne-eval` to score the model's
/// "first move" without re-implementing parsing in every bin.
pub fn assistant_first_action(payload: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(payload).ok()?;
    if v.get("type").and_then(|t| t.as_str()) == Some("Pipeline") {
        let steps = v.get("steps").and_then(|s| s.as_array())?;
        return steps.first().cloned();
    }
    Some(v)
}

// ── Tokenizer ────────────────────────────────────────────────────────────────

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "is", "it", "of", "to", "in", "on", "and", "or", "for", "with", "from", "by",
    "at", "as", "be", "are", "was", "were", "this", "that", "these", "those", "do", "does", "did",
    "have", "has", "had", "but", "not", "no", "yes", "i", "you", "we", "they", "he", "she", "his",
    "her", "their", "our", "my", "your", "me", "us", "them", "him", "if", "so", "can", "could",
    "would", "should", "will", "would", "may", "might", "what", "when", "where", "why", "how",
    "who", "le", "la", "les", "un", "une", "des", "de", "du", "et", "ou", "tu", "je", "il", "elle",
    "nous", "vous", "ils", "elles", "ce", "cet", "cette", "ces", "à", "au", "aux", "que", "qui",
    "quoi", "comment", "pour", "par", "dans", "sur", "sous", "avec", "sans", "ne", "pas", "plus",
    "moins", "fait", "faire", "avoir", "être", "sera", "etre",
];

pub fn tokenize(text: &str) -> Vec<String> {
    let stop: std::collections::HashSet<&str> = STOPWORDS.iter().copied().collect();
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2 && !stop.contains(t))
        .map(|t| t.to_string())
        .collect()
}

// ── Trajectory summary ───────────────────────────────────────────────────────

/// Compact human/LLM-readable description of a trajectory: the tool sequence
/// (deduplicated, in order) + outcome. Used as the few-shot the LLM consumes.
pub fn summarize_trajectory(steps: &[Value], outcome: &str) -> String {
    let mut seq: Vec<String> = Vec::new();
    let mut last = String::new();
    for s in steps {
        if let Some(t) = s.get("action_type").and_then(|v| v.as_str()) {
            if t != last {
                seq.push(t.to_string());
                last = t.to_string();
            }
        }
    }
    if seq.is_empty() {
        format!("(empty, outcome={})", outcome)
    } else {
        format!(
            "{} → {} ({} steps, {})",
            seq.first().unwrap(),
            seq.join(" → "),
            steps.len(),
            outcome
        )
    }
}

// ── TfIdf indexer ────────────────────────────────────────────────────────────

pub struct TfIdfBuilder {
    vocab: BTreeMap<String, u32>,
    df: HashMap<u32, u32>,
    docs: Vec<Vec<u32>>, // tokens-as-term-ids, kept for second pass
}

impl Default for TfIdfBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TfIdfBuilder {
    pub fn new() -> Self {
        Self {
            vocab: BTreeMap::new(),
            df: HashMap::new(),
            docs: Vec::new(),
        }
    }

    pub fn add(&mut self, text: &str) {
        let toks = tokenize(text);
        let mut ids: Vec<u32> = Vec::with_capacity(toks.len());
        let mut seen = std::collections::HashSet::new();
        for t in &toks {
            let next_id = self.vocab.len() as u32;
            let id = *self.vocab.entry(t.clone()).or_insert(next_id);
            ids.push(id);
            seen.insert(id);
        }
        for id in seen {
            *self.df.entry(id).or_insert(0) += 1;
        }
        self.docs.push(ids);
    }

    pub fn finish(self) -> (BTreeMap<String, u32>, Vec<f32>, Vec<Vec<(u32, f32)>>) {
        let n = self.docs.len() as f32;
        let vocab_size = self.vocab.len();
        let mut idf: Vec<f32> = vec![0.0; vocab_size];
        for (id, df) in &self.df {
            let v = ((n + 1.0) / (*df as f32 + 1.0)).ln() + 1.0;
            idf[*id as usize] = v;
        }
        let mut doc_vecs: Vec<Vec<(u32, f32)>> = Vec::with_capacity(self.docs.len());
        for tokens in self.docs {
            let mut tf: HashMap<u32, f32> = HashMap::new();
            for t in &tokens {
                *tf.entry(*t).or_insert(0.0) += 1.0;
            }
            let mut v: Vec<(u32, f32)> = tf
                .into_iter()
                .map(|(id, c)| (id, c * idf[id as usize]))
                .collect();
            // L2 normalize so cosine = dot product.
            let norm: f32 = v.iter().map(|(_, w)| w * w).sum::<f32>().sqrt();
            if norm > 0.0 {
                for (_, w) in v.iter_mut() {
                    *w /= norm;
                }
            }
            v.sort_by_key(|(id, _)| *id);
            doc_vecs.push(v);
        }
        (self.vocab, idf, doc_vecs)
    }
}

pub fn embed_tfidf(text: &str, vocab: &BTreeMap<String, u32>, idf: &[f32]) -> Vec<(u32, f32)> {
    let toks = tokenize(text);
    let mut tf: HashMap<u32, f32> = HashMap::new();
    for t in &toks {
        if let Some(id) = vocab.get(t) {
            *tf.entry(*id).or_insert(0.0) += 1.0;
        }
    }
    let mut v: Vec<(u32, f32)> = tf
        .into_iter()
        .map(|(id, c)| (id, c * idf[id as usize]))
        .collect();
    let norm: f32 = v.iter().map(|(_, w)| w * w).sum::<f32>().sqrt();
    if norm > 0.0 {
        for (_, w) in v.iter_mut() {
            *w /= norm;
        }
    }
    v.sort_by_key(|(id, _)| *id);
    v
}

pub fn cosine_sparse(a: &[(u32, f32)], b: &[(u32, f32)]) -> f32 {
    // Both are sorted by id and L2-normalized → dot product = cosine.
    let (mut i, mut j) = (0usize, 0usize);
    let mut dot = 0.0f32;
    while i < a.len() && j < b.len() {
        match a[i].0.cmp(&b[j].0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                dot += a[i].1 * b[j].1;
                i += 1;
                j += 1;
            }
        }
    }
    dot
}

// ── Ollama embedder (best-effort) ────────────────────────────────────────────

pub fn ollama_probe_embed_model() -> Option<String> {
    let url = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".into());
    let resp = ureq_get(&format!("{}/api/tags", url), 2)?;
    let v: Value = serde_json::from_str(&resp).ok()?;
    let models = v.get("models")?.as_array()?;
    for m in models {
        let name = m.get("name").and_then(|x| x.as_str()).unwrap_or("");
        if name.contains("nomic-embed")
            || name.contains("mxbai-embed")
            || name.contains("all-minilm")
        {
            return Some(name.to_string());
        }
    }
    None
}

pub fn ollama_embed(model: &str, text: &str) -> Result<Vec<f32>> {
    let url = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".into());
    let body = serde_json::json!({ "model": model, "prompt": text });
    let resp = ureq_post(&format!("{}/api/embeddings", url), &body.to_string(), 30)
        .ok_or_else(|| anyhow!("ollama embeddings request failed"))?;
    let v: Value = serde_json::from_str(&resp).context("parsing ollama response")?;
    let arr = v
        .get("embedding")
        .and_then(|x| x.as_array())
        .ok_or_else(|| anyhow!("missing embedding field"))?;
    Ok(arr
        .iter()
        .filter_map(|x| x.as_f64().map(|f| f as f32))
        .collect())
}

pub fn cosine_dense(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..n {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let d = (na.sqrt() * nb.sqrt()).max(1e-9);
    dot / d
}

// Tiny blocking HTTP helpers using std::net to avoid pulling reqwest into
// every binary that links this module. Body is JSON.
fn ureq_get(url: &str, timeout_secs: u64) -> Option<String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;
    let (host, path) = parse_url(url)?;
    let mut s = TcpStream::connect_timeout(
        &host
            .parse()
            .ok()
            .or_else(|| std::net::ToSocketAddrs::to_socket_addrs(&host).ok()?.next())?,
        Duration::from_secs(timeout_secs),
    )
    .ok()?;
    s.set_read_timeout(Some(Duration::from_secs(timeout_secs)))
        .ok()?;
    s.set_write_timeout(Some(Duration::from_secs(timeout_secs)))
        .ok()?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path,
        host_no_port(&host)
    );
    s.write_all(req.as_bytes()).ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    let body_idx = buf.find("\r\n\r\n").map(|i| i + 4)?;
    Some(buf[body_idx..].to_string())
}

fn ureq_post(url: &str, body: &str, timeout_secs: u64) -> Option<String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;
    let (host, path) = parse_url(url)?;
    let mut s = TcpStream::connect_timeout(
        &host
            .parse()
            .ok()
            .or_else(|| std::net::ToSocketAddrs::to_socket_addrs(&host).ok()?.next())?,
        Duration::from_secs(timeout_secs),
    )
    .ok()?;
    s.set_read_timeout(Some(Duration::from_secs(timeout_secs)))
        .ok()?;
    s.set_write_timeout(Some(Duration::from_secs(timeout_secs)))
        .ok()?;
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        path,
        host_no_port(&host),
        body.len(),
        body
    );
    s.write_all(req.as_bytes()).ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    let body_idx = buf.find("\r\n\r\n").map(|i| i + 4)?;
    Some(strip_chunked(&buf[body_idx..]))
}

fn parse_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("http://")?;
    let slash = rest.find('/').unwrap_or(rest.len());
    let host = &rest[..slash];
    let path = if slash >= rest.len() {
        "/"
    } else {
        &rest[slash..]
    };
    let host_with_port = if host.contains(':') {
        host.to_string()
    } else {
        format!("{}:80", host)
    };
    Some((host_with_port, path.to_string()))
}

fn host_no_port(h: &str) -> &str {
    h.rsplit_once(':').map(|(a, _)| a).unwrap_or(h)
}

fn strip_chunked(s: &str) -> String {
    // Best-effort dechunking. Ollama responses are typically not chunked at
    // /api/embeddings (they set Content-Length), but be defensive.
    if let Some(first_nl) = s.find("\r\n") {
        if u64::from_str_radix(s[..first_nl].trim(), 16).is_ok() {
            // Looks chunked. Reassemble.
            let mut out = String::new();
            let mut rest = s;
            loop {
                let nl = match rest.find("\r\n") {
                    Some(i) => i,
                    None => break,
                };
                let size = match u64::from_str_radix(rest[..nl].trim(), 16) {
                    Ok(n) => n as usize,
                    Err(_) => {
                        out.push_str(rest);
                        break;
                    }
                };
                if size == 0 {
                    break;
                }
                let start = nl + 2;
                let end = (start + size).min(rest.len());
                out.push_str(&rest[start..end]);
                rest = &rest[(end + 2).min(rest.len())..];
            }
            return out;
        }
    }
    s.to_string()
}

// ── Build & query ────────────────────────────────────────────────────────────

pub fn x0ne_home() -> PathBuf {
    if let Ok(p) = std::env::var("X0NE_HOME") {
        return PathBuf::from(p);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".x0ne")
}

pub fn dataset_dir() -> PathBuf {
    x0ne_home().join("dataset")
}

pub fn build_index(min_quality: u32, force_tfidf: bool) -> Result<(IndexMeta, usize)> {
    let dir = dataset_dir();
    let traj_path = dir.join("trajectories.jsonl");
    let f = File::open(&traj_path).with_context(|| format!("opening {}", traj_path.display()))?;
    let reader = BufReader::new(f);

    #[derive(Deserialize)]
    struct RawTraj {
        session_id: String,
        goal: String,
        outcome: String,
        quality_score: u32,
        trajectory: Vec<Value>,
        stats: RawStats,
    }
    #[derive(Deserialize)]
    struct RawStats {
        num_actions: usize,
        tools_used: Vec<String>,
    }

    let mut keepers: Vec<(String, String, String, String, u32, usize, Vec<String>)> = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) if !l.trim().is_empty() => l,
            _ => continue,
        };
        let t: RawTraj = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if t.quality_score < min_quality {
            continue;
        }
        if t.goal.trim().is_empty() {
            continue;
        }
        let summary = summarize_trajectory(&t.trajectory, &t.outcome);
        keepers.push((
            t.session_id,
            t.goal,
            summary,
            t.outcome,
            t.quality_score,
            t.stats.num_actions,
            t.stats.tools_used,
        ));
    }

    let n = keepers.len();
    if n == 0 {
        return Err(anyhow!(
            "no trajectories meet quality_score >= {} — run `make dataset` first",
            min_quality
        ));
    }

    let model = if force_tfidf {
        None
    } else {
        ollama_probe_embed_model()
    };

    let index_path = dir.join("index.jsonl");
    let meta_path = dir.join("index_meta.json");
    let mut out = File::create(&index_path)?;

    let meta = if let Some(m) = model.clone() {
        eprintln!("[index] backend = ollama ({})", m);
        let mut entries: Vec<DocEntry> = Vec::with_capacity(n);
        let mut dim = 0usize;
        for (i, (id, goal, summary, outcome, q, na, tools)) in keepers.into_iter().enumerate() {
            let v = ollama_embed(&m, &goal)?;
            if i == 0 {
                dim = v.len();
            }
            entries.push(DocEntry {
                id,
                goal,
                summary,
                outcome,
                quality_score: q,
                num_actions: na,
                tools,
                vec_sparse: None,
                vec_dense: Some(v),
            });
            if i % 25 == 0 {
                eprintln!("[index] embedded {}/{}", i + 1, n);
            }
        }
        for e in &entries {
            writeln!(out, "{}", serde_json::to_string(e)?)?;
        }
        IndexMeta {
            backend: Backend::Ollama { model: m, dim },
            n_docs: n,
            vocab_size: None,
            idf: None,
            vocab: None,
        }
    } else {
        eprintln!("[index] backend = tfidf (ollama embed model not found)");
        let mut builder = TfIdfBuilder::new();
        for (_, goal, _, _, _, _, _) in &keepers {
            builder.add(goal);
        }
        let (vocab, idf, doc_vecs) = builder.finish();
        for ((id, goal, summary, outcome, q, na, tools), v) in keepers.into_iter().zip(doc_vecs) {
            let entry = DocEntry {
                id,
                goal,
                summary,
                outcome,
                quality_score: q,
                num_actions: na,
                tools,
                vec_sparse: Some(v),
                vec_dense: None,
            };
            writeln!(out, "{}", serde_json::to_string(&entry)?)?;
        }
        IndexMeta {
            backend: Backend::TfIdf,
            n_docs: n,
            vocab_size: Some(vocab.len()),
            idf: Some(idf),
            vocab: Some(vocab),
        }
    };

    let mut mf = File::create(&meta_path)?;
    writeln!(mf, "{}", serde_json::to_string_pretty(&meta)?)?;
    out.flush()?;
    Ok((meta, n))
}

pub fn load_index() -> Result<(IndexMeta, Vec<DocEntry>)> {
    let dir = dataset_dir();
    let meta: IndexMeta = serde_json::from_str(&fs::read_to_string(dir.join("index_meta.json"))?)?;
    let f = File::open(dir.join("index.jsonl"))?;
    let mut docs: Vec<DocEntry> = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        docs.push(serde_json::from_str(&line)?);
    }
    Ok((meta, docs))
}

pub fn query(intent: &str, k: usize) -> Result<Vec<Hit>> {
    let (meta, docs) = load_index()?;
    let mut scored: Vec<(f32, &DocEntry)> = match &meta.backend {
        Backend::TfIdf => {
            let vocab = meta
                .vocab
                .as_ref()
                .ok_or_else(|| anyhow!("tfidf index missing vocab"))?;
            let idf = meta
                .idf
                .as_ref()
                .ok_or_else(|| anyhow!("tfidf index missing idf"))?;
            let qv = embed_tfidf(intent, vocab, idf);
            docs.iter()
                .filter_map(|d| d.vec_sparse.as_ref().map(|v| (cosine_sparse(&qv, v), d)))
                .collect()
        }
        Backend::Ollama { model, .. } => {
            let qv = ollama_embed(model, intent)?;
            docs.iter()
                .filter_map(|d| d.vec_dense.as_ref().map(|v| (cosine_dense(&qv, v), d)))
                .collect()
        }
    };
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(scored
        .into_iter()
        .take(k)
        .map(|(s, d)| Hit {
            id: d.id.clone(),
            goal: d.goal.clone(),
            summary: d.summary.clone(),
            outcome: d.outcome.clone(),
            similarity: s,
            num_actions: d.num_actions,
            tools: d.tools.clone(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_drops_stopwords_and_short() {
        let toks = tokenize("Make me a fun website on artifacts");
        assert!(toks.contains(&"make".to_string()));
        assert!(toks.contains(&"website".to_string()));
        assert!(toks.contains(&"artifacts".to_string()));
        assert!(!toks.contains(&"a".to_string()));
        assert!(!toks.contains(&"on".to_string()));
        assert!(!toks.contains(&"me".to_string()));
    }

    #[test]
    fn cosine_sparse_identical_returns_one() {
        let a = vec![(1u32, 0.6f32), (2, 0.8)];
        let b = vec![(1u32, 0.6f32), (2, 0.8)];
        let c = cosine_sparse(&a, &b);
        // 0.6*0.6 + 0.8*0.8 = 0.36 + 0.64 = 1.0
        assert!((c - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_sparse_disjoint_returns_zero() {
        let a = vec![(1u32, 1.0f32)];
        let b = vec![(2u32, 1.0f32)];
        assert_eq!(cosine_sparse(&a, &b), 0.0);
    }

    #[test]
    fn tfidf_builder_round_trip() {
        let mut b = TfIdfBuilder::new();
        b.add("build a website with artifacts");
        b.add("make a website fun");
        b.add("ssh into a fleet host");
        let (vocab, idf, vecs) = b.finish();
        assert_eq!(vecs.len(), 3);
        // "website" appears in 2/3 → low idf; "ssh" in 1/3 → high idf
        let website_id = *vocab.get("website").unwrap();
        let ssh_id = *vocab.get("ssh").unwrap();
        assert!(idf[ssh_id as usize] > idf[website_id as usize]);
    }

    #[test]
    fn embed_tfidf_unknown_query_returns_empty() {
        let mut b = TfIdfBuilder::new();
        b.add("hello world");
        let (vocab, idf, _) = b.finish();
        let v = embed_tfidf("nothing matches here", &vocab, &idf);
        assert!(v.is_empty());
    }

    #[test]
    fn summarize_basic() {
        use serde_json::json;
        let steps = vec![
            json!({"action_type": "ListDir"}),
            json!({"action_type": "ReadFile"}),
            json!({"action_type": "ReadFile"}),
            json!({"action_type": "WriteFile"}),
        ];
        let s = summarize_trajectory(&steps, "success");
        assert!(s.contains("ListDir"));
        assert!(s.contains("ReadFile"));
        assert!(s.contains("WriteFile"));
        assert!(s.contains("4 steps"));
        assert!(s.contains("success"));
    }

    #[test]
    fn pascalize_basic_and_edge_cases() {
        assert_eq!(pascalize("read_file"), "ReadFile");
        assert_eq!(pascalize("artifact_create"), "ArtifactCreate");
        assert_eq!(pascalize("ssh_exec"), "SshExec");
        assert_eq!(pascalize("press_key_combination"), "PressKeyCombination");
        // Empty segments (double underscore) are skipped, not panicked.
        assert_eq!(pascalize("foo__bar"), "FooBar");
        assert_eq!(pascalize(""), "");
        // Already-uppercase first letter is preserved (no double-uppercase).
        assert_eq!(pascalize("Foo_bar"), "FooBar");
    }

    #[test]
    fn build_assistant_payload_singleton_unwraps() {
        use serde_json::json;
        let steps = vec![json!({"type":"ListDir","path":"/tmp"})];
        let payload = build_assistant_payload(&steps);
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        // Single-step: bare action, NOT wrapped in Pipeline. Avoids polluting
        // the SFT assistant message with structural noise the model would
        // then learn to over-emit.
        assert_eq!(v.get("type").and_then(|x| x.as_str()), Some("ListDir"));
        assert!(v.get("steps").is_none());
    }

    #[test]
    fn build_assistant_payload_multi_wraps_in_pipeline() {
        use serde_json::json;
        let steps = vec![
            json!({"type":"ReadFile","path":"a"}),
            json!({"type":"WriteFile","path":"b","content":"x"}),
        ];
        let payload = build_assistant_payload(&steps);
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v.get("type").and_then(|x| x.as_str()), Some("Pipeline"));
        assert_eq!(v["steps"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn schema_valid_quick_accepts_known_with_required() {
        use serde_json::json;
        use std::collections::HashSet;
        let schema = json!({
            "anyOf": [
                {
                    "properties": {
                        "type": {"const": "ReadFile"},
                        "path": {"type": "string"}
                    },
                    "required": ["type", "path"]
                },
                {
                    "properties": {"type": {"type": "string"}},
                    "required": ["type"]
                }
            ]
        });
        let known: HashSet<String> = ["ReadFile".to_string()].into_iter().collect();
        assert!(schema_valid_quick(
            &json!({"type":"ReadFile","path":"/tmp/x"}),
            &schema,
            &known
        ));
    }

    #[test]
    fn schema_valid_quick_rejects_known_missing_required() {
        use serde_json::json;
        use std::collections::HashSet;
        let schema = json!({
            "anyOf": [{
                "properties": {"type": {"const": "ReadFile"}},
                "required": ["type", "path"]
            }]
        });
        let known: HashSet<String> = ["ReadFile".to_string()].into_iter().collect();
        // Missing `path` field → invalid even though type matches.
        assert!(!schema_valid_quick(
            &json!({"type":"ReadFile"}),
            &schema,
            &known
        ));
    }

    #[test]
    fn schema_valid_quick_admits_unknown_via_catchall() {
        use serde_json::json;
        use std::collections::HashSet;
        let schema = json!({"anyOf": []});
        let known: HashSet<String> = HashSet::new();
        // Unknown type → falls through to permissive catch-all.
        assert!(schema_valid_quick(
            &json!({"type":"SpawnAgent","name":"x"}),
            &schema,
            &known
        ));
    }

    #[test]
    fn schema_valid_quick_rejects_non_object() {
        use serde_json::json;
        use std::collections::HashSet;
        let schema = json!({"anyOf": []});
        let known: HashSet<String> = HashSet::new();
        assert!(!schema_valid_quick(&json!("bare string"), &schema, &known));
        assert!(!schema_valid_quick(&json!([1, 2, 3]), &schema, &known));
        assert!(!schema_valid_quick(
            &json!({"no_type": true}),
            &schema,
            &known
        ));
    }

    #[test]
    fn assistant_first_action_unwraps_pipeline() {
        let payload = r#"{"type":"Pipeline","steps":[{"type":"ListDir","path":"/"}]}"#;
        let action = assistant_first_action(payload).unwrap();
        assert_eq!(action.get("type").and_then(|v| v.as_str()), Some("ListDir"));
    }

    #[test]
    fn assistant_first_action_returns_bare_when_not_pipeline() {
        let payload = r#"{"type":"ReadFile","path":"/etc/hostname"}"#;
        let action = assistant_first_action(payload).unwrap();
        assert_eq!(
            action.get("type").and_then(|v| v.as_str()),
            Some("ReadFile")
        );
    }

    #[test]
    fn assistant_first_action_returns_none_on_empty_pipeline() {
        let payload = r#"{"type":"Pipeline","steps":[]}"#;
        // Empty Pipeline → no first step → None. The eval bin must report
        // this as a structural failure rather than panicking.
        assert!(assistant_first_action(payload).is_none());
    }

    #[test]
    fn assistant_first_action_returns_none_on_invalid_json() {
        assert!(assistant_first_action("not json").is_none());
        assert!(assistant_first_action("").is_none());
    }
}
