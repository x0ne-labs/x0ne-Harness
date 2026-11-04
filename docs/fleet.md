# Fleet admin from the CLI

Two complementary modes. The same agent (Claude Code, Codex, or `x0ne` itself) can use both, choosing per-host.

## Mode "adhoc" — `SshExec` typed primitive

Zero install on the target. Just `ssh user@host -- cmd` typed:

```jsonc
{ "type": "SshExec",
  "host": "user@hv-example-01",
  "command": "virsh",
  "args": ["list", "--all"],
  "timeout_secs": 10 }
// returns: { command, args, exit_code, stdout, stderr, timed_out }
```

- Inherits your `~/.ssh/config` — ProxyJump, IdentityFile, etc. work.
- `BatchMode=yes` and `StrictHostKeyChecking=accept-new` forced by default (no interactive deadlock; first connection auto-trusts host key).
- Shell metacharacters in `command` rejected (use `command:"sh", args:["-c","<pipeline>"]` for pipes).
- **Default-deny in policy** — opt in via `[actions] SshExec = true` or per-host gate (planned: `confirm` for prod tags).

## Mode "native" — deploy `x0ne-runtime` on the host

Same wire format, but every action lands typed + policy-checked + audited **on the remote box**. If your laptop is compromised, the remote runtime still refuses anything outside its local policy. Defense in depth.

```bash
# Build a glibc-2.35 musl-static binary and push to the host
make sync-incus-musl     # in-cluster Incus example
```

The orchestrator routes via `bridge.mode = "sandbox"` with `sandbox_cmd = "ssh <target> -- /opt/x0ne-runtime --mode mcp"`. From there, every primitive (Read*, Git*, RunCommand whitelist, snapshots, Try blocks, the works) executes inside the host's policy.

## Host inventory — `~/.x0ne/hosts.toml`

```toml
[[hosts]]
name = "hv-example-01"
target = "root@192.0.2.13"
mode = "native"                       # adhoc | native
runtime_path = "/opt/x0ne-runtime"
policy_profile = "readonly"           # template kept on the host
tags = ["hypervisor", "libvirt", "dc1"]
description = "DC1 hypervisor, KVM/QEMU, ZFS pool tank"

[[hosts]]
name = "vm-api-01"
target = "user@192.0.2.42"
mode = "adhoc"
tags = ["api", "dc1", "vm"]
ssh_options = ["Port=2222", "ConnectTimeout=5"]   # keys of the SshExec allowlist only
```

Six native MCP tools manage it (visible to Claude Code as `mcp__x0ne-gateway__x0ne_host_*`):

- `x0ne_host_list [tag]` — list all (filter by tag)
- `x0ne_host_get name` — full record
- `x0ne_host_add { name, target, mode, tags, ... }` — upsert
- `x0ne_host_remove name` — drop entry, clear active pointer if needed
- `x0ne_host_use [name]` — set/clear active host
- `x0ne_host_active` — current active host

`x0ne_host_add`, `x0ne_host_remove` and `x0ne_host_use` go through the gateway's human gate (elicitation) like every side-effecting call; a client that cannot ask gets `human_gate_refused`. A `target` starting with `-` or an `ssh_options` key outside the `SshExec` allowlist is refused.

## Persistent SSH multiplexing — `make setup-ssh-aliases`

```bash
make setup-ssh-aliases
```

Writes `~/.ssh/config.d/x0ne.conf`:

```
Host *
    ControlMaster auto
    ControlPath ~/.ssh/x0ne-cm/%C
    ControlPersist 30m
    ServerAliveInterval 30
```

First SSH to a host authenticates; subsequent invocations (other terminals, follow-up `SshExec` actions) reuse the same TCP+TLS session. **Reconnects to fleet hosts become instant.** Auto-add of `Include config.d/*.conf` to `~/.ssh/config` if missing.

## Recommended workflow with Claude Code

1. `make claude-add` — registers `x0ne` (runtime) + `x0ne-gateway` (orchestrator) MCP servers in Claude Code's config.
2. `make setup-ssh-aliases` — opt into persistent SSH multiplexing.
3. In Claude Code, ask: *"register hv-example-01 as a native host on root@192.0.2.13, then list its VMs"*. Claude calls `x0ne_host_add` then `ssh_exec virsh list --all`.
4. MCP calls are not journaled by X0NE (only the `x0ne` agent loop writes `~/.x0ne/sessions/`): keep the host agent's own transcript if you need one.

## Threat model — what X0NE does NOT do

- Does not store SSH credentials. `~/.ssh/` and your agent (gpg-agent / yubikey / 1password) own that.
- Does not bypass `~/.ssh/known_hosts` checks. `accept-new` mode means: first connection trusts, subsequent ones must match. Tamper triggers a hard fail.
- Masks secrets in the session journal: private keys, `NAME=value` secrets, URL credentials and token-like words are replaced by `[redacted]`, `SshExec` output included. The masking works by pattern, so treat sessions as private (`chmod 600`).
