<h1 align="center"><picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/logo-dark.svg"><img src="docs/assets/logo-light.svg" alt="x0ne Harness" width="720"></picture></h1>

<p align="center"><strong>A hardened harness for AI coding agents.</strong></p>

<p align="center">
  <a href="https://github.com/x0ne-labs/x0ne-Harness/actions/workflows/ci.yml"><img src="https://github.com/x0ne-labs/x0ne-Harness/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License: Apache-2.0"></a>
  <a href="rust-toolchain.toml"><img src="https://img.shields.io/badge/rust-1.98%2B-orange?logo=rust&amp;logoColor=white" alt="Rust 1.98+"></a>
  <a href="docs/mcp.md"><img src="https://img.shields.io/badge/MCP-Claude%20Code%20%C2%B7%20Codex-555" alt="MCP: Claude Code, Codex"></a>
  <a href="#status"><img src="https://img.shields.io/badge/status-preview%200.2.0-yellow" alt="Status: preview 0.2.0"></a>
</p>

<p align="center">
  <a href="https://x0ne.co/harness">Website</a> ·
  <a href="docs/README.md">Documentation</a> ·
  <a href="SECURITY.md">Security</a> ·
  <a href="CHANGELOG.md">Changelog</a>
</p>

[![x0ne Harness terminal UI](docs/assets/tui.gif)](docs/tui.md)

<p align="center"><sub>The terminal agent starting on a demo project: a file read, a read of
<code>.env</code> refused by the policy, and the approval prompt before the tests run.
The model's replies and the RAM and load in the status bar were scripted for this capture (<a href="docs/assets/tui.tape">recording script</a>).</sub></p>

x0ne Harness puts a safety layer between an AI agent and your machine. Every
action is typed and checked against a policy before it runs, files are
snapshotted before they change so a mistake can be undone, and the agent's
actions are written to a journal. Use it as its own terminal agent, or plug it
into Claude Code, Codex and other MCP clients.

## Highlights

- **Simulate before you run.** Dry-run any action, or a whole plan, through the
  policy engine without writing anything.
- **Undo.** Before each file write, the agent loop snapshots the files it
  targets: copy-on-write on btrfs and APFS, a copy elsewhere. `/undo` restores
  the last point, file by file.
- **A policy on every call.** `x0ne-runtime` exposes 101 typed actions, each one
  gated. On Linux, agent commands run under bubblewrap; where it cannot start,
  they are refused, not run unconfined.
- **Works with your agent, or alone.** `make claude-add` registers it in Claude
  Code; Codex and other MCP clients connect the same way. Its own agent runs
  with any OpenAI-compatible provider, Ollama, or [x0ne Engine](https://github.com/x0ne-labs/x0ne-Engine)
  for confidential inference.
- **More isolation when you want it.** An optional container sandbox (Podman
  or Incus), and a QEMU/KVM body outside the default build.
- **An audit trail.** Each session writes a JSONL journal of the agent's
  actions.

## Requirements

On Linux, agent commands run in a bubblewrap sandbox with Landlock inside it.
When the machine cannot build that sandbox, they are refused, not run
unconfined. It needs:

- **bubblewrap** (`bwrap`) in `/usr/bin`, `/usr/local/bin` or `/bin`. It is
  never looked up in `PATH`.
- **Unprivileged user namespaces**, which bubblewrap uses. Some distributions
  restrict them with a sysctl or an AppArmor policy. The runtime checks once,
  at start, with
  `bwrap --unshare-all --die-with-parent --ro-bind / / --dev /dev --proc /proc -- true`;
  if that fails, its error is the reason given.
- **Landlock**: Linux 5.13 or later, with `landlock` listed in
  `/sys/kernel/security/lsm`. The runtime asks for the rules of Landlock ABI 6
  (Linux 6.12) and applies those an older kernel supports; on a kernel without
  Landlock, agent commands are refused.

macOS has neither bubblewrap nor Landlock: there, the default isolation
refuses every agent command. `--isolation none`, typed by you, runs them on
the host, unconfined.

When the runtime itself runs inside a Podman or Incus container,
`--isolation container` takes the place of bubblewrap: see
[docs/sandbox.md](docs/sandbox.md) and [docs/security.md](docs/security.md).
To build from source you also need Rust 1.98 or later (rustup installs the
version pinned in `rust-toolchain.toml`), a C compiler, make and git.

## Quick start

```bash
git clone https://github.com/x0ne-labs/x0ne-Harness && cd x0ne-Harness
make install      # builds and installs x0ne, x0ne-runtime and x0ne-orchestrator
x0ne              # the first run opens the setup wizard: model and provider
```

The build needs network access to crates.io and to the
[x0ne-Engine](https://github.com/x0ne-labs/x0ne-Engine) repository, which
provides one crate. No crate is published on crates.io. Each GitHub release,
from v0.2.0, carries prebuilt binaries: `install.sh` downloads the latest one
and verifies it ([docs/install.md](docs/install.md#from-a-release)).

To use it from Claude Code instead:

```bash
make claude-add   # registers the runtime and the gateway as MCP servers
```

Then try the undo demo, which needs no model: `make demo-undo-fast`.
[docs/install.md](docs/install.md) covers macOS, the container sandbox and
uninstalling.

## Status

> [!IMPORTANT]
> **Preview (0.2.0).** Usable today; defaults and interfaces may change before
> 1.0. Known limits and fixed issues are listed in [SECURITY.md](SECURITY.md).

## Documentation

| Read | For |
|---|---|
| [docs/install.md](docs/install.md) | install, first run, the container sandbox, Incus, make targets |
| [docs/tui.md](docs/tui.md) | the terminal agent: panels, keys, slash commands, plan mode |
| [docs/modes.md](docs/modes.md) | non-interactive, autonomous and Matrix modes, `X0NE.md` |
| [docs/mcp.md](docs/mcp.md) | Claude Code, Codex, Claude Desktop, Cursor, Windsurf |
| [docs/primitives.md](docs/primitives.md) | the action families, Parallel Worlds, scripted functions |
| [docs/actions.md](docs/actions.md) | the generated reference of the typed actions |
| [docs/policy.md](docs/policy.md) | the policy, trust roots, safety rails, the session journal |
| [docs/sandbox.md](docs/sandbox.md) | the Podman and Incus container backends |
| [docs/architecture.md](docs/architecture.md) | how the runtime, the orchestrator and the CLI fit together |
| [docs/security.md](docs/security.md) | the threat model and its limits |
| [docs/fleet.md](docs/fleet.md) | remote hosts over SSH, the host inventory |
| [docs/training.md](docs/training.md) | datasets from the session journals |
| [docs/development.md](docs/development.md) | the test suite, the project layout |

The full index is [docs/README.md](docs/README.md).

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md) says what a change must pass before review.

## Security

Report a vulnerability privately, as [SECURITY.md](SECURITY.md) explains: we
answer within 72 hours. Please do not open a public issue for it.

## License

Apache-2.0: see [LICENSE](LICENSE) and [NOTICE](NOTICE).
