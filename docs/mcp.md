# MCP server

`x0ne-runtime --mode mcp` serves the runtime's actions as MCP tools over
stdio (JSON-RPC 2.0, one message per line). `x0ne-orchestrator --mcp` is a
gateway in front of it: it starts a runtime of its own, forwards its tools,
adds its `x0ne_*` tools, and asks you before a call that changes something.

Measured on 2026-11-02 with `tools/list`: the runtime lists 70 tools. The
list does not depend on the policy; the policy decides at each call, and a
refused call returns `isError: true` with the reason. MCP calls are not
journaled by x0ne.

## Two entries

| Entry | Command | What decides |
|---|---|---|
| `x0ne` | `x0ne-runtime --mode mcp --config ~/.config/x0ne/x0ne-runtime-mcp.toml` | the read-only profile: reads under `~`, no write, no command |
| `x0ne-gateway` | `x0ne-orchestrator --mcp --downstream-runtime x0ne-runtime` | your runtime policy, plus a question to you, through the client, before any call that changes something |

`make install` writes `~/.config/x0ne/x0ne-runtime-mcp.toml` from
`x0ne-runtime-mcp-readonly.toml`, and `install.sh` copies the same file from
the release archive (an edited copy is kept). `make claude-add`
and `make codex-add` register both entries with Claude Code and Codex.

`--config` is not optional. Without it the runtime reads the policy written
for the `x0ne` CLI, where a human answers the prompts, and refuses to start
in MCP mode: an MCP client has no human behind it, so it accepts only
`policy.mode = "readonly"` or `"confirm"` there. To let an MCP client write
or run commands, use the `x0ne-gateway` entry, or write your own profile in
`confirm` mode and point `--config` at it.

`--upstream-human-gate` belongs to the gateway: it passes it to the runtime
it starts, because it asks the human itself. Do not add it to a client's
entry: the runtime would then run every enabled action with no one asked.

How the gateway asks, and what a client without MCP elicitation gets (reads
only): [mcp-claude-code.md](mcp-claude-code.md), "Write-enabled from an MCP
client".

## Client configuration

The runtime entry in each client. Replace `/home/you` with your home: not
every client expands `~` or `$HOME` in its arguments.

Claude Code:

```bash
make claude-add        # both entries, with --config
# or by hand:
claude mcp add x0ne -- x0ne-runtime --mode mcp \
    --config /home/you/.config/x0ne/x0ne-runtime-mcp.toml --log-level error
```

Claude Desktop (`~/.config/Claude/claude_desktop_config.json` on Linux,
`~/Library/Application Support/Claude/claude_desktop_config.json` on macOS),
Cursor (`.cursor/mcp.json` or the global MCP settings) and Windsurf take the
same `mcpServers` block:

```json
{
  "mcpServers": {
    "x0ne": {
      "command": "x0ne-runtime",
      "args": ["--mode", "mcp",
               "--config", "/home/you/.config/x0ne/x0ne-runtime-mcp.toml",
               "--log-level", "error"]
    },
    "x0ne-gateway": {
      "command": "x0ne-orchestrator",
      "args": ["--mcp", "--downstream-runtime", "x0ne-runtime", "--log-level", "warn"]
    }
  }
}
```

Codex (`~/.codex/config.toml`, what `make codex-add` writes):

```toml
[mcp_servers.x0ne]
command = "/home/you/.cargo/bin/x0ne-runtime"
args = ["--mode", "mcp", "--config", "/home/you/.config/x0ne/x0ne-runtime-mcp.toml", "--log-level", "error"]

[mcp_servers.x0ne-gateway]
command = "/home/you/.cargo/bin/x0ne-orchestrator"
args = ["--mcp", "--downstream-runtime", "x0ne-runtime", "--log-level", "warn"]
```

Any other client: start the same command over stdio. To check that the
server starts and which policy it loaded, the startup line on stderr names
the file in force:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}' \
  | x0ne-runtime --mode mcp --config ~/.config/x0ne/x0ne-runtime-mcp.toml
```

## The tools

The 70 tools of 2026-11-02, by family. `tools/list` is the reference; each
tool carries its JSON Schema and its MCP annotations (read-only,
destructive).

| Family | Tools |
|---|---|
| Files, read | `read_file`, `read_files`, `stat_files`, `file_info`, `checksum`, `read_logs`, `search_code`, `search_and_read` |
| Files, write | `write_file`, `edit_file`, `apply_patch`, `patch_set`, `safe_change`, `move_to_trash` |
| Snapshots | `create_snapshot`, `restore_snapshot`, `list_snapshots`, `delete_snapshot` |
| Commands | `run_command`, `run_program`, `preview_program`, `safe_test`, `run_in_sandbox`, `ssh_exec` |
| Handles, terminals | `check_handle`, `list_handles`, `stop_handle`, `subscribe`, `term_open`, `term_send`, `term_read`, `term_wait`, `term_snapshot`, `term_resize`, `term_list`, `term_close` |
| System, network | `list_processes`, `port_check`, `dns_lookup`, `gpu_status`, `cloud_cost`, `send_notification` |
| Artifacts | `artifact_create`, `artifact_save`, `artifact_serve`, `artifact_stop`, `artifact_list`, `artifact_destroy` |
| Desktop | `screenshot`, `click`, `double_click`, `move_mouse`, `scroll`, `type_text`, `press_key`, `press_key_combination`, `get_clipboard`, `set_clipboard`, `focus_window`, `get_focused_window`, `get_window_tree` |
| Functions | `define_function`, `call_function`, `list_functions`, `delete_function` |
| Composition, meta | `pipeline`, `simulate`, `policy_check`, `ask_human`, `confirm` |

`ask_human` and `confirm` answer `isError: true` in a bare MCP runtime:
its stdin carries the protocol, so no human is reachable (measured
2026-11-02). The gateway is the entry that asks you.

## What the policy enforces

The same `PolicyEngine` as the CLI, on every call:

- read and write roots (`allowed_read_paths`, `allowed_write_paths`), after
  `~` is expanded and the path canonicalized; an empty list allows nothing;
- the deny list (`deny_path_patterns` plus the built-in floor: keys, `.env`,
  credential stores, see [security.md](security.md)) on every path an action
  names and on every file a search walks;
- `allowed_commands` for `run_command`, executed without a shell;
- `policy.mode`: `readonly` refuses every action that changes the host,
  `confirm` refuses them too in MCP (no one can approve), `auto` is refused
  at startup unless the gateway runs the runtime.

The policy governs what the runtime itself reads and writes. A command it
runs reads what the command reads; the isolation (`--isolation`, bwrap on
the host) is what bounds it. See [SECURITY.md](../SECURITY.md).

## Where the runtime finds its policy
x0ne-runtime looks for config in this order (`x0ne-runtime/src/config.rs`,
`find_and_load_with_source`):
1. `--config <path>`
2. `./x0ne-runtime.toml` — **only** when `X0NE_RUNTIME_ALLOW_CWD_CONFIG=1` is
   set. A policy file in the working directory is the policy of whoever
   controls that directory (a cloned repository, an agent that can write one
   file), so it is never read by default; the runtime logs the skip to
   `~/.x0ne/logs/core.log` and the `x0ne` CLI says it on stderr at startup.
3. `~/.config/x0ne/x0ne-runtime.toml` (what `make install` deposits)
4. `~/.config/x0ne-runtime/config.toml` (pre-0.2 layout)

None found: the runtime refuses to start, and the message names the file to
write (`x0ne --setup` writes it). The same refusal for a file that is empty or
comments only, unreadable, invalid, or a dangling link: a link at one location
never falls through to the next.

A partial `[policy]` table is completed with the fail-closed defaults
(`default_effect = "deny"`, empty allowlists);
only the keys you write change. One exception: a table that names `allowed_*`
entries must also state `default_effect` (a grant without it is the shape of a
truncated or pre-0.2 file, and is refused with the line to add). The
`x0ne-runtime` startup line names the file actually in force.

## HTTP API (clients without MCP)

For tools that don't support MCP, use the HTTP API:

```bash
curl -s http://localhost:7898/execute \
  -H "Authorization: Bearer $(cat "$XDG_RUNTIME_DIR/x0ne/runtime-7898.token")" \
  -H "Content-Type: application/json" \
  -d '{"type": "ReadFile", "path": "./src/main.rs"}' | jq
```

> **Security:** a bearer token is always required. Without `auth_token` under
> `[runtime.http]`, one is generated at startup into a 0600 file whose path is
> printed on stderr. `Host` must be a loopback name with the bound port (or in
> `allowed_hosts`); browser `Origin`s are refused unless in `allowed_origins`.
