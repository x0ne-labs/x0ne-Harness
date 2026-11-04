# Development

## Testing

Three-tier test suite, all run without an LLM:

```bash
make test-all    # runs everything
```

| Tier | What | Count (measured 2026-10-16, Linux container) |
|------|------|-------|
| **Unit and integration tests** | `cargo test --workspace --all-features` (doctests included) | 3 496 tests: 3 488 passed, 0 failed, 8 ignored |
| **Scenario suite** | JSON-driven in-process harness (`x0ne-scenario-runner`) | 186 cases across 13 categories, 186 passed |
| **Incus integration** | Real containers on a remote Incus server (`make scenarios-incus`) | 13 cases, not run for this release |

### Scenario categories

| Category | Tests | Description |
|----------|-------|-------------|
| `composites` | 10 | DefinePrimitive/CallPrimitive lifecycle |
| `edge_cases` | 20 | Empty paths, nonexistent files, zero timeouts |
| `happy_path` | 39 | Full CRUD, git, system info, memory |
| `human_interaction` | 12 | AskHuman, Confirm, QCM with scripted responses |
| `malformed` | 18 | Invalid JSON, missing fields, wrong types |
| `pipeline` | 5 | Sequential multi-step execution |
| `pipeline_advanced` | 12 | Parallel, stop_on_error, snapshot, nested rejection |
| `policy_violations` | 22 | Path traversal, forbidden commands, SSRF, secrets |
| `qcm` | 16 | AskMultipleChoice: skip, other, out-of-range |
| `scripted_functions` | 10 | Rhai scripted primitives |
| `snapshots` | 9 | Create/restore/delete/list snapshots |
| `ssh_exec` | 6 | SshExec argument handling |
| `try_rollback` | 7 | Transactional execution with automatic rollback |

`desktop.json` (2 cases) only runs with `--desktop`; `incus.json` (13 cases) is the Incus tier above.

### Incus pre-flight

When `--incus` is set with a non-local remote, the scenario runner **probes connectivity** with a 5s timeout before running any container tests. If the remote is unreachable, Incus tests are skipped with a warning instead of failing with 20s timeouts per test:

```
⚠  Incus remote 'prod-infra' is unreachable, skipping Incus scenarios
   Fix: incus remote list  |  check server at remote 'prod-infra'
```

## Project layout

```
x0ne/
├── x0ne-mcp/                      # Shared MCP / JSON-RPC plumbing (wire-level only)
│   └── src/
│       ├── jsonrpc.rs             # JSON-RPC 2.0 types + error codes
│       ├── transport.rs           # Line-delimited stdio transport
│       └── child_client.rs        # Spawn + speak MCP to a downstream server
├── x0ne-runtime/                  # Executor crate, zero AI, typed primitives only
│   ├── src/
│   │   ├── action.rs              # Action enum (122 variants)
│   │   ├── result.rs              # ActionResult / ActionOutput
│   │   ├── policy.rs              # PolicyEngine
│   │   ├── config.rs              # AppConfig
│   │   ├── mcp/                   # MCP server (tools/list, tools/call…)
│   │   └── executor/              # One file per primitive category
│   ├── bin/x0ne-scenario-runner/  # JSON-driven test harness binary
│   └── tests/scenarios/           # 175+ JSON test scenarios
├── x0ne-orchestrator/             # Orchestrator library + headless serve binary
│   ├── src/
│   │   ├── lib.rs                 # Public modules (consumed by x0ne-code)
│   │   ├── main.rs                # `x0ne-orchestrator` headless HTTP/SSE binary
│   │   ├── mcp_server.rs          # MCP gateway, proxifies runtime + native sysaicalls
│   │   ├── tui_mode.rs            # TUI frontend (ratatui)
│   │   ├── matrix_mode.rs         # Matrix mode (LLM-controlled canvas)
│   │   ├── agent_loop.rs          # LLM → parse → dispatch → collapse cycle
│   │   │                          # Orchestrator-first Pipeline interception
│   │   ├── loop_tracker.rs        # T11 repeated-action circuit breaker
│   │   ├── sysaicalls.rs          # SpawnWorld / WaitAgent / ListAgents dispatch
│   │   ├── incus_actions.rs       # Host-side Incus dispatch (privilege boundary)
│   │   ├── subagent.rs            # Parallel isolated agent tasks
│   │   ├── session.rs             # JSONL audit logger
│   │   ├── terminal.rs            # Display formatting, /history renderer
│   │   ├── project.rs             # Project detection, X0NE.md, @file, indexing
│   │   ├── slash.rs               # Slash commands (/commit, /export, /matrix…)
│   │   ├── config.rs              # Config management (TOML + env)
│   │   └── bridge.rs              # stdio/HTTP/sandbox bridge
│   └── x0ne-orchestrator.toml
└── x0ne-code/                      # `x0ne` CLI binary, wraps the orchestrator library
    └── src/
        └── main.rs                # `x0ne` entry point (CLI/TUI/Matrix, -p, --auto, --stdin)

~/.x0ne/                       # User data directory
├── sessions/                  # Auto JSONL audit logs (per session)
│   └── 2026-04-16_02-12.jsonl
├── exports/                   # Manual JSON exports (/export)
│   └── session_2026-04-16.json
├── history                    # readline history
└── config/                    # User overrides
```
