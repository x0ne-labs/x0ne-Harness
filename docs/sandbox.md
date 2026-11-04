# Container Sandbox

X0NE supports **two container backends** that can run **simultaneously**. Each has its own image allowlist, resource limits, and configuration section.

| | Podman | Incus |
|---|---|---|
| **Actions** | `RunInSandbox`, `RunContainerGroup` | `RunIncus`, `RunIncusGroup` |
| **Config** | `[sandbox.podman]` | `[sandbox.incus]` |
| **Platforms** | Linux, macOS (via `podman machine`) | Linux only (LXD/LXC) |
| **Daemon** | Daemonless, rootless | Requires `incusd` |
| **Snapshots** | No (ephemeral only) | btrfs CoW snapshots (<200ms) |
| **Best for** | OCI containers, CI/CD, macOS dev | System containers, GPU passthrough, time travel |

Both backends enforce a **fail-closed image allowlist** — if the list is empty, all images are denied.

---

## Podman backend

### Requirements

```bash
# Linux
sudo apt install podman    # or dnf/pacman equivalent

# macOS
brew install podman
podman machine init && podman machine start
```

### Configuration

```toml
[actions]
RunInSandbox = true
RunContainerGroup = true

[sandbox.podman]
allowed_images = ["alpine", "python:3.12-slim", "node:20-slim"]
default_memory_mb = 256
max_memory_mb = 1024
default_cpu_limit = 2
default_timeout_secs = 60
max_timeout_secs = 300
default_network = false
security_profile = "seccomp"       # seccomp | apparmor | none
no_new_privileges = true
max_parallel_instances = 4
```

### Single container — `RunInSandbox`

```json
{
  "type": "RunInSandbox",
  "image": "python:3.12-slim",
  "command": ["python", "main.py"],
  "files": {"main.py": "print('hello')"},
  "collect_files": ["/tmp/output.txt"],
  "memory_mb": 256,
  "cpu_limit": 2,
  "timeout_secs": 30,
  "network": false
}
```

Features:
- **File injection** — `files` map is written to a temp dir and bind-mounted
- **File collection** — `collect_files` extracts files via `podman cp` after execution
- **Resource capping** — `memory_mb` and `cpu_limit` enforced, capped to config maximums
- **Security** — `--cap-drop=ALL`, `--no-new-privileges`, env var key validation
- **RAII cleanup** — `ContainerGuard` ensures force-removal even on panic/timeout

### Parallel containers — `RunContainerGroup`

```json
{
  "type": "RunContainerGroup",
  "tasks": [
    {"id": "test-py", "image": "python:3.12-slim", "command": ["python", "-c", "print('ok')"]},
    {"id": "test-sh", "image": "alpine", "command": ["sh", "-c", "echo ok"]},
    {"id": "lint",    "image": "node:20-slim", "command": ["npx", "eslint", "."]}
  ],
  "memory_mb": 256,
  "timeout_secs": 60,
  "max_parallel": 4
}
```

Concurrency is controlled by a `tokio::Semaphore` — at most `max_parallel` containers run simultaneously.

---

## Incus backend

For full Incus setup (local + remote), see the [Container Isolation section of docs/install.md](install.md#container-isolation-incus-setup).

### Quick setup

```bash
sudo apt install incus
sudo adduser $USER incus-admin && newgrp incus-admin
incus admin init --minimal
```

### Configuration

```toml
[actions]
RunIncus = true
RunIncusGroup = true

[sandbox.incus]
allowed_images = ["ubuntu:22.04", "debian/12"]
remote = "prod-infra"
default_memory_mb = 512
```

### Sandbox mode (`make run-incus`)

Even when x0ne-runtime runs inside a sandbox container, both backends work. The orchestrator maintains a **secondary host-side bridge** for Incus:

```
Orchestrator (host)
├── Primary bridge ──→ sandbox container (ReadFile, RunCommand, RunInSandbox…)
└── Incus bridge ────→ local x0ne-runtime → incus launch → ephemeral containers
```

---

## Using both together

Both backends are independent — enable whichever you need:

```toml
[actions]
# Podman (OCI containers)
RunInSandbox = true
RunContainerGroup = true

# Incus (system containers)
RunIncus = true
RunIncusGroup = true

[sandbox.podman]
allowed_images = ["alpine", "python:3.12-slim"]

[sandbox.incus]
allowed_images = ["ubuntu:22.04"]
remote = "prod-infra"
```

The LLM can use either backend depending on the task — OCI images via Podman, full system containers via Incus. There is no conflict.

## Troubleshooting

### Podman

| Issue | Fix |
|-------|-----|
| `image not allowed` | Add the image to `sandbox.podman.allowed_images` |
| `action disabled` | Set `RunInSandbox=true` or `RunContainerGroup=true` in `[actions]` |
| `podman: command not found` | Install podman (`apt install podman` / `brew install podman`) |
| macOS: `Cannot connect to Podman` | Run `podman machine start` |
| Zombie containers | Should not happen (RAII guard). Manual cleanup: `podman rm -f $(podman ps -aq)` |

### Incus

| Issue | Fix |
|-------|-----|
| glibc mismatch | Use `make sync-incus-musl` for cross-compilation |
| Remote unreachable | Check `incus remote list` and TLS cert trust |
| Slow container creation | Use btrfs for CoW snapshots (<200ms) |
