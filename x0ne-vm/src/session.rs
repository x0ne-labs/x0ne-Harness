//! Abstract VM session: lifecycle, display capture, input injection, snapshots.
//!
//! Implementations:
//! - [`crate::virsh::LibvirtSession`] (default, subprocess `virsh`)
//!
//! Designed to grow a native libvirt-rs impl later without changing callers.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Lifecycle state, mirroring `virsh list` states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    Running,
    Paused,
    ShutOff,
    Crashed,
    Other,
}

impl VmState {
    pub fn parse_virsh(s: &str) -> Self {
        match s.trim() {
            "running" => Self::Running,
            "paused" => Self::Paused,
            "shut off" | "shutoff" => Self::ShutOff,
            "crashed" => Self::Crashed,
            _ => Self::Other,
        }
    }
}

impl fmt::Display for VmState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => write!(f, "running"),
            Self::Paused => write!(f, "paused"),
            Self::ShutOff => write!(f, "shut off"),
            Self::Crashed => write!(f, "crashed"),
            Self::Other => write!(f, "other"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmInfo {
    pub name: String,
    pub uuid: Option<String>,
    pub state: VmState,
    pub vcpu: Option<u32>,
    pub max_mem_kib: Option<u64>,
    pub used_mem_kib: Option<u64>,
    /// `127.0.0.1:5901` style — `None` if the VM is off or has no VNC display.
    pub vnc_endpoint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

impl MouseButton {
    /// RFB PointerEvent button-mask bit (LSB = left = bit 0).
    pub fn mask(self) -> u8 {
        match self {
            Self::Left => 1 << 0,
            Self::Middle => 1 << 1,
            Self::Right => 1 << 2,
        }
    }
}

/// A simple key chord, e.g. `ctrl+alt+t`. Parsed by [`KeyChord::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChord {
    pub modifiers: Vec<String>,
    pub key: String,
}

impl KeyChord {
    /// Parse `ctrl+alt+t` → modifiers=[ctrl, alt], key=t.
    /// Splits on `+`; the last segment is the main key, the rest are modifiers.
    /// Each segment is lower-cased and trimmed.
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<String> = s
            .split('+')
            .map(|p| p.trim().to_ascii_lowercase())
            .filter(|p| !p.is_empty())
            .collect();
        if parts.is_empty() {
            return None;
        }
        let key = parts.last().cloned()?;
        let modifiers = parts[..parts.len() - 1].to_vec();
        Some(Self { modifiers, key })
    }
}

/// A captured screenshot in raw PNG bytes.
#[derive(Debug, Clone)]
pub struct Screenshot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Tiny enum alias used in MCP tool args.
pub type Button = MouseButton;

/// Scroll-wheel axis + direction. Maps to RFB PointerEvent button-mask bits
/// 4 (up), 5 (down), 6 (left), 7 (right).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

impl ScrollDirection {
    /// Returns the RFB button-mask bit for this scroll direction.
    pub fn mask(self) -> u8 {
        match self {
            Self::Up => 1 << 3,    // bit 3 = 0x08
            Self::Down => 1 << 4,  // bit 4 = 0x10
            Self::Left => 1 << 5,  // bit 5 = 0x20
            Self::Right => 1 << 6, // bit 6 = 0x40
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "up" | "u" => Self::Up,
            "down" | "d" => Self::Down,
            "left" | "l" => Self::Left,
            "right" | "r" => Self::Right,
            _ => return None,
        })
    }
}

#[async_trait]
pub trait VmSession: Send + Sync {
    /// List all VMs defined on the connected libvirt host.
    async fn list(&self) -> anyhow::Result<Vec<VmInfo>>;

    /// Detailed info about one VM.
    async fn info(&self, name: &str) -> anyhow::Result<VmInfo>;

    async fn start(&self, name: &str) -> anyhow::Result<()>;
    async fn shutdown(&self, name: &str) -> anyhow::Result<()>;
    async fn force_off(&self, name: &str) -> anyhow::Result<()>;
    async fn reboot(&self, name: &str) -> anyhow::Result<()>;

    /// Capture a PNG screenshot of the primary display.
    async fn screenshot(&self, name: &str) -> anyhow::Result<Screenshot>;

    async fn mouse_move(&self, name: &str, x: i32, y: i32) -> anyhow::Result<()>;
    async fn mouse_click(
        &self,
        name: &str,
        x: i32,
        y: i32,
        button: MouseButton,
        double: bool,
    ) -> anyhow::Result<()>;

    /// Triple click at (x, y) — typically used to select an entire line in
    /// text editors / Firefox URL bar. Default impl: 3× `mouse_click`.
    async fn mouse_triple_click(
        &self,
        name: &str,
        x: i32,
        y: i32,
        button: MouseButton,
    ) -> anyhow::Result<()> {
        for _ in 0..3 {
            self.mouse_click(name, x, y, button, false).await?;
        }
        Ok(())
    }
    async fn mouse_drag(
        &self,
        name: &str,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        button: MouseButton,
    ) -> anyhow::Result<()>;

    /// Scroll the mouse wheel in `direction` for `clicks` detents.
    /// Default impl: pretend it's a no-op (used by backends that lack scroll).
    async fn mouse_scroll(
        &self,
        _name: &str,
        _direction: ScrollDirection,
        _clicks: u32,
    ) -> anyhow::Result<()> {
        anyhow::bail!("mouse_scroll: not implemented for this backend")
    }

    /// Return the last-known (x, y) of the cursor as tracked by this backend.
    /// Backends that don't track cursor return `None`.
    async fn cursor_position(&self, _name: &str) -> anyhow::Result<Option<(i32, i32)>> {
        Ok(None)
    }

    /// Find a text region on screen via OCR (tesseract) and return its
    /// bounding box. Doesn't move the cursor. `occurrence` (0-indexed)
    /// disambiguates when multiple matches exist.
    ///
    /// Default impl uses `screenshot()` + `ocr::tesseract_words` so all
    /// backends inherit it for free.
    async fn find_text(
        &self,
        name: &str,
        target: &str,
        occurrence: usize,
    ) -> anyhow::Result<Option<crate::ocr::MatchedRegion>> {
        let shot = self.screenshot(name).await?;
        let words = crate::ocr::tesseract_words(&shot.png).await?;
        Ok(crate::ocr::find_match(&words, target, occurrence))
    }

    /// Find on-screen text and click its center. Returns the matched region
    /// for debug/visualization. `occurrence` disambiguates ties.
    async fn click_text(
        &self,
        name: &str,
        target: &str,
        button: MouseButton,
        double: bool,
        occurrence: usize,
    ) -> anyhow::Result<crate::ocr::MatchedRegion> {
        let region = self
            .find_text(name, target, occurrence)
            .await?
            .ok_or_else(|| anyhow::anyhow!("text not found on screen: {target:?}"))?;
        let (cx, cy) = region.center();
        self.mouse_click(name, cx, cy, button, double).await?;
        Ok(region)
    }

    /// Poll OCR every `poll_ms` until `target` appears or `timeout_ms` elapses.
    /// Useful for "wait until login form rendered" style agent steps.
    async fn wait_for_text(
        &self,
        name: &str,
        target: &str,
        timeout_ms: u64,
        poll_ms: u64,
    ) -> anyhow::Result<crate::ocr::MatchedRegion> {
        let started = std::time::Instant::now();
        let interval = std::time::Duration::from_millis(poll_ms.max(50));
        loop {
            if let Some(region) = self.find_text(name, target, 0).await? {
                return Ok(region);
            }
            if started.elapsed().as_millis() as u64 >= timeout_ms {
                anyhow::bail!("text {target:?} did not appear within {timeout_ms} ms");
            }
            tokio::time::sleep(interval).await;
        }
    }

    /// Type a UTF-8 string into the VM.
    async fn type_text(&self, name: &str, text: &str) -> anyhow::Result<()>;
    /// Send a key chord, e.g. `ctrl+alt+t` or `return`.
    async fn key_chord(&self, name: &str, chord: &KeyChord) -> anyhow::Result<()>;

    async fn snapshot_create(&self, name: &str, snap: &str) -> anyhow::Result<()>;
    async fn snapshot_restore(&self, name: &str, snap: &str) -> anyhow::Result<()>;
    async fn snapshot_list(&self, name: &str) -> anyhow::Result<Vec<String>>;
    async fn snapshot_delete(&self, name: &str, snap: &str) -> anyhow::Result<()>;

    /// Create a linked clone of `base` as `new_name`.
    /// The clone uses copy-on-write so the base disk is shared (read-only) and
    /// only the clone's writes consume space. Foundation for the agent pool —
    /// spawn N agents from one golden image in ~200ms each.
    /// Default impl returns "unsupported" — backends opt in.
    /// (Named `clone_vm` to avoid colliding with `Clone::clone_from` when this
    /// trait is used through `Arc<dyn VmSession>`.)
    async fn clone_vm(&self, _base: &str, _new_name: &str) -> anyhow::Result<()> {
        anyhow::bail!("clone_vm: not implemented for this backend")
    }

    /// Destroy a VM created via clone_from: undefine + delete its overlay disk.
    /// Safe-by-default: refuses to touch VMs whose disk is not under
    /// `~/.x0ne/vm/disks/` (so the user's hand-defined VMs are never harmed).
    async fn destroy_clone(&self, _name: &str) -> anyhow::Result<()> {
        anyhow::bail!("destroy_clone: not implemented for this backend")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_state_parses_virsh_strings() {
        assert_eq!(VmState::parse_virsh("running"), VmState::Running);
        assert_eq!(VmState::parse_virsh("  running  "), VmState::Running);
        assert_eq!(VmState::parse_virsh("shut off"), VmState::ShutOff);
        assert_eq!(VmState::parse_virsh("shutoff"), VmState::ShutOff);
        assert_eq!(VmState::parse_virsh("paused"), VmState::Paused);
        assert_eq!(VmState::parse_virsh("crashed"), VmState::Crashed);
        assert_eq!(VmState::parse_virsh("zombie"), VmState::Other);
    }

    #[test]
    fn keychord_parses_combos() {
        let kc = KeyChord::parse("ctrl+alt+t").unwrap();
        assert_eq!(kc.modifiers, vec!["ctrl", "alt"]);
        assert_eq!(kc.key, "t");

        let single = KeyChord::parse("Return").unwrap();
        assert!(single.modifiers.is_empty());
        assert_eq!(single.key, "return");

        let shifty = KeyChord::parse("SHIFT + F10").unwrap();
        assert_eq!(shifty.modifiers, vec!["shift"]);
        assert_eq!(shifty.key, "f10");

        assert!(KeyChord::parse("").is_none());
        assert!(KeyChord::parse(" + ").is_none());
    }

    #[test]
    fn mouse_button_masks_are_distinct() {
        let masks: Vec<u8> = [MouseButton::Left, MouseButton::Middle, MouseButton::Right]
            .into_iter()
            .map(|b| b.mask())
            .collect();
        assert_eq!(masks, vec![0b001, 0b010, 0b100]);
    }

    #[test]
    fn scroll_directions_have_distinct_high_bits() {
        let masks: Vec<u8> = [
            ScrollDirection::Up,
            ScrollDirection::Down,
            ScrollDirection::Left,
            ScrollDirection::Right,
        ]
        .into_iter()
        .map(|d| d.mask())
        .collect();
        assert_eq!(masks, vec![0x08, 0x10, 0x20, 0x40]);
        // Crucially, scroll bits don't collide with click bits.
        for s in &masks {
            assert_eq!(s & 0b111, 0);
        }
    }

    #[test]
    fn scroll_parses() {
        assert_eq!(ScrollDirection::parse("up"), Some(ScrollDirection::Up));
        assert_eq!(ScrollDirection::parse("Down"), Some(ScrollDirection::Down));
        assert_eq!(ScrollDirection::parse(" L "), Some(ScrollDirection::Left));
        assert_eq!(ScrollDirection::parse("nope"), None);
    }
}
