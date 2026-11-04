//! X0NE-VM: libvirt + VNC backed VM agent.
//!
//! This crate exposes a clean trait [`VmSession`] for driving a QEMU/KVM
//! virtual machine (lifecycle + screenshot + input), plus a default
//! implementation [`virsh::LibvirtSession`] that shells out to `virsh`.
//!
//! The companion binary `x0ne-vm-mcp` (in `src/bin/mcp.rs`) wraps this in
//! an MCP stdio server so Claude Code — or any MCP client — can pilot a VM
//! with `vm_screenshot`, `vm_click`, `vm_type`, `vm_snapshot_*`, etc.

pub mod keymap;
pub mod ocr;
pub mod pool;
pub mod session;
pub mod virsh;
pub mod vnc;
pub mod wayland;

pub use session::{Button, KeyChord, MouseButton, Screenshot, VmInfo, VmSession, VmState};
pub use virsh::LibvirtSession;
pub use wayland::WaylandSession;
