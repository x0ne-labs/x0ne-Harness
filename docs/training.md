# Training your own X0NE LLM — the short path

The goal: a model that natively speaks X0NE's typed-action protocol, without a 2000-token system prompt eating its context window. Smaller, faster, locally-deployable, irreplaceable on your domain.

This doc covers the **tooling-driven path** built into the workspace.

---

## The data flywheel

> 📘 **Specific recipes:** the phases below give the concrete axolotl/unsloth configs, GGUF conversion, ollama redeployment, eval comparisons, and troubleshooting.

Every `x0ne` agent session is logged at `~/.x0ne/sessions/<ts>.jsonl` as a typed trajectory: user goal, every typed action emitted by the LLM, every typed result. Six make targets turn those journals into datasets. This project trains no model; the exports are for your own tools.

```
brick 1 ── trajectory extractor   ──►  ~/.x0ne/dataset/trajectories.jsonl
brick 2 ── RAG index + x0ne_recipe ──► ~/.x0ne/dataset/index.jsonl + MCP tool
brick 3 ── action schema + GBNF    ──► ~/.x0ne/dataset/action_schema.json + .gbnf
brick 4 ── SFT/DPO export          ──► sft_v1.jsonl + dpo_v1.jsonl  (LoRA-ready)
brick 5 ── eval harness            ──► reproducible baseline per model
brick 6 ── incremental refresh     ──► one-shot `make dataset-refresh`
```

### Today: zero training, RAG + constrained generation

```bash
make dataset           # extract trajectories.jsonl from ~/.x0ne/sessions/
make dataset-index     # TF-IDF index (auto-upgrades to ollama embeddings if nomic-embed-text is pulled)
make dataset-query Q="make me a fun website"   # smoke retrieval
make export-schema     # action_schema.json + action.gbnf
```

The gateway's **`x0ne_recipe(intent, k)`** tool returns the top-K past Pipelines for an intent, as few-shot examples. `action.gbnf` (`llama-cli --grammar-file`) constrains a local model's output to the action grammar.

### Fine-tuning exports

```bash
make dataset-sft   # ~/.x0ne/dataset/sft_v1.jsonl   — ChatML pairs (system+user+assistant)
make dataset-dpo   # ~/.x0ne/dataset/dpo_v1.jsonl   — auto-mined preference pairs (chosen=success retry, rejected=initial fail)
```

Both files are drop-in for [axolotl](https://github.com/axolotl-ai-cloud/axolotl), [unsloth](https://github.com/unslothai/unsloth), [TRL](https://github.com/huggingface/trl). Recipe sketch:

```yaml
# axolotl config
base_model: Qwen/Qwen2.5-7B-Instruct
chat_template: chatml
datasets:
  - path: ~/.x0ne/dataset/sft_v1.jsonl
    type: chat_template
adapter: lora
lora_r: 16
lora_alpha: 32
lora_target_modules: [q_proj, k_proj, v_proj, o_proj]
```

Then evaluate the result:

```bash
make eval MODEL=ollama/gemma4:31b              # baseline (vanilla)
make eval MODEL=ollama/my-lora-model           # your fine-tuned model, against that baseline
make eval MODEL=... LOSS=1                     # also estimate per-token cost via ollama
```

Reports `schema_valid_rate` (does the model emit JSON that parses against `action_schema.json`?) + `action_known_rate` (does it use real MCP-registry types vs hallucinated names?) + an optional `loss_estimate` per row.

The dataset targets read and write under `~/.x0ne/`.

---

## TL;DR

```bash
make dataset-refresh          # extract + index + sft + dpo + schema (idempotent)
# → ~/.x0ne/dataset/sft_v1.jsonl   ← feed this into axolotl/unsloth
# → ~/.x0ne/dataset/dpo_v1.jsonl   ← feed this into TRL DPOTrainer
# → ~/.x0ne/dataset/action.gbnf    ← use as constrained-decoding grammar
```

The make targets are thin wrappers around a single binary, `x0ne-dataset`, with subcommands `build / index / query / export {sft|dpo} / eval / schema`. Run `x0ne-dataset --help` for the full surface, or use the `make` targets for the well-trodden defaults.


You don't need to fine-tune to get value. **Constrained generation** with `action.gbnf` (zero training) already locks any open-weight LLM into emitting only valid X0NE JSON. Combined with the `x0ne_recipe` MCP tool (RAG over past trajectories), you get most of the win for one afternoon's work.

The full progression below.

---

## What's on disk after `make dataset-refresh`

| File | Purpose | Format |
|---|---|---|
| `~/.x0ne/dataset/trajectories.jsonl` | Raw extraction from sessions, with `quality_score` | one trajectory per line |
| `~/.x0ne/dataset/stats.json` | Aggregate counters (outcome × quality × model × tools) | JSON |
| `~/.x0ne/dataset/index.jsonl` + `index_meta.json` | RAG embedding index (TF-IDF or Ollama) | JSONL + meta |
| `~/.x0ne/dataset/action_schema.json` | JSON Schema (anyOf, 105 strict + catch-all) | JSON Schema 2020-12 |
| `~/.x0ne/dataset/action.gbnf` | llama.cpp grammar — locks `type` field to known names | GBNF |
| `~/.x0ne/dataset/sft_v1.jsonl` | ChatML supervised fine-tuning pairs | OpenAI/ChatML |
| `~/.x0ne/dataset/dpo_v1.jsonl` | Auto-mined preference pairs (chosen=success retry, rejected=initial fail) | TRL/DPO |
| `~/.x0ne/dataset/eval_*.summary.json` | Per-model scoring report | JSON |
| `~/.x0ne/dataset/.last_build_ts` | Marker for incremental rebuilds | epoch seconds |

Quality buckets: `noise` (<30) | `low` (30-59) | `ok` (60-79) | `gold` (80-100). The SFT/DPO export filters on `quality_score >= 60` by default — flag `--min-quality=N` to widen.

---

## Phase 0 — Constrained generation (today, zero training)

The fastest win. Any open-weight model with llama.cpp support emits **only valid X0NE JSON** when sampled with `action.gbnf`:

```bash
make export-schema       # writes ~/.x0ne/dataset/action.gbnf

llama-cli \
  -m ~/.local/models/qwen2.5-7b-instruct-q4_k_m.gguf \
  --grammar-file ~/.x0ne/dataset/action.gbnf \
  -p "Read /etc/hostname"
# Output is *guaranteed* to be a JSON object whose `type` field is one of
# the 105 known PascalCase Action variants. No hallucinated tool names.
```

Pair with the RAG tool for prompt-time recipe lookup:

```bash
make dataset-index        # build the index
make dataset-query Q="check if python is installed"
# Returns top-K past trajectories; paste a successful one as few-shot.
```

The MCP tool `x0ne_recipe(intent, k)` is already exposed via the gateway — Claude Code, Codex, Cursor can call it directly. See [The data flywheel](#the-data-flywheel) for the full flywheel.

For semantic retrieval (handles paraphrases), pull a local embed model:

```bash
ollama pull nomic-embed-text
make dataset-index        # auto-detects, switches from TF-IDF to embeddings
```

---

## Phase 1 — LoRA SFT (3-6 month horizon, ~150-250€ compute)

When the corpus crosses ~1k high-quality trajectories, the first realistic training move. Targets a small open-weight base (1-7B) and adds a low-rank adapter.

### Hardware

- **Minimum**: 1× RTX 4090 (24 GB) for 7B QLoRA
- **Comfortable**: 1× H100 / RTX 5090 (32-80 GB) for full LoRA on 7-13B
- **Cloud**: Lambda, Vast.ai, RunPod — count ~3-8 hours per epoch on the corpus, ~$5-15/hour

### Pick a base

| Base | Size | Why |
|---|---|---|
| **Qwen2.5-7B-Instruct** | 7B | Best instruction-following / tool-use base for this size class. **Recommended default.** |
| Qwen2.5-3B-Instruct | 3B | Fits on smaller GPUs; trades 3-5% accuracy for 2× speed |
| Llama-3.2-3B-Instruct | 3B | Strong baseline, smaller community for tool-use |
| Phi-4-mini | 3.8B | Microsoft's recent small-but-strong; less well-tested with axolotl |
| Gemma2-9B-it | 9B | Bigger, slower, marginally better — overkill for first iteration |

### axolotl config (recommended)

Save as `x0ne-qwen2.5-7b.yml`:

```yaml
base_model: Qwen/Qwen2.5-7B-Instruct
model_type: AutoModelForCausalLM
tokenizer_type: AutoTokenizer

load_in_4bit: true
strict: false

chat_template: chatml
datasets:
  - path: /home/USER/.x0ne/dataset/sft_v1.jsonl
    type: chat_template
    field_messages: messages
    message_field_role: role
    message_field_content: content

dataset_prepared_path: ./prepared/x0ne-qwen
val_set_size: 0.05
output_dir: ./outputs/x0ne-qwen2.5-7b-lora-v1

sequence_len: 4096
sample_packing: true
pad_to_sequence_len: true

adapter: lora
lora_model_dir:
lora_r: 16
lora_alpha: 32
lora_dropout: 0.05
lora_target_modules:
  - q_proj
  - k_proj
  - v_proj
  - o_proj
  - gate_proj
  - up_proj
  - down_proj

gradient_accumulation_steps: 4
micro_batch_size: 2
num_epochs: 3
optimizer: adamw_bnb_8bit
lr_scheduler: cosine
learning_rate: 2.0e-4
warmup_steps: 10

train_on_inputs: false        # only learn the assistant message
group_by_length: false
bf16: auto
tf32: false
gradient_checkpointing: true
flash_attention: true

logging_steps: 1
saves_per_epoch: 1
weight_decay: 0.0
```

Then:

```bash
pip install axolotl[flash-attn,deepspeed]   # one-time
accelerate launch -m axolotl.cli.train x0ne-qwen2.5-7b.yml
```

On a 4090 with the default 161-trajectory corpus (≈ 50k tokens), 3 epochs runs in ~25 minutes. Loss should drop from ~1.4 to ~0.3 over training.

### unsloth alternative (faster on small GPUs)

```python
from unsloth import FastLanguageModel
from datasets import load_dataset
from trl import SFTTrainer

model, tokenizer = FastLanguageModel.from_pretrained(
    model_name="unsloth/Qwen2.5-7B-Instruct-bnb-4bit",
    max_seq_length=4096,
    load_in_4bit=True,
)
model = FastLanguageModel.get_peft_model(
    model, r=16, lora_alpha=32, lora_dropout=0.05,
    target_modules=["q_proj","k_proj","v_proj","o_proj",
                    "gate_proj","up_proj","down_proj"],
)
ds = load_dataset("json", data_files="~/.x0ne/dataset/sft_v1.jsonl")
trainer = SFTTrainer(
    model=model, tokenizer=tokenizer,
    train_dataset=ds["train"],
    dataset_text_field=None,         # ChatML already structured
    formatting_func=lambda x: tokenizer.apply_chat_template(
        x["messages"], tokenize=False),
    max_seq_length=4096, packing=True,
    args=dict(per_device_train_batch_size=2,
              gradient_accumulation_steps=4,
              num_train_epochs=3, learning_rate=2e-4,
              output_dir="./outputs/x0ne-qwen-unsloth"),
)
trainer.train()
model.save_pretrained_merged("./outputs/x0ne-qwen-merged",
                             tokenizer, save_method="merged_16bit")
```

unsloth is typically 2-5× faster than vanilla axolotl on consumer GPUs and handles the LoRA→merged step in one call.

### Convert to GGUF + push to ollama

```bash
git clone https://github.com/ggerganov/llama.cpp /tmp/llama.cpp
cd /tmp/llama.cpp && pip install -r requirements/requirements-convert_hf_to_gguf.txt

python convert_hf_to_gguf.py \
  /path/to/outputs/x0ne-qwen-merged \
  --outfile /tmp/x0ne-qwen.gguf \
  --outtype q4_k_m

cat > /tmp/Modelfile <<'EOF'
FROM /tmp/x0ne-qwen.gguf
TEMPLATE """{{ if .System }}<|im_start|>system
{{ .System }}<|im_end|>
{{ end }}{{ if .Prompt }}<|im_start|>user
{{ .Prompt }}<|im_end|>
{{ end }}<|im_start|>assistant
{{ .Response }}<|im_end|>
"""
SYSTEM "You are an X0NE agent. Emit a JSON Pipeline of typed Actions to accomplish the user's goal. Each Action is one object with a `type` field (PascalCase) plus its parameters."
PARAMETER temperature 0.2
PARAMETER stop "<|im_end|>"
EOF

ollama create x0ne-qwen2.5-7b-lora-v1 -f /tmp/Modelfile
```

Now your model is queryable as `ollama/x0ne-qwen2.5-7b-lora-v1` — point X0NE at it via `~/.x0ne/config.toml`:

```toml
[provider]
default = "ollama/x0ne-qwen2.5-7b-lora-v1"
```

### Eval the result

```bash
make eval MODEL=ollama/gemma4:31b              # baseline (vanilla)
make eval MODEL=ollama/x0ne-qwen2.5-7b-lora-v1 # post-LoRA
```

Compare `~/.x0ne/dataset/eval_*.summary.json`:

| Metric | Baseline target | Post-LoRA target |
|---|---|---|
| `schema_valid_rate` | ~85-95% | **>= 99%** |
| `action_known_rate` | ~70-80% | **>= 95%** |
| `mean_loss` (with `LOSS=1`) | reference | **lower** |

If post-LoRA isn't ≥98% schema-valid, something went wrong (under-trained, bad tokenizer alignment, or chat-template mismatch). Most common fix: increase `num_epochs` to 5 and re-run.

---

## Phase 2 — DPO (after SFT, sharpens recovery behavior)

DPO teaches the model the *difference* between a failed attempt and the correction. Auto-mined from same-session retries, so no human labeling.

```python
from trl import DPOTrainer, DPOConfig
from peft import PeftModel
from transformers import AutoModelForCausalLM, AutoTokenizer
from datasets import load_dataset

base = AutoModelForCausalLM.from_pretrained("./outputs/x0ne-qwen-merged",
                                            torch_dtype="auto", load_in_4bit=True)
tok = AutoTokenizer.from_pretrained("./outputs/x0ne-qwen-merged")

ds = load_dataset("json", data_files="~/.x0ne/dataset/dpo_v1.jsonl",
                  split="train")
# DPOTrainer expects {prompt, chosen, rejected} — sft_v1 already matches.

trainer = DPOTrainer(
    model=base, ref_model=None, tokenizer=tok,
    train_dataset=ds, beta=0.1,
    args=DPOConfig(
        output_dir="./outputs/x0ne-qwen-dpo",
        per_device_train_batch_size=1, gradient_accumulation_steps=8,
        num_train_epochs=1, learning_rate=5e-6,
        max_prompt_length=2048, max_length=4096,
    ),
)
trainer.train()
trainer.save_model()
```

Re-evaluate. DPO typically lifts `action_known_rate` by 2-5pp and reduces "first-shot wrong then retry" patterns at runtime.

---

## Phase 3 — Continued pretraining (12-month horizon)

Once the corpus has 5-10k+ high-quality trajectories AND you want a model that **thinks** in typed actions (not just imitates them), continued-pretraining a 3B base on:

- All raw `~/.x0ne/sessions/*.jsonl` (the pre-extraction signal — formatting, retry patterns, error messages)
- The full X0NE codebase (the runtime knows itself)
- Generated synthetic trajectories via `x0ne_simulate`
- `action_schema.json` and `action.gbnf` repeated as auxiliary objectives

Yields a vertical-specialized model. ~$2-5k compute on a multi-GPU rig. Worth it once SFT diminishing returns kick in (≈ epoch 3-5 not improving eval anymore).

For now, scope this as: keep using X0NE normally, let `make dataset-refresh` accumulate, revisit when the corpus crosses 5k gold trajectories.

---

## Privacy & data hygiene

- Everything stays in `~/.x0ne/` on your machine. No telemetry, no cloud upload.
- Trajectories may include file paths, hostnames, and command outputs from your sessions. **Inspect `sft_v1.jsonl` before sharing or using cloud GPUs**.
- To redact before training, post-process `sft_v1.jsonl`:

```bash
# Strip any /home/USER paths
sed -i "s|/home/$(whoami)|/home/REDACTED|g" ~/.x0ne/dataset/sft_v1.jsonl
```

- Sessions can be excluded from training by deleting the corresponding file in `~/.x0ne/sessions/` before running `make dataset-refresh`.

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `[sft] wrote ... 0 kept of N total` | All trajectories below quality threshold | `--min-quality=30` or run x0ne more |
| Training loss stuck near initial value | Wrong chat template or `train_on_inputs: true` | Verify config; ChatML is `chatml`, not `chat` |
| Eval shows `schema_valid_rate` *lower* than baseline | Likely catastrophic forgetting from too-high LR | Drop to `1e-4`, fewer epochs |
| `RuntimeError: CUDA out of memory` | Sequence length too long | Drop `sequence_len` to 2048 or enable `gradient_checkpointing` |
| Ollama runs the model but it ignores X0NE format | Modelfile SYSTEM mismatch with training system prompt | Copy SYSTEM line verbatim from `dataset_export.rs` |
| `x0ne_recipe` returns 0 hits with sim=0 on every query | TF-IDF backend with no shared keywords | Pull `nomic-embed-text` and re-run `make dataset-index` |
| `cargo build` errors after pulling new commits | Stale build artifacts post-refactor | `cargo clean -p x0ne-runtime && cargo build --release` |

---

## Reference: the bricks

| Brick | Make target | Output |
|---|---|---|
| 1. Trajectory extractor | `make dataset` | `trajectories.jsonl` + `stats.json` |
| 2. RAG indexer | `make dataset-index` | `index.jsonl` + `index_meta.json` |
| 2.5 RAG query | `make dataset-query Q="..."` | top-K hits with similarity |
| 3. Action schema export | `make export-schema` | `action_schema.json` + `action.gbnf` |
| 4a. SFT export | `make dataset-sft` | `sft_v1.jsonl` (ChatML) |
| 4b. DPO export | `make dataset-dpo` | `dpo_v1.jsonl` (preference pairs) |
| 5. Eval harness | `make eval MODEL=...` | `eval_*.summary.json` |
| 6. One-shot refresh | `make dataset-refresh` | all of the above, incremental |

All bricks share one source of truth: the `x0ne-dataset` crate (`pascalize`, `build_assistant_payload`, `schema_valid_quick`, `assistant_first_action`, plus the TF-IDF/Ollama embedders). 17 unit tests cover the helpers.

---

## See also

- [The data flywheel](#the-data-flywheel) above — the diagram and the strategic roadmap
- [`docs/architecture.md`](architecture.md) — how X0NE's typed-action system is designed (the *why* behind the schema)
- [axolotl docs](https://axolotl-ai-cloud.github.io/axolotl/) · [unsloth](https://github.com/unslothai/unsloth) · [TRL](https://huggingface.co/docs/trl/) · [llama.cpp grammars](https://github.com/ggerganov/llama.cpp/blob/master/grammars/README.md)
