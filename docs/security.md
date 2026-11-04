# Security Model

This document describes X0NE's threat model, how the policy engine works, and the design decisions behind each security boundary.

---

## Threat model

X0NE is designed for a specific use case: **a local AI agent with bounded, auditable access to your machine**. The threat model is:

### What X0NE protects against

| Threat | Mitigation |
|---|---|
| Shell injection | `Command::new()` — no shell, no metacharacter expansion |
| Path traversal (`../../etc/passwd`) | All paths canonicalized before policy check |
| Accidental deletion | `MoveToTrash`, snapshots, `Confirm` gates |
| Sensitive file access (SSH keys, GPG) | Hardcoded deny patterns (file actions); hidden from agent commands by bubblewrap (see *Command isolation*) |
| Agent command escaping the workspace | bubblewrap by default: read-only root, write only in `allowed_write_paths`, no network |
| Desktop runaway input | Rate limiter (100ms between actions) |
| Desktop input reaching another app | `desktop.allowed_apps`: every input gesture re-checks the focused window, or the windows under the pointer, against the list; refused by default when the identity is unknown (see [desktop.md](desktop.md#allowed-apps-whitelist)) |
| Desktop reached around the list | Under `allowed_apps`, agent commands run only under bubblewrap, which hides the Wayland, X11, sway/i3 IPC and ydotool sockets and has no `/dev/uinput`; `Screenshot` blacks out every window outside the list, refused when the tree is unreadable; window reads blank unlisted titles |
| Runaway action loops | `x0ne` hands the keyboard back after `cost.max_consecutive_actions` actions in one turn (default 100, `0` never pauses); an action that failed 4 times the same way is refused before a 5th run (`loop_detected`) |
| High-cost LLM usage | UsageTracker + `/cost` command |
| Unknown actions | Default deny on all write/exec actions |

### What X0NE does NOT protect against

- A **compromised x0ne-runtime binary** — if someone replaces it, all bets are off
- **Prompt injection** — an instruction planted in a file, a web page or a command's output can steer the model, and nothing prevents it. What bounds it: the policy is evaluated on every action before it runs, whoever asked for it (`x0ne-runtime/src/server.rs`, `x0ne-runtime/src/mcp/server.rs`), so an injected instruction gets no more than the policy grants. In `x0ne`'s own agent loop, a deterministic detector (`x0ne-orchestrator/src/injection_guard.rs`: patterns, no model call) marks a matching result untrusted and, for the rest of the session, sends network egress, secret reads and every command that is not a read to a human. It misses what its patterns do not match, and it does not scan the results an MCP client receives from the runtime or the gateway
- Actions the user explicitly **whitelisted** (`RunCommand` with full shell is your problem)
- **Social engineering** — if the LLM convinces you to add `/etc` to allowed paths
- **Network exfiltration by the runtime's own spawns** — agent commands have no network by default (see *Command isolation*), but `ArtifactServe` shares the host network (the proxy must reach it), and `SshExec`, the cloud probes and `x0ne cc` run unconfined by design
- **Desktop confinement limits** — `allowed_apps` does not cover a client that claims a listed `app_id`, the milliseconds between the last check and an injected event, a layer-shell overlay or popup drawn over a listed window (captured with it), or a session socket outside `/run/user/<uid>` and `/tmp` that the runtime's environment does not name
- **Multi-user environments** — the stdio bridge between orchestrator and
  runtime trusts the process that spawned it, and the HTTP mode's bearer token
  is a file readable by the same uid: neither separates two users of one account

X0NE is appropriate for: **single developer, local machine, interactive session**.  
X0NE is NOT designed for: **production servers, multi-tenant environments, untrusted input**.

---

## Policy engine internals

The policy engine (`policy.rs`) is a pure function: `evaluate(action) → PolicyEffect`.

```rust
pub enum PolicyEffect {
    Allow,
    RequireConfirmation(String),
    Deny(DenyReason),
}
```

`DenyReason` names the refusal (`ActionDisabled`, `DefaultDeny`,
`ReadonlyMode`, `PlanMode`, `PathMatchesDenyPattern`, `PathNotInAllowlist`,
`CommandNotAllowed`, `DesktopDisabled`, `PipelineStepDenied`, and others);
the full list is in `policy.rs`.

Evaluation order (first match wins):

```
1. Always allowed     → PolicyCheck, Simulate, CheckHandle, ListHandles,
                        ListCustomPrimitives, ListFunctions (no capability)
2. CallerEnvDenied    → RunCommand / ArtifactServe env outside the allowed map
3. DesktopDisabled    → desktop action + !desktop.enabled
4. PlanMode           → action that touches the host, in plan mode
   ReadonlyMode       → action that touches the host + mode = "readonly"
   RequireConfirmation→ action that touches the host + mode = "confirm"
5. ActionDisabled     → actions[type] == false (a session grant can lift it)
   DefaultDeny        → action absent from [actions] + default_effect = "deny"
6. Composites         → Pipeline, Try, SafeChange, If: every step evaluated first
7. Paths              → deny patterns (the configured list plus the floor), then
                        the read or write roots, on every path the action names
8. CommandNotAllowed  → command not in allowed_commands; image allowlists
9. Allow              → everything else
```

A search does not stop at its root: every file it walks meets step 7, and
the reply counts what was withheld (`omitted_by_policy`).

---

## Path canonicalization and traversal prevention

All file-system actions resolve the path to an absolute path before policy evaluation:

```rust
let canonical = std::fs::canonicalize(path)?;
// or for non-existent paths:
let canonical = std::path::absolute(path)?;
```

Then check against deny patterns and allowed prefixes using the canonical form.

This means:
- `../../etc/passwd` → `/etc/passwd` → denied by deny pattern
- `./src/../src/main.rs` → `/home/user/project/src/main.rs` → allowed
- Symlinks are resolved before policy check

Deny patterns use glob matching (`glob` crate) against the canonical path string. Default deny patterns:

```toml
deny_path_patterns = [
    "**/.ssh/**",
    "**/.gnupg/**",
    "**/id_rsa*",
    "**/id_ed25519*",
    "**/*.key",
    "**/*.pem",
    "**/secrets/**",
    "**/.env*",
    "/proc/**/environ",
    "**/.x0ne/keys/**",
    "**/.x0ne/run/**",
    "/run/user/*/x0ne/**",
    "**/.x0ne/config.toml",
    "**/.x0ne/trusted-projects.toml",
    "**/.x0ne/snapshots/**",
    "**/.x0ne-secrets/**",
    "**/.config/x0ne/**",
    "**/.config/x0ne-orchestrator/**",
    "**/.claude/.credentials.json",
    "**/.codex/auth.json",
    "**/.config/gh/**",
    "**/.config/hub",
    "**/.netrc",
    "**/.git-credentials",
    "**/.kube/**",
    "**/.aws/**",
    "**/.azure/**",
    "**/.config/gcloud/**",
    "**/.docker/config.json",
    "**/.pgpass",
    "**/.pypirc",
    "**/.cargo/credentials*",
    "**/.vault-token",
    "**/.terraform.d/credentials.tfrc.json",
    "**/.password-store/**",
    "**/.local/share/keyrings/**",
]
```

These are checked **before** `allowed_read_paths`, so a typo in allowed_paths cannot accidentally un-deny a sensitive file.

---

## Why no shell

`RunCommand` uses `tokio::process::Command::new(command).args(args)`, which calls `execve()` directly (under bubblewrap by default, see *Command isolation*: bwrap then `execvp`s the same argv). `ArtifactServe` takes a command line but splits it into an argv and refuses shell syntax (`x0ne_actions::serve_command::parse`, `tests/artifact_serve_has_no_shell.rs`); its former `sh -c` is gone — `executor/artifact.rs` only mentions it in a comment. This means:

```
allowed: ["cargo", "test"]
→ execve("/usr/bin/cargo", ["cargo", "test"], env)

NOT: sh -c "cargo test"
→ which would allow: "cargo test; rm -rf ~"
```

Shell injection attacks require a shell. Without a shell:
- `&&`, `;`, `|`, `$()`, `` ` `` — all treated as literal argument strings, not metacharacters
- Redirections (`>`, `<`) — not interpreted
- Glob expansion (`*`, `?`) — not expanded
- Environment variable expansion (`$HOME`) — not expanded by the shell (env is inherited, not interpolated)

What this does NOT mean: if `sh` or `bash` is in `allowed_commands`, the model can ask for `{"command":"sh","args":["-c","…"]}` itself (the runtime's own error hint suggests it). "No shell" is a property of the runtime's parsing, not a sandbox; the command allowlist and the isolation below are what bound such a command.

The tradeoff: the LLM cannot chain commands in a single `RunCommand`. This is intentional. Use a `Pipeline` of `RunCommand` actions instead, or a streaming `RunCommand` followed by `CheckHandle`.

---

## Sensitive file patterns

The default deny patterns are chosen conservatively. Even if `allowed_read_paths = ["/home"]`, these patterns apply first:

| Pattern | Rationale |
|---|---|
| `**/.ssh/**` | SSH private keys, authorized_keys, known_hosts |
| `**/.gnupg/**` | GPG private keys |
| `**/id_rsa*`, `**/id_ed25519*` | Common SSH key filenames |
| `**/*.pem`, `**/*.key` | TLS certificates and private keys |
| `**/secrets/**` | Convention for secrets directories |
| `**/.env*` | Environment files often contain API keys (`.env`, `.env.local`, `.envrc`) |
| `/proc/**/environ` | The environment of any process, the orchestrator's included |
| `**/.x0ne/keys/**`, `**/.x0ne/run/**`, `/run/user/*/x0ne/**`, `**/.x0ne/config.toml`, `**/.x0ne/trusted-projects.toml`, `**/.x0ne/snapshots/**`, `**/.config/x0ne/**`, `**/.config/x0ne-orchestrator/**` | x0ne's own stores: the journal signing key, the HTTP tokens, the configs that hold a model key or hooks, the project trust store, the snapshot copies. A project's `.x0ne/` (rules, skills) stays readable |
| `**/.claude/.credentials.json`, `**/.codex/auth.json` | Other coding agents' login tokens |
| `**/.config/gh/**`, `**/.config/hub`, `**/.netrc`, `**/.git-credentials` | Forge and git credentials |
| `**/.kube/**`, `**/.aws/**`, `**/.azure/**`, `**/.config/gcloud/**`, `**/.docker/config.json` | Cluster and cloud credentials |
| `**/.pgpass`, `**/.pypirc`, `**/.cargo/credentials*`, `**/.vault-token`, `**/.terraform.d/credentials.tfrc.json`, `**/.password-store/**`, `**/.local/share/keyrings/**`, `**/.x0ne-secrets/**` | Other tools' credential files, as hidden from commands by `executor/isolation.rs` |

A pattern that ends in `/**` also denies the directory itself (`**/.ssh/**` denies `~/.ssh`).

To add additional deny patterns:

```toml
[policy]
# The defaults apply whatever this list says: name only your own stores.
deny_path_patterns = [
    "**/credentials",
    "**/.config/my-tool/token",
]
```

The defaults (`DEFAULT_DENY_PATH_PATTERNS` in `x0ne-actions/src/deny_paths.rs`) are a floor: the runtime adds every one the list omits when it loads the file, so `deny_path_patterns = []` still refuses them. To lift a default on purpose, name it, exactly as the defaults write it, in `deny_path_exceptions`:

```toml
[policy]
deny_path_exceptions = ["**/.env*"]   # every other default still applies
```

An exception that names no default refuses the file at load, and each lifted pattern is printed as a warning at startup.

---

## Desktop rate limiting rationale

Desktop actions (click, type, key) share a single `RateLimiter` — a `Mutex<Instant>` tracking the last input event.

Default: 100ms minimum between consecutive input actions.

This means at most ~10 input events per second, which is:
- Fast enough for realistic automation
- Slow enough that a human can intervene if needed
- Too slow to cause harm via rapid-fire commands

In addition, a per-minute cap (`max_input_actions_per_minute = 120`) provides a secondary ceiling.

If rate limit is exceeded, the action returns `Deny(PolicyDenied)` — the LLM must wait before retrying.

---

## Snapshots as safety net

Snapshots complement the policy engine. While policy prevents unauthorized access, snapshots provide a rollback path for authorized operations that go wrong.

Design choices:
- **Snapshots are opt-in** — only created explicitly or via `snapshot_before` in Pipeline
- **Restore is destructive** — for a directory root, files added after the snapshot are DELETED on restore (`executor/snapshot.rs`, atomic-restore cleanup; pinned by the test `restore_removes_files_added_after_snapshot`). Legacy manifests without `roots` skip this pass
- **Max size enforced** — `max_snapshot_size_mb` prevents snapshotting large directories
- **Manifest stored separately** — `manifest.json` records, per file, the original path, the stored file name and the size. It carries **no checksum**: there is no integrity verification before restore (`ManifestEntry` in `executor/snapshot.rs` has no hash field, and nothing in that file computes one)

---

## Command isolation (bubblewrap)

Every command the agent chooses — `RunCommand` (blocking and streaming), `Listen`, `SafeTest`, `ArtifactServe` — is started through `executor/isolation.rs`. By default (`x0ne-runtime --isolation bwrap`) the child runs under bubblewrap:

| Aspect | Default |
|---|---|
| Root filesystem | read-only (`--ro-bind / /`) |
| Writable | `[policy] allowed_write_paths` only (plus the artifact directory for `ArtifactServe`) |
| `/tmp` | private tmpfs; read/write roots under `/tmp` are bound back in |
| `cwd` | not a read root of its own (it was, H1-1); one the action names outside every read or write root is refused before the command is spawned |
| Hidden (empty, read-only) | `~/.ssh ~/.gnupg ~/.aws ~/.azure ~/.config/gcloud ~/.kube ~/.docker ~/.x0ne ~/.x0ne-secrets ~/.config/x0ne ~/.config/gh ~/.netrc ~/.git-credentials` … (`DEFAULT_HIDDEN_HOME_PATHS`), `/run/user/<uid>`, daemon sockets (`/var/lib/incus`, `/run/libvirt`, `/run/docker.sock`, `/run/podman` …), plus `[isolation] hidden_paths` |
| Network | own namespace, loopback only |
| Processes | own PID/IPC/UTS namespaces, `--new-session`, `--die-with-parent` |

Hides are mounted after the write roots, so a secret inside the workspace (workspace = `$HOME`) stays hidden. bwrap keeps supplementary groups: an `incus-admin` user reached `/var/lib/incus/unix.socket` from inside the sandbox until that directory was hidden (measured 2026-09-23).

**Availability.** bubblewrap is looked up in fixed locations (`/usr/bin`, `/usr/local/bin`, `/bin`), never `$PATH`, and probed once (`bwrap --unshare-all … true`). If it is missing or cannot create namespaces, agent commands are **refused** (`missing_dependency`, message naming the way out). There is no fallback to the host.

**Opt-outs are command-line flags only**, typed by a human: `x0ne-runtime --isolation none` (host execution), `--isolation container` (the runtime already runs in Incus/Podman; the autowrap and `make run-incus`/`run-podman` pass it), `--isolation-network host`. The `x0ne` CLI forwards `--isolation` / `--isolation-network` to the local runtime. None of them has an environment variable or a TOML key: a project can ship its own runtime TOML (`bridge.runtime_config`), and `[isolation]` in TOML can only add `hidden_paths`. `x0ne cc` starts its runtime with `--isolation none`: its gcloud argv is built by code and needs the operator's credentials.

**Not isolated:** `SshExec` (remote execution, needs `~/.ssh`), the cloud probes of `cloud.rs` (fixed gcloud argv), the extraction tools of `ml.rs` and the fixed helpers of `system.rs`/`desktop.rs` (fixed binaries), and the container backends below. **Not done yet:** a domain allow-list for the network (planned: an HTTP CONNECT proxy outside the sandbox, reached through a bound unix socket); Landlock without bwrap (bwrap stays required).

**Second layer inside bwrap** (`executor/sandbox_child.rs`): bwrap starts `x0ne-runtime __x0ne-confine`, which sets rlimits (RLIMIT_DATA 4 GiB, NPROC 512, CPU 3600 s, FSIZE 4 GiB; `[isolation.limits]` may only lower them), then Landlock: read and execute on the system directories, the toolchains under `$HOME` (`.cargo`, `.rustup`, `.local/bin`, `.local/lib`, `.nvm`, `.pyenv`, `go`), and the policy read roots, not the cwd (a cwd the action names outside every root is refused); write on the write roots, the private `/tmp` and `/dev`; abstract sockets and signals scoped (ABI 6). Every other path, `$HOME` included, is unreadable. A kernel without Landlock refuses the command. Attacks in `tests/agent_commands_are_confined.rs`, run in a container. Not covered: a private `/tmp` filled up to RAM size (tmpfs is not counted by rlimits; cgroups would be).

---

## Container sandbox hardening

Both container backends (Podman and Incus) enforce multiple security layers:

| Layer | Podman (`RunInSandbox` / `RunContainerGroup`) | Incus (`RunIncus` / `RunIncusGroup`) |
|---|---|---|
| **Image allowlist** | `sandbox.podman.allowed_images` (fail-closed) | `sandbox.incus.allowed_images` (fail-closed) |
| **Capabilities** | `--cap-drop=ALL` | Container profile-based |
| **Privileges** | `--no-new-privileges` (configurable) | Profile-enforced |
| **Memory limit** | `--memory` flag, capped to `max_memory_mb` | Profile-based |
| **CPU limit** | `--cpus` flag, capped to config | Profile-based |
| **Timeout** | `tokio::time::timeout`, capped to `max_timeout_secs` | Same |
| **Network** | `--network=none` by default | Profile-based |
| **Cleanup** | RAII `ContainerGuard` (force-remove on Drop) | `--ephemeral` flag |
| **Env validation** | Alphanumeric + underscore keys only | Same |
| **Path traversal** | Stripped from injected file names | Same |
| **Security profile** | `seccomp` / `apparmor` / `none` | Incus profiles |

The image allowlist is **fail-closed** — if the list is empty, all images are denied. Wildcard prefix matching is supported (e.g. `python:3.*` matches `python:3.12-slim`).

---

## X0NE vs. other approaches

### Raw shell agents (e.g., early AutoGPT, basic LangChain tools)

```
LLM → sh -c "..." → machine
```

No policy. No deny patterns. Any prompt injection in a file read can lead to arbitrary code execution. Not suitable for anything beyond demos.

### MCP (Model Context Protocol)

MCP is a protocol specification, not a runtime, and it does not specify a policy engine: that is up to each server. X0NE ships two MCP servers over stdio: `x0ne-runtime --mode mcp`, whose every tool call meets the same policy as the stdio and HTTP modes (`x0ne-runtime/src/mcp/server.rs`), and the gateway `x0ne-orchestrator --mcp`, which asks the human before a call that changes something. See [mcp.md](mcp.md).

### OpenHands / OpenDevin

Full agent platform with Docker sandboxing. Heavier weight — requires Docker. X0NE is lighter and runs in user space. X0NE is better for: local dev workflow automation, no-container environments, minimal resource usage. OpenHands is better for: isolated execution, web browsing, running untrusted code.

### Open Interpreter

Runs a Python REPL — essentially a shell with extra steps. Very expressive but: no policy engine, no path whitelisting, code execution is unrestricted within the interpreter. X0NE is more conservative and auditable.

---

## Known limitations

1. **HTTP mode: one bearer token, one uid** — `x0ne-runtime --mode http` refuses to start without a token and answers 401 to a request that does not carry it (`x0ne-runtime/src/server.rs`). The token is a file the same uid can read: it does not separate two processes of one account
2. **The journal covers `x0ne`'s own sessions** — the agent loop writes each action and its result to `~/.x0ne/sessions/<id>.jsonl`, owner-only and hash-chained (`x0ne-orchestrator/src/session.rs`, `x0ne-audit`; `x0ne audit verify` checks it). A call an MCP client makes, or a request to the HTTP mode, is not journaled, except the `Term*` actions (`executor/term.rs`); the runtime's own tracing goes to stderr
3. **Resource limits on `RunCommand` are per process** — under bubblewrap, `executor/sandbox_child.rs` sets RLIMIT_DATA 4 GiB, NPROC 512, CPU 3600 s and FSIZE 4 GiB (`[isolation.limits]` may only lower them). There is no cgroup: the sum over the sandbox's processes and the private `/tmp` are not capped, and under `--isolation none` or `container` these limits do not apply. Use `RunInSandbox` / `RunContainerGroup` for resource-capped execution
4. **No domain allow-list** — agent commands have no network by default; `--isolation-network host` gives them all of it, with nothing in between yet
5. **Desktop actions not sandboxed** — they control the actual display, not a virtual one
6. **Snapshot size limit is approximate** — checked per-file, not cumulative during copy
7. **No cryptographic signing** of action results — a MITM on the stdio pipe could forge results
