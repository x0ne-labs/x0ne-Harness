# Agent hooks: native tools under the x0ne policy

An agent driven through x0ne's MCP tools is gated by the runtime. The same
agent also has its own tools — Claude Code's `Bash`, `Edit`, `Write`; Codex's
shell and `apply_patch` — and without a hook those reach the host with no
policy at all. `x0ne hook pretooluse` closes that door: each native gesture is
judged by the same `x0ne-runtime.toml` before it runs.

## Wire it

Claude Code, in `~/.claude/settings.json` (or a project's `.claude/settings.json`):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "^(Bash|Edit|Write|MultiEdit|NotebookEdit|Read|Grep|Glob)$",
        "hooks": [
          { "type": "command", "command": "x0ne hook pretooluse", "onFailure": "block", "timeout": 30 }
        ]
      }
    ]
  }
}
```

`onFailure: "block"` (Claude Code 2.1.295 or later) is what makes a missing
binary, a crash or a timeout block the call; without it Claude Code lets the
call through on any hook failure ([hooks reference, "Block the action when a
hook fails"](https://code.claude.com/docs/en/hooks#block-the-action-when-a-hook-fails)).
To make the hook impossible to switch off from the project or the user
settings, put it in managed settings (`/etc/claude-code/managed-settings.json`):
`disableAllHooks` outside managed settings cannot disable a managed hook.

Codex, in `~/.codex/hooks.json`
([Codex hooks](https://learn.chatgpt.com/docs/hooks)):

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "^(Bash|apply_patch)$",
        "hooks": [ { "type": "command", "command": "/opt/x0ne/scripts/hooks/x0ne-codex-pretooluse.sh", "timeout": 30 } ] }
    ]
  }
}
```

Codex has no `onFailure`, so it must not call `x0ne hook` directly: a missing
binary, a crash or a hang would then let the tool call through with no policy
(measured 2026-10-16: an absent binary exits 127 and emits no decision). Wire
it at `scripts/hooks/x0ne-codex-pretooluse.sh` instead — the fail-closed
envelope that denies, in Codex's format, whenever x0ne cannot render a verdict,
and forwards x0ne's own allow or deny otherwise. It reads `X0NE_BIN` (default
`x0ne`, found on `PATH`) and `X0NE_HOOK_TIMEOUT` (default 25s, under Codex's).

Options passed to `x0ne hook pretooluse`: `--policy FILE` (default: the
runtime's own search, `$XDG_CONFIG_HOME/x0ne/x0ne-runtime.toml`), `--agent
claude-code|codex`. The envelope already sets `--agent codex`.

## What it decides

| Gesture | Judged as |
|---|---|
| `Bash` line (Claude Code), shell command (Codex, string or argv) | one `RunCommand` per program, `WriteFile`/`ReadFile` per redirection; the start directory and every path argument against the deny patterns and the read+write roots |
| `Write` | `WriteFile` |
| `Edit`, `MultiEdit`, `NotebookEdit`, `apply_patch` Update | `EditFile` |
| `apply_patch` Add, Delete, Move | `WriteFile` |
| `Read` | `ReadFile` |
| `Grep`, `Glob` | `SearchCode` on `path` (default: cwd), and on the fixed directory of a pattern that is absolute or holds `..` |
| any other tool | refused (`UnknownTool`): narrow the matcher |

The lists (`[actions]`, `allowed_commands`, roots, `deny_path_patterns`) are
applied first. Then `mode`: `readonly` refuses a modifying gesture, `confirm`
answers `ask` (Claude Code shows its prompt), `auto` answers `allow`. A refusal
by the lists is never softened into a question. Codex cannot ask (it parses
`ask` but does not support it): a question becomes a refusal there.

A Bash line is read, never run. Variables are given the value the line sets
(`F=/etc/passwd; cat $F` is judged as `cat /etc/passwd`) or, failing that,
the agent's inherited environment (`$HOME`); `$'\x72m'` is decoded. A program
name or a program argument whose value is still unknown is refused. A glob
argument is judged by its fixed directory and by every path it matches at
decision time.

No policy, an empty or invalid policy file, an unreadable payload, a panic:
the hook prints a `deny` and exits 2, which both agents treat as a block.

Files that would switch the hook off or rewrite the policy are refused to the
agent whatever the policy says: the policy file and `$XDG_CONFIG_HOME/x0ne/`,
the `x0ne` binary itself, `.claude/settings.json`, `.claude/settings.local.json`,
`.claude/hooks/`, `.codex/config.toml`, `.codex/hooks.json`, `.codex/hooks/`,
`/etc/claude-code/`, `/etc/codex/`.

## Journal

Each decision is one entry in the system journal (native protocol,
`/run/systemd/journal/socket`), the source `x0ne-hardening watch` reads:

| Field | Value |
|---|---|
| `SYSLOG_IDENTIFIER` | `x0ne-harness` |
| `X0NE_EVENT` | `harness` |
| `X0NE_OUTCOME` | `allowed`, `asked`, `refused` |
| `PRIORITY` | 6, 5, 4 |
| `X0NE_RULE` | the `DenyReason` (`CommandNotAllowed`, `PathNotInAllowlist`, `PathMatchesDenyPattern`, `ActionDisabled`, `DefaultDeny`, `ReadonlyMode`…), or a hook rule (`CommandSubstitution`, `ProcessSubstitution`, `ParameterExpansion`, `CommandFromVariable`, `PathFromVariable`, `ShellScript`, `ShellSyntax`, `UnknownDirectory`, `GlobTooWide`, `EnvironmentVariable`, `ArgumentOutsideRoots`, `ProtectedPath`, `UnknownTool`, `NoPolicy`, `BadInput`), `Allow`, `ConfirmMode` |
| `X0NE_REASON` | the text the agent or the human is shown |
| `X0NE_ACTION` | the policy action judged (`RunCommand`, `WriteFile`…) |
| `X0NE_TARGET` | the program and its arguments, or the path (2 KiB max) |
| `X0NE_TOOL`, `X0NE_AGENT` | `Bash`…, `claude-code` or `codex` |
| `X0NE_SESSION`, `X0NE_TOOL_USE_ID`, `X0NE_SUBAGENT`, `X0NE_CWD` | from the payload, `-` if absent |
| `X0NE_POLICY` | the policy file, `none` without one |
| `X0NE_TIME` | RFC 3339 |

`journalctl -t x0ne-harness -o verbose` shows them. Without a journal the
decision stands and a line goes to stderr.

## Proved by attack

`x0ne-code/tests/pretooluse_hook_attacks.rs` sends the payloads Claude Code
and Codex send to the real binary, with a fake key in `~/.ssh`. Refused: a
program outside `allowed_commands`, alone or behind `bash -c`, `sh -c`, `env`,
`xargs`, `find -exec`, `exec`, `command`, a program path (`/bin/rm`); a
program or a path behind a variable (`X=rm; $X`, `F=/etc/passwd; cat $F`,
`$'\x2fetc/passwd'`, an unset `$X`); `$(…)`, backticks, an unquoted
here-document holding `$(…)`; `LD_PRELOAD=`, `PATH=`, `export BASH_ENV=`;
`eval`; a shell reading a script or stdin (`bash x.sh`, `… | bash`,
`bash <<EOF`); a write or redirection outside the write roots, written
relative, with `..`, through a link to a directory or to a file, or after
`cd ..`; a secret named in an argument (`~/.ssh/id_rsa`, `$HOME/…`, `$X/.ssh/…`,
a link to it, a glob matching it); a path argument outside the roots
(`cat /etc/passwd`, `git -C ../..`, `--git-dir=/srv/x`, a glob reaching out,
through a link); a `Glob` pattern reaching out; the hook's settings and the
policy file. Allowed: a listed program, a write inside the roots, a quoted
here-document written there, `cat *.txt` in the workspace.
Shell reader unit tests: `x0ne-code/src/hook_cmd/shell.rs`.

## Limits

- **A static reading, not a sandbox.** Commands run on the host with your
  rights; nothing confines them once allowed. An allowed program that starts
  other programs — `git` (aliases, hooks), `make`, `cargo` (build scripts),
  `npm run`, `python -c`, `ssh` — is a shell: listing it allows what it runs,
  as in the runtime's `allowed_commands`. Run the agent in a container or a VM
  (x0ne OS) for confinement.
- **Only named paths are checked.** A path computed by the program, or read
  from a file or stdin (`xargs`), is invisible. A path argument inside a
  read-only root is not told apart from a write to it. Links and globs are
  resolved at decision time: one created and used in the same line is not
  (the `ln` arguments themselves are checked).
- **The shell's own state is not seen.** Aliases and functions from your
  shell profile (Claude Code runs Bash with a snapshot of it), and variables
  a previous call left in a persistent shell, are read from this process's
  environment, not from that shell.
- **Refused rather than understood:** `$(…)`, backticks, `<(…)`, `${…}` with
  operators, an unknown variable or `$@`/`$1` in a program argument, braces
  `{a,b}`, globs in the program name, `if`/`for`/`while`/`case`, `( … )`,
  `{ … }`, functions, `eval`, `source`, `alias`, `trap`, a shell without `-c`,
  `env -S`, `sudo -s`. A legitimate line using them is refused: write it out
  as simple commands.
- **Shell builtins that start nothing** (`cd`, `echo`, `printf`, `test`, `[`,
  `export`, `set`, `read`…) are not looked up in `allowed_commands`; their
  redirections and path arguments are judged.
- **Codex** has no `onFailure`, so it is wired through
  `scripts/hooks/x0ne-codex-pretooluse.sh` (above), which denies when a missing
  binary, a crash or a timeout leaves x0ne unable to determine. Its PreToolUse
  covers shell, `apply_patch` and MCP tools, not hosted tools.
- **The hook is only as strong as its wiring:** a matcher that omits a tool
  leaves that tool ungated. In user or project settings, a human can remove
  it; the agent is refused those files through every tool the hook sees, but
  an allowed program that writes files (an interpreter, `cp`) could still
  rewrite them. Root-owned managed settings are out of its reach.
- An `allow` from the hook skips Claude Code's prompt but not your settings'
  `deny` and `ask` rules, which are still applied
  ([PreToolUse decision control](https://code.claude.com/docs/en/hooks)).
