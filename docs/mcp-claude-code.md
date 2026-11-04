# Plug X0NE into Claude Code (MCP)

X0NE ships an MCP (Model Context Protocol) server that exposes 70 typed
primitives (`tools/list`, 2026-11-02) to an MCP client. [mcp.md](mcp.md)
configures Claude Code, Codex, Claude Desktop, Cursor and Windsurf, and lists
the tools.

## TL;DR

```bash
# 1. Make sure x0ne-runtime is on PATH
which x0ne-runtime              # /home/you/.cargo/bin/x0ne-runtime

# 2. Register X0NE with Claude Code (runtime + gateway, both with --config)
make claude-add
# or the runtime alone, by hand:
claude mcp add x0ne -- x0ne-runtime --mode mcp \
    --config "$HOME/.config/x0ne/x0ne-runtime-mcp.toml" --log-level error

# 3. Restart Claude Code: `/mcp` lists "x0ne" with 70 tools
```

Without `--config` the runtime refuses to start: see below.

The MCP tools are gated by the runtime; Claude Code's own `Bash`, `Edit` and
`Write` are not, unless you also wire the PreToolUse hook:
[agent-hooks.md](agent-hooks.md).

## Three deployment profiles

You probably don't want to give Claude Code the full default
runtime config (which allows `RunCommand`, `WriteFile`, container
spawning, etc.). Pick the profile that matches what you trust this
session to do.

### 1. Default (read-only, what this repo ships)

Uses `~/.config/x0ne/x0ne-runtime-mcp.toml`, deposited by `make install`
from `x0ne-runtime-mcp-readonly.toml`, or by `install.sh` from the release
archive.

```json
{
  "mcpServers": {
    "x0ne": {
      "type": "stdio",
      "command": "x0ne-runtime",
      "args": [
        "--mode", "mcp",
        "--config", "${HOME}/.config/x0ne/x0ne-runtime-mcp.toml",
        "--log-level", "error"
      ]
    }
  }
}
```

`--config` is the load-bearing part of it.

> **Why `--config` is not optional.** Until 2026-08-15 this entry omitted
> it. `find_and_load` then fell through to `~/.config/x0ne/x0ne-runtime.toml`
> — the *CLI* reference profile, which carries `policy.mode = "auto"` and
> `default_effect = "allow"` because a human is sitting in front of `x0ne`.
> An MCP client is not a human. The result, reproduced on the real binary:
> `run_command {"command": "id"}` from an unauthenticated MCP client
> returned the host's uid and group list, with `isError: false`. Naming the
> policy file is what keeps an MCP session from inheriting a policy written
> for a different threat model.
>
> The runtime now also refuses to start `--mode mcp` under
> `policy.mode = "auto"` unless the launcher passes `--upstream-human-gate`
> to assert it owns the human gate (which is what `x0ne-orchestrator --mcp`
> does). And it prints the absolute path of the policy it loaded to stderr
> at startup, at every log level — so you can always check.

### 1b. Write-enabled from an MCP client

Point `--config` at a profile you wrote, or use the `x0ne-gateway` entry
(`make claude-add`), which front-ends the runtime with the orchestrator.
Do not simply drop `--config`: that does not "enable writes", it removes
the reasoning about who approves them.

**How the gateway asks the human.** The gateway starts the runtime
with `--upstream-human-gate`, so the confirmation is its job, and it does it
through the MCP client with *elicitation* (MCP specification
[2025-11-25, client/elicitation](https://modelcontextprotocol.io/specification/2025-11-25/client/elicitation),
form mode; also served to `2025-06-18` clients):

- Each forwarded call is classified with the harness's own predicate (the
  one the `x0ne` agent loop gates on: `SideEffectClass` from `x0ne-actions`,
  composites walked, read-only commands and network reads waived). Reads
  are forwarded as is.
- A side-effecting call (write, command, network write, desktop, delete,
  `render_template`, any `vm_*` or unknown tool) sends `elicitation/create`
  to the client with the tool, its class and its key arguments (escaped,
  truncated, secret-looking keys redacted). It is forwarded only if the user
  **accepts with `approve` ticked**. Decline, cancel, an error answer, no
  answer within 120 s, or a response to any other id: nothing is forwarded
  and the call returns `isError: true` with `human_gate_refused: …`.
- A client that did not declare the `elicitation` capability at
  `initialize` cannot ask anyone: side-effecting calls are **refused**,
  read-only ones still pass.
- The native tools whose effect leaves the gateway, `x0ne_host_add`,
  `x0ne_host_remove`, `x0ne_host_use` (the inventory and active pointer
  under `~/.x0ne`), `x0ne_artifact_expose` and `x0ne_artifact_unexpose` (a
  URL opened or closed on this machine), are asked the same way before
  their handler runs; the question says what the tool does. `x0ne_host_add`
  also refuses a `target` that `ssh` would read as an option and any
  `ssh_options` key outside the `SshExec` allowlist (audit of 2026-10-30,
  H3-2).
- `x0ne_plan_execute {approve: true}` follows the same rule: `approve` is
  the model's word, a plan with a side-effecting step also needs the user's.
  The same holds for `x0ne_render_template` and for a goal whose achievement
  check runs a command or an action (`x0ne_goal_step`).

The client's own tool-permission prompt is a separate, earlier layer; this
one is the gateway's.

### 2. Read-only (audit, exploration, code review)

For when you want Claude Code to *understand* a project but not
*modify* it. The profile of § 1, under its own name, straight from the
source tree (`x0ne-runtime-mcp-readonly.toml`: reads only, no command).

```bash
claude mcp add x0ne-readonly -- x0ne-runtime --mode mcp \
    --config /path/to/x0ne-Harness/x0ne-runtime-mcp-readonly.toml
```

Or in `.mcp.json`:

```json
{
  "mcpServers": {
    "x0ne-readonly": {
      "type": "stdio",
      "command": "x0ne-runtime",
      "args": [
        "--mode", "mcp",
        "--config", "/path/to/x0ne-runtime-mcp-readonly.toml"
      ]
    }
  }
}
```

What's denied: `WriteFile`, `RunCommand`, `RunInSandbox`,
`CreateSnapshot/Restore/Delete`, all desktop primitives, all network
writes. `~/.ssh`, `~/.aws`, `*.key`, `*.pem`, `.env*` are
glob-denied even within allowed paths.

### 3. Sandboxed (let Claude Code rm -rf — but inside a container)

Same idea as `x0ne` itself: the runtime runs **inside** an Incus or
Podman container, so even `rm -rf /` only empties the container. Run
`make sync-incus-musl` once. The gateway starts the runtime in the
container and keeps asking you before a call that changes something:

```json
{
  "mcpServers": {
    "x0ne-sandbox": {
      "type": "stdio",
      "command": "x0ne-orchestrator",
      "args": [
        "--mcp",
        "--downstream-prefix", "incus exec x0ne-sandbox --",
        "--downstream-runtime", "/opt/x0ne-runtime",
        "--downstream-config", "/root/x0ne-runtime.toml"
      ]
    }
  }
}
```

The container's profile (`x0ne-runtime-sandbox.toml`) is in `auto` mode,
so started directly from a client, without the gateway, the runtime
refuses to start. `incus snapshot create` and `incus snapshot restore`,
from the host, roll the container back. The host filesystem is not
mounted in it.

## Verifying it works

After registering and restarting Claude Code:

```
/mcp
```

You should see `x0ne` (or `x0ne-readonly` / `x0ne-sandbox`) listed
with 70 tools. Try:

> Ask Claude: "use the x0ne `policy_check` tool on a `read_file` of ~/.ssh/config"

You should get a refusal that names the deny pattern.

## What you actually get

Compared to Claude Code's built-in tools, X0NE adds:

- **Typed primitives instead of raw shell**. `RunCommand` has a whitelist
  with no shell metacharacters; `WriteFile` does path canonicalization
  before policy check.
- **Snapshots and restore**. `create_snapshot` before a risky step,
  `restore_snapshot` to undo; each file is copied aside, by reflink where
  the filesystem allows, then renamed over the current one.
- **Pipeline**: several actions in a single MCP call (`pipeline`), each
  step checked by the policy before the first one runs, with per-step
  results.
- **System and handles**: `list_processes`, `port_check`, `dns_lookup`,
  `gpu_status`, background handles and terminals (`term_*`).

## Troubleshooting

**`/mcp` shows x0ne but with 0 tools**
The runtime crashed at startup. Check `~/.x0ne/logs/core.log` —
most often the config TOML can't load, or the policy is in `auto` mode,
which MCP refuses. Point `--config` at a known-good TOML: without one the
runtime does not fall back on defaults, it refuses to start.

**"Tool execution failed: This action type is disabled"**
Working as intended — the policy denied it. Check the profile you
loaded vs what the LLM tried to do. Ask Claude to call
`policy_check` first if it's unsure.

**"x0ne-runtime: command not found"**
Claude Code spawns from your shell's PATH. Either install via
`make install` (puts the binary at `~/.cargo/bin/x0ne-runtime`) or
use an absolute path in `.mcp.json` (`"command": "/full/path/to/x0ne-runtime"`).
