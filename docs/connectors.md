# Connectors

A connector is a third-party [MCP](https://modelcontextprotocol.io) server
(GitHub, Gmail, Slack, …) whose tools the model can call. Each tool is offered
as `<connector>.<tool>`, for example `github.create_issue`.

Connectors are declared in your **user** config
(`~/.config/x0ne-orchestrator/config.toml` or `~/.x0ne/config.toml`). A project
`x0ne-orchestrator.toml` cannot declare one: that would let a cloned repository
start a process.

## `/connectors`

| Command | Effect |
|---|---|
| `/connectors` | every connector: on/off, transport, state, and its tools marked `[read]` or `[asks]` |
| `/connectors enable <name>` | start it now and at every later session |
| `/connectors disable <name>` | stop it now and at every later session |
| `/connectors trust <name>` | accept the server's current tools after a pin alert (see below) |

The enable/disable choice is stored in `~/.local/share/x0ne/connector-state.json`.

## Declaring a connector

```toml
[connectors.github]
enabled = true                    # templates are off until you say so

[connectors.notes]                # your own server
transport = "stdio"               # or "http" (MCP streamable HTTP)
command = "/usr/local/bin/notes-mcp"
args = ["--stdio"]
env = { NOTES_TOKEN = "${secret:notes_token}" }
read = ["get_*", "list_*"]        # these run without a question
hide = ["delete_*"]               # never shown to the model

[connectors.remote]
transport = "http"
url = "https://example.invalid/mcp/"
headers = { Authorization = "Bearer ${secret:remote_token}" }
```

A table named after a template (`github`, `gmail`, `gcalendar`, `slack`,
`filesystem`, `fetch`) overrides that template key by key.

## Plugging in your own token

Never write a token in `config.toml`. Reference it:

- `${secret:NAME}`: read from `~/.config/x0ne/connector-secrets.toml`, a flat
  TOML table (`NAME = "value"`). The file **must be mode 0600**; any other mode
  is refused.

  ```sh
  install -m 600 /dev/null ~/.config/x0ne/connector-secrets.toml
  $EDITOR ~/.config/x0ne/connector-secrets.toml   # github_token = "github_pat_…"
  ```
- `${keyring:NAME}`: read with `secret-tool lookup service x0ne-connectors account NAME`
  (libsecret). Store it with
  `secret-tool store --label "x0ne github" service x0ne-connectors account github_token`.
- `${env:NAME}`: read from x0ne's own environment.

What happens to a credential:

- it is passed to the server process only (its environment or HTTP headers);
- a stdio server starts from an **empty** environment plus `PATH`, `HOME`,
  `LANG`, `LC_ALL`, `TMPDIR`, `USER`, `TERM` and what you declared, so it never
  sees your LLM API keys;
- the server's stderr is discarded;
- every resolved value is masked (`[secret]`) in what the model receives, in
  the connector log, and on screen. Masking is a literal match: a server that
  re-encodes the token (base64, reversed) is not caught. The server is trusted
  with the token by design; masking protects the context and the logs.

## Security

**Tool pinning.** At first use, x0ne records a SHA-256 of each tool's name,
description and input schema in `~/.local/share/x0ne/connector-pins.json`. The
list is fetched again before every call. A changed tool, or a tool that was not
there when pins were taken, **blocks the whole connector** and shows an alert.
A removed tool is not an alert. After review, `/connectors trust <name>`
re-pins.

**Read or publish.** A tool matching the connector's `read` patterns runs
without a question. Every other tool is a publishing action (sending an e-mail,
creating an issue, pushing code, posting a comment) and is **confirmed every
time, in sandbox yolo too**. The prompt offers no session-wide "always". The
`read` list is an allowlist: a new or renamed tool upstream is asked about, it
never runs silently. Server-declared hints (`readOnlyHint`) are not trusted.

**Untrusted output.** Every result reaches the model inside
`<untrusted source="connector:NAME/TOOL">…</untrusted>` with a notice that it is
data, not instructions. The text is scanned by the session's injection guard
(`injection_guard.rs`); a hit sets `injection_suspected: true` with the rules
found, wraps the result in the guard's untrusted banner, journals it, and makes
network egress, secret reads and every program that is not a read ask for the
rest of the session, in sandbox yolo too. A fence escape
is flagged too. Tags that could close the fence or pass for an action are
defanged. Detection is pattern matching: it marks, it does not prove safety.

**Log.** One JSON line per call in `~/.local/share/x0ne/connectors.log` (mode
0600): time, connector, tool, masked arguments, outcome (`ok`, `tool_error`,
`denied`, `refused`, `blocked`, …) and the first 300 characters of the masked
result.

## Templates

All disabled. None carries a credential. Checked 2026-10-06.

| Name | Server | Needs | `read` |
|---|---|---|---|
| `github` | [github/github-mcp-server](https://github.com/github/github-mcp-server) (official, `github-mcp-server stdio`) | `github_token` (fine-grained PAT) | `get_*`, `list_*`, `search_*`, `*_read` |
| `gmail` | [taylorwilsdon/google_workspace_mcp](https://github.com/taylorwilsdon/google_workspace_mcp) (`uvx workspace-mcp --tools gmail`) | `google_oauth_client_id`, `google_oauth_client_secret` | `search_*`, `get_*`, `list_*` |
| `gcalendar` | same server, `--tools calendar` | same | `get_*`, `list_*`, `query_*` |
| `slack` | [zencoderai/slack-mcp-server](https://github.com/zencoderai/slack-mcp-server) (the former reference server, now maintained by Zencoder) | `slack_bot_token`, `slack_team_id` | `slack_list_*`, `slack_get_*` |
| `filesystem` | [MCP reference filesystem](https://github.com/modelcontextprotocol/servers/tree/main/src/filesystem), rooted at `.` | nothing | `read_*`, `list_*`, `directory_tree`, `search_files`, `get_file_info` |
| `fetch` | [MCP reference fetch](https://github.com/modelcontextprotocol/servers/tree/main/src/fetch) (`uvx mcp-server-fetch`) | nothing | none: a URL can carry data out, so every fetch asks |

Google Workspace MCP opens a browser for the OAuth consent the first time; the
client id and secret come from your own Google Cloud OAuth client. GitHub's
server also accepts `--read-only`: add it to `args` to remove every write tool
at the source.

## Tests

`x0ne-orchestrator/tests/connectors.rs` and `connectors_session.rs` run against
`examples/fake_mcp_server.rs`, a three-tool stdio server, and a local HTTP
server. No network, no account.
