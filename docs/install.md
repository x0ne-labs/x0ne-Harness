# Install and run

How to get the three binaries, run the agent a first time, and opt into the container sandbox.

## Install

What the machine needs: [Requirements](../README.md#requirements).

From source (no crate is published on crates.io):

```bash
git clone https://github.com/x0ne-labs/x0ne-Harness && cd x0ne-Harness
make install   # builds release + installs to ~/.cargo/bin
```

Network access to crates.io and to the x0ne-Engine repository is needed for the build (see [Add to Claude Code](mcp.md#add-to-claude-code) for the measured duration).

### From a release

`install.sh` downloads a GitHub release, v0.2.0 or later, and verifies it. Linux (x86_64, aarch64, static binaries) and macOS (Apple silicon, Intel):

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/x0ne-labs/x0ne-Harness/main/install.sh | sh
```

### Verify before you run it

A pipe into `sh` runs whatever the server sends. To read the script first:

```bash
curl --proto '=https' --tlsv1.2 -fsSLO https://raw.githubusercontent.com/x0ne-labs/x0ne-Harness/main/install.sh
less install.sh                                    # read it
sh install.sh                                      # then run it
```

What the script does, and nothing else:

- downloads `x0ne-<target>.tar.gz` and `SHA256SUMS` from the release
  (`X0NE_VERSION=v0.2.0` pins a version; default: the latest release),
  with curl, over https only: an http mirror or a redirect to http is
  refused;
- refuses to install if the archive is missing from `SHA256SUMS` or its
  SHA-256 differs;
- if the GitHub CLI 2.56 or later is installed and logged in, runs
  `gh attestation verify --signer-workflow` and refuses on failure; otherwise
  says the provenance is not verified and why, and goes on with the SHA-256
  check alone;
- copies `x0ne`, `x0ne-runtime` and `x0ne-orchestrator` to `~/.local/bin`
  (`X0NE_INSTALL_DIR` to change it), and the sample configs of the archive
  (the runtime profiles and the read-only MCP profile) to `~/.config/x0ne`
  only if absent. No `sudo`; no shell file is edited;
- stops, naming the source build, when the release has no archive for this
  version and platform (HTTP 404).

`sh install.sh --uninstall` removes the three binaries and keeps your configuration.

To verify a release by hand (gh 2.56 or later):

```bash
gh release download v0.2.0 --repo x0ne-labs/x0ne-Harness \
  --pattern 'x0ne-x86_64-unknown-linux-musl.tar.gz' --pattern SHA256SUMS
sha256sum --ignore-missing -c SHA256SUMS
gh attestation verify x0ne-x86_64-unknown-linux-musl.tar.gz --repo x0ne-labs/x0ne-Harness \
  --signer-workflow x0ne-labs/x0ne-Harness/.github/workflows/release.yml
```

Release assets (stable names): `x0ne-x86_64-unknown-linux-musl.tar.gz`,
`x0ne-aarch64-unknown-linux-musl.tar.gz`, `x0ne-x86_64-apple-darwin.tar.gz`,
`x0ne-aarch64-apple-darwin.tar.gz`, `SHA256SUMS`, and the CycloneDX SBOMs
`x0ne-code.cdx.json`, `x0ne-runtime.cdx.json`, `x0ne-orchestrator.cdx.json`.

### Providers, F4 / F5 / F6 in the TUI

Configure in `x0ne-orchestrator.toml`:

```toml
[llm]
provider = "ollama"  # ollama | anthropic | openai-compat

[llm.ollama]
endpoint = "http://localhost:11434"
model    = "mistral"
num_ctx  = 32768     # window asked of Ollama, prompt and answer (default)

[llm.anthropic]
model = "claude-sonnet-4-20250514"

[llm.openai_compat]
base_url = "https://api.openai.com"
model    = "gpt-4o"
```

With Ollama, every call sends `options.num_ctx` (without it, Ollama sizes the
window from the GPU memory: 4096, 32768 or 262144 tokens) and offers five
actions as native tools: `ReadFile`, `WriteFile`, `EditFile`, `RunCommand`,
`SearchCode`. A returned `tool_calls` becomes the same action as its text form,
under the same policy and gate. When Ollama answers 400 `… does not support
tools`, the call is replayed once without them, a warning says so, and the
session stays on the text protocol; any other error stays an error. The
conversation budget is held under the window: `conversation.max_context_tokens`,
at most `num_ctx` minus `max_tokens` (28672 by default). A `num_ctx` not above
`max_tokens` is refused. Tested against a fake Ollama on loopback (`api` tests),
not against a real server.

Local servers have presets on the openai-compat provider: `/provider llamacpp
[model]` for llama.cpp's `llama-server` at 127.0.0.1:9931 (its default port
since 2026-10-09, 8080 before), `/provider lmstudio [model]` for LM Studio at
127.0.0.1:1234. Without a model, the server is asked for its list and its only
model taken. `/model list` and `/setup` ask Ollama, llama.cpp and LM Studio at
those addresses which models they serve. These listings run on request only,
towards a loopback address only (anything else is refused before a socket
opens), 1.5 s per server, without redirect or proxy
(`x0ne-orchestrator/src/local_models.rs`). Tested against fake servers on
loopback, not against the real ones.

Env vars (`.env`):
```
OLLAMA_BASE_URL=http://localhost:11434
OLLAMA_API_KEY=...
ANTHROPIC_API_KEY=...
X0NE_OPENAI_COMPAT_KEY=...
```

A key read from a vendor's variable without the config naming it goes to
that vendor only, over https: `ANTHROPIC_API_KEY` to api.anthropic.com,
`OPENAI_API_KEY` to api.openai.com, `OLLAMA_API_KEY` to ollama.com.
Anywhere else the session stops before any request and says which line to
add. For an Ollama behind an auth proxy, or an openai-compat server that
takes your OpenAI key, name it in the provider's section:
`api_key_env = "OLLAMA_API_KEY"` under `[llm.ollama]`,
`api_key_env = "OPENAI_API_KEY"` under `[llm.openai_compat]`;
`api_key_env = ""` sends no key. A named key, `api_key` and
`X0NE_OPENAI_COMPAT_KEY` go to the configured host, and the session says so
at startup (`key from OPENAI_API_KEY → host:port`). A key that holds
`sk-ant-` anywhere (`Bearer sk-ant-…`, a leading U+FEFF) goes through
provider `anthropic` only. The table is `x0ne-actions/src/model_keys.rs`.

The runtime reads the same table for the `auth` of an `LlmCall`,
`OllamaChat` or `OllamaApi` action, whose variable and endpoint the model
writes. `$ENV:ANTHROPIC_API_KEY`, `$ENV:OPENAI_API_KEY` and
`$ENV:OLLAMA_API_KEY` go to their vendor's hosts over https only, a key that
holds `sk-ant-` to api.anthropic.com only, and api.anthropic.com and
api.openai.com take no key of another format. Anywhere else the action
fails with `auth refused` before any request. A variable the table does not
list goes to the endpoint the action names, under the last two rules. An
action that carries an `auth` meets the confirmation gate, as other network
actions do; one without an `auth` is the agent's own model traffic and does
not. The runtime the session starts itself
(`bridge.mode = "stdio"`, the default) inherits none of
`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OLLAMA_API_KEY` and
`X0NE_OPENAI_COMPAT_KEY`. To give an action a key, export it under a name of
your own before starting `x0ne` (`MY_LLM_KEY`) and have the action say
`"auth": "$ENV:MY_LLM_KEY"`. A sandbox runtime still receives the variables
of `X0NE_FORWARD_ENV`, and a runtime you start yourself
(`bridge.mode = "http"`) your environment, both under the table.

A request to a loopback model endpoint (`localhost`, 127.0.0.0/8, `::1`)
ignores `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY`: the harness hands a
prompt for a loopback endpoint to no proxy. The Anthropic client follows no
redirect.

### Provider `x0ne-engine`, an x0ne-Engine session

`provider = "x0ne-engine"` sends the conversation to the local relay that
`x0ne-engine cloud up` ([x0ne-Engine](https://github.com/x0ne-labs/x0ne-Engine))
serves on loopback. Set it in your own config (`~/.config/x0ne-orchestrator/config.toml`,
`~/.x0ne/config.toml`) or in the system layer below; a project's
`x0ne-orchestrator.toml` cannot set `[llm]`. At the setup question, answering
`x0ne-engine` checks the relay's status, sends no prompt, and saves the
provider only: no key, no model.

```toml
[llm]
provider = "x0ne-engine"

[llm.x0ne_engine]                                      # every key is optional
base_url = "http://127.0.0.1:8478/v1"                  # 127.0.0.1 or [::1], plain http
key_file = "$XDG_RUNTIME_DIR/x0ne-engine/local.key"
require_verified = true                                # false is refused
# model = "..."                                        # default: the relay's model
```

Before every model call, in this order; a failed step sends nothing more:

1. The Bearer key is read from `key_file`: a regular file, not a symbolic
   link, owned by you, no access for group or others. It is never read from
   the environment, and an `api_key` key in `[llm.x0ne_engine]` is a config
   error.
2. Every socket that could take the connection (listening on the relay's
   port, on its address or a wildcard) belongs to your uid, and there is one
   (`/proc/net/tcp`, `/proc/net/tcp6`; refused when they cannot be read).
3. `GET /x0ne/v1/status` answers `"state": "serving"` and `"verified": true`,
   and the session's end (`expires_at_unix`, or `seconds_left`) is not past.

A refusal says which step failed, `Start a session: x0ne-engine cloud up`, and
`No prompt was sent.` There is no fallback: port 8477 (the default of a server
without attestation) is refused in `base_url`, and a refusal at startup is an error, not the
setup question nor another provider. `x0ne --setup` is the one place that
asks: it shows the refusal, then the setup question (an API key, `ollama` or
`x0ne-engine`), and saves the answer in `~/.x0ne/config.toml`, which wins
over the system layer. Native tools are never offered; the text
action protocol is used from the first call. Requests to the relay ignore
`HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` and follow no redirect. The model
label carries the relay's verdict and the session's end, for example
`x0ne-engine · qwen3:0.6b · verified · 52 min left (until 22:05 UTC)`.

Not checked by this client: the attestation itself and the receipt carried by
each answer. It relies on the verdict the local relay reports. What the relay
and the confidential VM guarantee is x0ne-Engine's to state; its README calls
its demonstration client "not end-to-end confidential". The behaviour above
is tested against a fake relay on loopback (`engine_local` tests), not against
a running Engine session.

### System config layer

`/etc/x0ne-orchestrator/config.toml` is read only when you have no config of
your own (neither `~/.config/x0ne-orchestrator/config.toml` nor
`~/.x0ne/config.toml`). A user config replaces it entirely; nothing is merged.
It is ignored, with a warning, unless the file and every directory above it,
symbolic links resolved, are owned by root and not writable by group or
others. Its `[project_trust]` is dropped: only your own config can trust a
project directory.

## Quick Start

```bash
git clone https://github.com/x0ne-labs/x0ne-Harness && cd x0ne-Harness
make install            # builds + installs the 3 binaries to ~/.cargo/bin/
                        # plus runtime profiles to ~/.config/x0ne/
x0ne                    # first run auto-launches the configuration wizard
                        # (model, provider) — re-run anytime with
                        # `x0ne --setup`, or `/setup` from inside a session
```

**No container required.** X0NE runs directly on your host — Linux or macOS (the platforms the release workflow builds). On Linux, agent commands run under bubblewrap (`bwrap`); where bwrap cannot start, they are refused, not run unconfined (see [docs/security.md](security.md)). Snapshots clone files where the filesystem allows it:

| Filesystem | Snapshot method | OS |
|---|---|---|
| btrfs | reflink (`FICLONE`): data shared, not copied (measured) | Linux |
| APFS | `clonefile` (not measured for this release) | macOS |
| ext4 / others | byte copy | Linux |

A restore is file by file on every filesystem.

To remove X0NE: `make uninstall` removes the binaries and the MCP registrations and keeps `~/.config/x0ne` (`make uninstall PURGE=1` lists, then deletes it) and `~/.x0ne/` user data: sessions, snapshots, goals, memory.

## Undo demo

```bash
make demo-undo-fast      # or: ./scripts/demo-undo.sh --no-pause
```

A scripted agent (no model) damages a throwaway project; `RestoreSnapshot` brings back the 6 files and removes the 2 the agent added. Every file is checked by content: if one check fails, the demo prints `DEMO FAILED` and exits 1 (`make test-demo-undo` proves it against a runtime that restores nothing). Measured on 2026-10-16 with the release build, in a container whose `/tmp` is overlayfs on btrfs: snapshot 10 ms and restore 11 ms, both by reflink, for a 28 KB project.

<!-- DEMO RECORDING: docs/media/demo-undo.gif or an asciinema link (file to come). -->

## Optional: full container sandbox

If you want **process isolation** on top of snapshot/restore — e.g. to run untrusted code via `RunInSandbox`, or to give the agent a fresh filesystem each turn — opt in via:

```bash
make setup-podman    # Linux + macOS + Windows (uses Podman Machine on Mac)
make setup-incus     # Linux only — fastest, btrfs storage pool, baseline container
```

Each needs the daemon already installed: when the `incus` or `podman` CLI is missing, the target stops, installs nothing, and prints the `scripts/install-sandbox-backend.sh` command to run yourself (sudo). With the CLI present it creates the `x0ne-sandbox` container and captures a baseline snapshot. Idempotent, safe to re-run. Plain `x0ne` still runs the runtime on the host (`[bridge] mode = "stdio"`, the default). `x0ne --sandbox`, `X0NE_AUTO_SANDBOX=1` or `mode = "auto"` makes it probe Incus then Podman and run the runtime in `x0ne-sandbox`, with a warning and a host fallback when no container is ready. The probe starts `x0ne-sandbox` when it exists and is stopped. Known limit: the REPL, `-p`, `--tui` and `--matrix` probe at every start, a stdio session included, so an installed `incus` or `podman` gets that `start` even when the session will not use the container. `X0NE_REQUIRE_SANDBOX=1` turns that fallback into a refusal to start the session. Limit: `x0ne cc` and `X0NE_VM_DOWNSTREAM` are not covered by that refusal.

```bash
# Non-interactive, run a single prompt and exit
x0ne -p "what OS is this and how much disk is free"

# Pipe mode, feed stdin as context
cat error.log | x0ne --stdin -p "explain this error and fix it"

# @file references, inject file contents inline
x0ne -p "refactor @src/main.rs to use async"

# Autonomous mode, continuous loop with git checkpoints
x0ne --auto-mode --max-iterations 30
```

```
Orchestrator (LLM loop) ──[Bridge: stdio / HTTP / SSH]──▶ Core (executor + policy)
```

## Container Isolation, Incus Setup

X0NE can execute code inside ephemeral [Incus](https://linuxcontainers.org/incus/) (LXC) containers for full process isolation. Two modes are supported: **local** (via Unix socket) and **remote** (via TLS, recommended for production, especially with btrfs for instant CoW snapshots).

```bash
make run-incus-runtime    # build, deploy x0ne-runtime to the Incus container, run the CLI on the host
make run-incus            # run x0ne itself inside a throwaway Incus container (binaries + ~/x0ne-work only)
```

### Mode 1, Local Incus (Unix socket)

No configuration needed if Incus is installed locally:

```bash
# Install Incus
sudo apt install incus
sudo incus admin init   # accept defaults

# Verify
incus list
```

x0ne-runtime will connect via `/run/incus/unix.socket` automatically.

---

### Mode 2, Remote Incus server (btrfs, recommended for production)

A remote Incus server backed by **btrfs** allows container creation via **Copy-on-Write snapshots** in under 200ms. This is the fastest and most scalable sandboxing option.

#### Step 1, Enable the Incus REST API on the server

```bash
ssh your-incus-server "incus config set core.https_address '0.0.0.0:8443'"

# Verify it's listening
ssh your-incus-server "ss -tlnp | grep 8443"
```

#### Step 2, Generate a client certificate for X0NE

```bash
mkdir -p ~/.config/x0ne/incus

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout ~/.config/x0ne/incus/prod-infra.key \
  -out    ~/.config/x0ne/incus/prod-infra.crt \
  -days   3650 -nodes \
  -subj   "/CN=x0ne-client"
```

#### Step 3, Trust the client cert on the server

```bash
# Server generates a one-time token
ssh your-incus-server "incus config trust add x0ne"
# → prints a base64 token

# Add the remote using that token
incus remote add prod-infra https://<SERVER_IP>:8443 \
  --accept-certificate \
  --auth-type tls \
  --token <TOKEN_FROM_ABOVE>
```

You should see: `Client certificate now trusted by server: prod-infra`

#### Step 4, Retrieve the server CA certificate

```bash
ssh your-incus-server "cat /var/lib/incus/server.crt" \
  > ~/.config/x0ne/incus/prod-infra-ca.crt
```

#### Step 5, Smoke test

```bash
# List containers on the remote
incus list prod-infra:

# Launch an ephemeral container (instant on btrfs)
incus launch images:alpine/edge prod-infra:x0ne-test --ephemeral
incus exec prod-infra:x0ne-test -- echo "incus-ok"
incus delete prod-infra:x0ne-test --force
```

---

### x0ne-runtime.toml, Incus configuration

```toml
[sandbox]
# "podman" (default) or "incus"
default_backend = "incus"

[sandbox.incus]
# Remote configured via `incus remote add`
default_remote = "prod-infra"

# Allowlist, empty list = all images denied (fail-closed)
allowed_images = [
  "images:ubuntu/24.04",
  "images:alpine/edge",
  "images:debian/12",
]

# Resource defaults (LLM can request less, never more)
default_memory_mb    = 512
default_cpu_limit    = 1
default_disk_mb      = 2048
default_timeout_secs = 60

# Security hardening
security_profile   = "default"   # AppArmor profile
no_new_privileges  = true
default_network    = "none"      # "none" | "bridge"

# Remote connection (TLS mutual auth)
[sandbox.incus.remotes.prod-infra]
url  = "https://192.0.2.5:8443"
cert = "~/.config/x0ne/incus/prod-infra.crt"
key  = "~/.config/x0ne/incus/prod-infra.key"
ca   = "~/.config/x0ne/incus/prod-infra-ca.crt"

# Local socket (no TLS needed)
[sandbox.incus.remotes.local]
url  = "unix:///run/incus/unix.socket"
```

---

### Why btrfs matters

On a btrfs-backed Incus server, new containers are created via **Copy-on-Write** from a base image snapshot. This means:

A full copy (ext4 and other pools without CoW) costs time proportional to the image size; a btrfs snapshot clone does not. Start times were not measured for this release.

To use an existing container as a CoW base template, create it once and snapshot it:

```bash
# Create a pre-warmed template on the btrfs server
incus launch images:ubuntu/24.04 prod-infra:template-base
incus exec prod-infra:template-base -- apt-get install -y python3 nodejs
incus snapshot prod-infra:template-base ready

# X0NE will clone from this snapshot instantly
# (set base_snapshot = "template-base/ready" in RunIncus)
```

## Make targets

```
make build             build release (default members)
make build-dev         build debug
make install           install to ~/.cargo/bin
make run               local mode (CLI)
make run-tui           local mode (TUI)
make run-matrix        matrix mode (LLM-controlled canvas)
make run-mcp           run x0ne-runtime as MCP server
make run-incus         x0ne inside a throwaway Incus container (binaries + ~/x0ne-work only)
make run-podman        same guarantees as run-incus, on Podman
make run-incus-runtime CLI on the host, x0ne-runtime in the Incus sandbox
make run-incus-matrix  Incus container (isolated Matrix mode)
make test              unit tests only
make test-all          full suite: unit + scenarios + Incus
make test-matrix       run matrix mode unit tests only
make test-desktop      desktop/Wayland tests (requires grim, wlrctl, wtype)
make scenarios-incus   Incus scenarios only
make dev               watch + rebuild on save
make demo              send 3 scripted commands for quick testing
make demo-undo         undo demo: scripted damage, restore, every file checked
make demo-undo-fast    same as above, no pauses (for screencasts)
make test-demo-undo    the demo fails loudly against a runtime that restores nothing
```
