source: x0ne Harness SECURITY.md at option/integration-2026-10-06 (665abf8), reporting address removed 2026-11-02
# Security Policy

## Reporting Vulnerabilities

**Do NOT open public GitHub issues for security vulnerabilities.**

Please report security issues privately, through a **GitHub security advisory** on this repository.

Include:
- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

We will acknowledge your report within 72 hours and provide a timeline for a fix.

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.2.x   | ✅ Current |
| < 0.2   | ❌ No      |

## Security Model

X0NE uses a **deny-by-default** security model. See [docs/security.md](docs/security.md) for the full threat model.

### What X0NE protects against

- ✅ Arbitrary command execution (command allowlist; an EMPTY `allowed_commands` denies every command — see the BREAKING note below)
- ✅ Path traversal (canonicalization + deny patterns)
- ✅ Sensitive file access (`.ssh`, `.gnupg`, `*.key`, `*.pem`, `secrets/`, `.env`) — enforced by `deny_path_patterns`, which is **non-empty in the shipped reference config since 2026-08-15**. It shipped empty before that date, so this line was a claim the installed default did not honour; if you installed earlier, compare your `~/.config/x0ne/x0ne-runtime.toml` against `x0ne-runtime/x0ne-runtime.toml` (`make install` never overwrites your file).
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
| T0.5 | Authorisation bypass | [Unreleased] | `Plan` sysaicall would auto-approve in non-interactive contexts (sub-agents, `x0ne < /dev/null`). Now: input-presence heuristic — real terminal prompts, piped stdin with content is honoured, EOF-without-content rejects with `plan_requires_human`, sub-agent contexts always reject. Every decision is recorded in the session JSONL with `decision_source` (`tty_yes` / `tty_enter_default` / `env_auto_approve` / `rejected_non_interactive` / `rejected_subagent` / `rejected_eof`) for audit. |
| T11 | Resource exhaustion / cost runaway | [Unreleased] | Stuck LLMs could re-emit the same failing action indefinitely (token burn, action quota burn, possible side effects on retried actions). Per-signature circuit breaker now blocks the 5th identical `(action_type, normalized_args, error_code)` failure with a non-retryable `loop_detected` envelope. |

### LLM credential forwarding (sandbox mode)

When `x0ne` runs inside an Incus/Podman sandbox, the in-container runtime can call out to LLM providers (Anthropic, OpenAI-compatible, managed Ollama) via the `LlmCall`, `OllamaApi`, and `OllamaChat` actions. Credentials are handled this way:

- **Allowlisted env-var forwarding** — the Makefile only forwards env vars whose names appear in `X0NE_FORWARD_ENV` (default: `OLLAMA_HOST ANTHROPIC_API_KEY OPENAI_API_KEY X0NE_OPENAI_COMPAT_KEY X0NE_LLM_PROVIDER X0NE_LLM_MODEL`). Anything else stays on the host. Unset vars are silently skipped — no empty `--env` slots, no fingerprint.
- **Secrets stay out of trajectory JSONL** — agents reference keys by env name, not by value: `LlmCall { auth: "$ENV:ANTHROPIC_API_KEY" }`. The runtime resolves the variable at request time; the on-disk session log only stores the `$ENV:…` reference. Literal tokens (`auth: "sk-…"`) are accepted but discouraged.
- **Wire format auto-detected** — Anthropic endpoints get `x-api-key` + `anthropic-version`; everything else gets `Authorization: Bearer …`. No raw token ever lands in URL params, headers logged, or stdout.
- **Loopback Ollama still works keyless** — `auth` is optional. If the resolved token is empty (env var unset), no auth header is sent, so the same JSON works against both local Ollama and managed providers.
- **Threat: a compromised agent inside the sandbox can read forwarded vars.** This is by design — the agent needs the key to call the LLM. To limit blast radius, scope the forwarded keys to per-task project-specific credentials, not your personal master keys.

### HTTP Mode Security

When running in HTTP mode:
- **Bind to `127.0.0.1` by default** — not exposed to the network
- **Optional bearer token authentication** — set `auth_token` in `[runtime.http]`
- **CORS restricted to localhost** — configurable via `allowed_origins`
- The stdio and SSH modes are not affected by HTTP security settings
