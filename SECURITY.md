# Security policy

x0ne Harness is a safety layer between a coding agent and the machine it works
on: typed actions, a policy on every call, snapshots to undo, a journal of the
agent's actions. Reports that show a way for an agent action to reach the host
past the policy, the isolation or the snapshot are the highest-value
contribution this project can receive.

## Reporting

Please report vulnerabilities privately first: open a GitHub security
advisory (Security → Advisories → Report a vulnerability) on this
repository. You should hear back within 72 hours, with a timeline for a
fix.

Please do NOT open public issues for exploitable findings before a fix
ships.

A useful report includes a description of the vulnerability, steps to
reproduce, the potential impact and, if you have one, a suggested fix.

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.2.x   | ✅ Current |
| < 0.2   | ❌ No      |

## Known advisories

`cargo deny check` (RustSec advisories, bans, licenses, sources; `deny.toml`)
runs in CI on every push (`.github/workflows/security.yml`). Last recorded run:
2026-11-01, cargo-deny 0.20.2, RustSec advisory database of 2026-10-30:
advisories ok, bans ok, licenses ok, sources ok, with the entries `deny.toml`
ignores and explains. Open at that date: RUSTSEC-2026-0424 (wasmtime 36.0.19,
component model), not compiled into any binary of this workspace (the feature is
off, and wasmtime itself is behind the optional `asm-exec` feature); the fix is
the 36.0.20 bump.

## Security Model

X0NE uses a **deny-by-default** security model. See [docs/security.md](docs/security.md) for the full threat model.

### What X0NE protects against

- ✅ Arbitrary command execution (command allowlist; an EMPTY `allowed_commands` denies every command — see the BREAKING note below)
- ✅ Path traversal (canonicalization + deny patterns)
- ✅ Sensitive file access (`.ssh`, `.gnupg`, `*.key`, `*.pem`, `secrets/`, `.env`) — enforced by `deny_path_patterns` on every path a read action names and on every file a search walks: `SearchCode` and `SearchAndRead` skip a denied file, prune a denied directory and report the count in `omitted_by_policy`. The default patterns hold even when a config file omits them; one is lifted only by naming it in `deny_path_exceptions`. The list governs the runtime's own reads. A command the agent runs (`cat`, `grep -r`) opens files itself: the command allowlist and the isolation govern it, and the isolation hides the home's credential stores, not a project's `.env`.
- ✅ Uncontrolled container images (image allowlist)
- ✅ Environment variable exfiltration — by `policy.env_passthrough` plus the built-in allowlist in `x0ne-runtime/src/child_env.rs`: a spawned child inherits only the names on that list, everything else is erased before the command starts. Note that `policy.readable_env_vars` is **dead configuration** — the `GetEnv` action it gated no longer exists and `DenyReason::EnvVarNotReadable` is never constructed. Setting it protects nothing; it is kept only so old configs still parse.
- ✅ Desktop automation abuse (rate limiting, opt-in)
- ✅ LLM hallucination loops (retry limits, detection, per-signature `loop_detected` circuit breaker)
- ✅ Plan auto-approval in non-interactive contexts (sub-agents, piped EOF) — explicit env-var gate required

### What X0NE does NOT protect against

- ⚠️ Network egress from allowed commands (e.g. `curl` if whitelisted)
- ⚠️ Data exfiltration via allowed read paths
- ⚠️ Prompt injection from untrusted file content
- ⚠️ Side-channel attacks from process timing

### Recent security fixes

| Tag | Class | Fixed in | Summary |
|---|---|---|---|
| T0.5 | Authorisation bypass | 0.2.0-beta.1 | `Plan` sysaicall would auto-approve in non-interactive contexts (sub-agents, `x0ne < /dev/null`). Now: input-presence heuristic — real terminal prompts, piped stdin with content is honoured, EOF-without-content rejects with `plan_requires_human`, sub-agent contexts always reject. Every decision is recorded in the session JSONL with `decision_source` (`tty_yes` / `tty_enter_default` / `env_auto_approve` / `rejected_non_interactive` / `rejected_subagent` / `rejected_eof`) for audit. |
| T11 | Resource exhaustion / cost runaway | 0.2.0-beta.1 | Stuck LLMs could re-emit the same failing action indefinitely (token burn, action quota burn, possible side effects on retried actions). Per-signature circuit breaker now blocks the 5th identical `(action_type, normalized_args, error_code)` failure with a non-retryable `loop_detected` envelope. |

### LLM credential forwarding (sandbox mode)

When `x0ne` runs inside an Incus/Podman sandbox, the in-container runtime can call out to LLM providers (Anthropic, OpenAI-compatible, managed Ollama) via the `LlmCall`, `OllamaApi`, and `OllamaChat` actions. Credentials are handled this way:

- **Allowlisted env-var forwarding** — the Makefile only forwards env vars whose names appear in `X0NE_FORWARD_ENV` (default: `OLLAMA_HOST ANTHROPIC_API_KEY OPENAI_API_KEY X0NE_OPENAI_COMPAT_KEY X0NE_LLM_PROVIDER X0NE_LLM_MODEL`). Anything else stays on the host. Unset vars are silently skipped — no empty `--env` slots, no fingerprint.
- **Secrets stay out of trajectory JSONL** — agents reference keys by env name, not by value: `LlmCall { auth: "$ENV:ANTHROPIC_API_KEY" }`. The runtime resolves the variable at request time; the on-disk session log only stores the `$ENV:…` reference. Literal tokens (`auth: "sk-…"`) are accepted but discouraged.
- **Wire format auto-detected** — Anthropic endpoints get `x-api-key` + `anthropic-version`; everything else gets `Authorization: Bearer …`. No raw token ever lands in URL params, headers logged, or stdout.
- **Vendor keys stay with their vendor** — before any request, the runtime checks the resolved token against the key table of `x0ne-actions/src/model_keys.rs`, the one the session's own model calls read. `$ENV:ANTHROPIC_API_KEY`, `$ENV:OPENAI_API_KEY` and `$ENV:OLLAMA_API_KEY` reach their vendor's hosts over https only, whatever endpoint the action names; a token that holds `sk-ant-` anywhere reaches api.anthropic.com only. Anything else fails with `auth refused` and nothing is sent. A variable the table does not list is bound to no host: an action that carries any `auth` meets the human gate, and the operator decides.
- **The local runtime starts without vendor keys** — outside a sandbox, the runtime the session spawns (`bridge.mode = "stdio"`) does not inherit `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OLLAMA_API_KEY` or `X0NE_OPENAI_COMPAT_KEY`. An action that needs a key names a variable the operator exported under another name (`$ENV:MY_LLM_KEY`).
- **Loopback Ollama still works keyless** — `auth` is optional. If the resolved token is empty (env var unset), no auth header is sent, so the same JSON works against both local Ollama and managed providers.
- **Threat: a compromised agent inside the sandbox can read forwarded vars.** This is by design — the agent needs the key to call the LLM. To limit blast radius, scope the forwarded keys to per-task project-specific credentials, not your personal master keys.

### HTTP Mode Security

When running in HTTP mode:
- **Bind to `127.0.0.1` by default** — not exposed to the network
- **Bearer token required** — the server refuses to start without one; set
  `auth_token` in `[runtime.http]`, or `x0ne-runtime --mode http` generates
  one into a 0600 file and prints its path on stderr
- **CORS restricted to localhost** — configurable via `allowed_origins`
- The stdio and SSH modes are not affected by HTTP security settings
