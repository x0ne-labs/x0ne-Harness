# x0ne-vm

> **X0NE-VM** — a Rust crate + MCP server that gives Claude Code (or any MCP
> client) hands-on control of a real QEMU/KVM virtual machine: screenshot,
> click, type, scroll, snapshot, lifecycle. **The agent's body is a VM.**

```
              ┌──── LLM ────┐  (Claude, Mistral, Ollama, …)
              │
              │ MCP / stdio
              ▼
   ┌─────────────────────────────────────────────────┐
   │ Option A — direct      OR    Option B — gateway │
   │                                                 │
   │ ┌──────────────┐             ┌──────────────┐  │
   │ │ x0ne-vm-mcp  │             │ x0ne-orch    │  │
   │ │ 23 vm_* tools│             │ -estrator    │  │
   │ └──────┬───────┘             │   --mcp      │  │
   │        │                     │ 45 tools     │  │
   │        ▼                     │ (22 native + │  │
   │  virsh + RFB + OCR           │  23 vm_*)    │  │
   │        │                     └──────┬───────┘  │
   │        ▼                            │          │
   │ ┌──────────────┐                    ▼          │
   │ │   QEMU/KVM   │             ChildMcpClient    │
   │ │   live VM    │             spawns x0ne-vm    │
   │ └──────────────┘                               │
   └─────────────────────────────────────────────────┘
```

## Why

LLM agents that drive computers already exist (Anthropic's `computer_*` tool).
But they typically run on the *host* — letting Claude touch your real desktop is
fine for demos and risky for autonomy. X0NE-VM flips the model: **the agent's
body is a VM**, snapshotable in 2 seconds, isolated by KVM, parallelizable by
cloning. Everything Claude does happens *inside* a sandbox you can rewind.

## What's in the box

- **`x0ne-vm-mcp`** — MCP stdio server exposing **23 tools** (full parity with
  Anthropic `computer_20250124` + 10 VM-lifecycle/snapshot tools that the
  host-only spec doesn't have).
- **`VmSession` trait** — clean async backend abstraction.
- **`LibvirtSession`** — virsh + RFB (VNC) input + OCR via tesseract.
- **`WaylandSession`** — sway-headless + grim + wlrctl (subprocess).
- **CLI wrapper** — `scripts/x0ne-vm <subcommand>` for human use.
- **Live viewer** — browser-based, polled screenshots, synthetic cursor
  overlay (PIL crosshair) since QEMU's hw cursor is separated from the
  framebuffer.
- **Orchestrator gateway integration** — set `X0NE_VM_DOWNSTREAM=1` and
  `vm_*` tools appear next to `x0ne_*` ones in `x0ne-orchestrator --mcp`.

## Tool catalog (23 MCP tools)

| Category | Tools |
|---|---|
| **Lifecycle** | `vm_list`, `vm_info`, `vm_start`, `vm_shutdown`, `vm_force_off`, `vm_reboot` |
| **Display** | `vm_screenshot` (returns image content) |
| **Mouse** | `vm_click` (left/right/middle, double), `vm_triple_click`, `vm_move`, `vm_drag`, `vm_scroll`, `vm_cursor_position` |
| **Keyboard** | `vm_type` (text), `vm_key` (chords: `ctrl+alt+t`, `return`, `shift+f10`…) |
| **Vision (OCR)** | `vm_find_text`, `vm_click_text`, `vm_wait_for_text` (requires `tesseract`) |
| **Snapshots** | `vm_snapshot_create`, `vm_snapshot_restore`, `vm_snapshot_list`, `vm_snapshot_delete` |
| **Timing** | `vm_wait` (sleep N ms) |

13/13 of Anthropic's `computer_20250124` actions covered, plus 10 VM ops their host-only tool can't do.

## Architecture in 4 lines

- **Lifecycle + screenshots** go through `virsh` (subprocess — zero
  build-time dep on libvirt-dev).
- **Input injection** uses a minimal RFB v3.8 client (~250 LOC) sending
  PointerEvent / KeyEvent. We don't decode framebuffer updates.
- **OCR** shells out to `tesseract -c tessedit_create_tsv=1` and parses the
  TSV. Optional — gracefully skipped when tesseract isn't installed.
- **MCP tools** are mapped to Anthropic `computer_*` semantics so any
  multimodal LLM picks them up naturally.

## Prerequisites

- Linux with `qemu-kvm` + `libvirt-daemon` + `virsh` (≥ 7.0). Tested on Debian 14.
- KVM acceleration is optional — TCG fallback works (~5× slower).
- Optional: `tesseract-ocr` + `tesseract-ocr-eng` for `vm_*_text` tools.
- Run `bash scripts/x0ne-vm-doctor.sh` for an exhaustive system check.

## Quickstart — from zero to a live agent body

```bash
# 1. Build
cargo build -p x0ne-vm --release

# 2. Provision a tiny test VM (Alpine 256MB, ~50MB ISO, ~3min total)
bash scripts/x0ne-vm-setup.sh

# 3. Or a full Debian + Xorg + Firefox VM (~5min apt install via cloud-init)
bash scripts/x0ne-vm-firefox-setup.sh

# 4. Drive it via the friendly CLI:
bash scripts/x0ne-vm vms          # list VMs
bash scripts/x0ne-vm shot         # screenshot + xdg-open
bash scripts/x0ne-vm click 460 590
bash scripts/x0ne-vm type "hello"

# 5. Browser-based live view (cursor overlay included):
bash scripts/x0ne-vm-watch.sh     # opens http://localhost:7841/

# 6. One-command session bootstrap (golden snapshot revert, ~2s):
bash scripts/x0ne-vm-session

# 7. System audit:
bash scripts/x0ne-vm-doctor.sh
```

## Wire into Claude Code (option A — direct)

`.mcp.json`:
```jsonc
{
  "mcpServers": {
    "x0ne-vm": { "command": "x0ne-vm-mcp" }
  }
}
```

x0ne-vm is outside the default build: `cargo build` and `make install` skip
it. Once `make install-vm` has put `x0ne-vm-mcp` in `~/.cargo/bin`, Claude
Code sees the 23 `vm_*` tools and can drive a libvirt VM.

## Wire into the X0NE toolbelt (option B — unified gateway)

When you want **one** MCP server that exposes both the 22 native
`x0ne_*` tools (goals, hosts, artifacts, …) AND the 23 `vm_*` tools:

```bash
X0NE_VM_DOWNSTREAM=1 x0ne-orchestrator --mcp
```

`x0ne-orchestrator` will spawn `x0ne-vm-mcp` as a child MCP server,
aggregate the tool lists, and route `tools/call` by name prefix. The LLM
sees one unified toolbelt with 45 tools and can interleave shell ops with VM ops in the same session.

```jsonc
"x0ne": {
  "command": "x0ne-orchestrator",
  "args": ["--mcp"],
  "env": { "X0NE_VM_DOWNSTREAM": "1" }
}
```

## Live viewer — see what the agent sees

`virt-viewer` competes with `x0ne-vm-mcp` on the VNC handshake (QEMU
serializes ServerInit by default). The x0ne-vm watcher solves this by
polling `vm_screenshot` and serving the PNG over HTTP — no VNC contention.
And because QEMU separates the hw cursor from the framebuffer, we overlay
a synthetic crosshair using PIL at the position recorded by the last
`vm_move`/`vm_click`:

```bash
VM_NAME=x0ne-firefox bash scripts/x0ne-vm-watch.sh
# → http://localhost:7842/  auto-refreshes every 500-1500ms
```

`LibvirtSession::remember_cursor()` persists every mouse op to
`~/.x0ne/vm/cursor-<vm>.json`. The watcher reads that and draws a red
crosshair on the served PNG so the cursor is finally visible.

**Want hardware-cursor-included view instead?** Use `virt-viewer` (but
expect occasional handshake conflicts with the agent's input path). For
true concurrent access, the VM is started with `sharePolicy='force-shared'`
on its `<graphics>` element — see the `Pre-flight` section of
`x0ne-vm-firefox-setup.sh`.

## Tests

```bash
cargo test -p x0ne-vm                      # 28 unit + 2 integration tests
bash scripts/x0ne-vm-control-test.sh       # 24 cross-verified assertions on live VM
bash scripts/x0ne-vm-test-advanced.sh      # 21-step stress test
bash scripts/x0ne-vm-agent-scenario.sh     # narrated agent loop + composite image
```

A typical kitchen-sink run (all tests + control + gateway integration) reports
~80 assertions in ~30 seconds.

## State persistence

- `~/.x0ne/vm/iso/`                — base ISOs (Alpine, Debian cloud images)
- `~/.x0ne/vm/disks/`              — qcow2 disks
- `~/.x0ne/vm/cidata/<vm>/`        — cloud-init user-data (when used)
- `~/.x0ne/vm/cursor-<vm>.json`    — last cursor coords (read by watcher overlay)
- `~/.x0ne/wayland/<session>/`     — WaylandSession state (sway pid, socket)

## Security posture

- All loopback by default: `vnc listen=127.0.0.1`, watcher HTTP `--bind 127.0.0.1`.
- Libvirt via `qemu:///session` — VMs run as the user, never root.
- Network on Firefox VM is SLIRP NAT (`--network user`) — outbound only.
- Alpine test VM has `--network none` for full isolation.
- VNC auth is `None` — acceptable for loopback, **must** add `passwd=...`
  before exposing to LAN.
- Cloud-init user-data files may contain plaintext passwords — keep
  `~/.x0ne/vm/cidata/` user-only.

## Roadmap

- **v1**: native libwayland Rust client (drop the subprocess hop on Wayland
  backend), VNC password auth, ISO checksum verification.
- **v2**: WaylandSession overlayfs snapshots (`vm_snapshot_*` for the
  lightweight Linux-app backend).
- **v3**: SPICE display + audio + clipboard + USB redirection.
- **v4**: multi-VM pool with linked clones for parallel agents from a
  shared base snapshot.

## License

Apache-2.0
