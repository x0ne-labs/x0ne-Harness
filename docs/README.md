# Documentation

Every document of x0ne Harness, by subject. The [README](../README.md) is the front page.

## Use it

| Document | What it covers |
|---|---|
| [install.md](install.md) | install from source or from a release, first run, the undo demo, the optional container sandbox, Incus setup, make targets |
| [tui.md](tui.md) | the terminal agent: panels, keyboard, slash commands, plan mode, hooks, shell mode, keybindings |
| [modes.md](modes.md) | non-interactive `-p`, autonomous mode, Matrix mode, `X0NE.md` project memory |
| [mcp.md](mcp.md) | the MCP server and the gateway: Claude Code, Codex, Claude Desktop, Cursor, Windsurf, the HTTP bridge |
| [mcp-claude-code.md](mcp-claude-code.md) | plugging the runtime into Claude Code, step by step |
| [agent-hooks.md](agent-hooks.md) | `x0ne hook pretooluse`: the host agent's native tools under the x0ne policy |
| [hooks.md](hooks.md) | `[hooks]` in the user configuration: PreToolUse, PostToolUse, SessionStart, Stop |
| [connectors.md](connectors.md) | third-party MCP servers offered to the model as `<connector>.<tool>` |
| [fleet.md](fleet.md) | `SshExec`, a native runtime on a remote host, the host inventory |
| [desktop.md](desktop.md) | desktop automation on Wayland/Sway |
| [browser-tools.md](browser-tools.md) | the isolated browser primitive (`x0ne-browser`) |
| [web-search.md](web-search.md) | `WebSearch` backends and privacy, `FetchUrl` |

## What it guarantees

| Document | What it covers |
|---|---|
| [policy.md](policy.md) | the policy file, trust roots, safety rails, the session journal, hardening |
| [security.md](security.md) | the threat model, policy engine internals, bubblewrap, known limitations |
| [sandbox.md](sandbox.md) | the Podman and Incus container backends |
| [../SECURITY.md](../SECURITY.md) | how to report a vulnerability, known advisories |

## How it works

| Document | What it covers |
|---|---|
| [primitives.md](primitives.md) | the action families with examples, Parallel Worlds, multimodal, scripted functions |
| [actions.md](actions.md) | the generated reference of the 101 typed actions |
| [architecture.md](architecture.md) | crates, bridge, agent loop, handles, snapshots, policy evaluation, Parallel Worlds & snapshot architecture |
| [training.md](training.md) | datasets from the session journals, SFT/DPO exports, eval |

## Contribute

| Document | What it covers |
|---|---|
| [development.md](development.md) | the test suite, scenario categories, project layout |
| [BRANCHING.md](BRANCHING.md) | branches, commits, releases |
| [../CONTRIBUTING.md](../CONTRIBUTING.md) | what a change must pass before review |
| [../GOVERNANCE.md](../GOVERNANCE.md) | who decides and how a change gets in |
| [../CHANGELOG.md](../CHANGELOG.md) | what each version changed |

## Crate documentation

[x0ne-runtime](../x0ne-runtime/README.md), [x0ne-orchestrator](../x0ne-orchestrator/README.md),
[x0ne-browser](../x0ne-browser/README.md), [x0ne-vm](../x0ne-vm/README.md),
[the playground](../playground/README.md).
