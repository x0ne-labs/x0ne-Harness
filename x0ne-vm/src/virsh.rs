//! Default [`VmSession`] backend: shells out to `virsh`.
//!
//! Zero build-time dep on libvirt — we just need the `virsh` CLI in PATH and
//! the user in the `libvirt` group (or `LIBVIRT_DEFAULT_URI=qemu:///system`
//! with sudo policy in place).
//!
//! Output parsing lives in `parse_*` free functions so they can be unit-tested
//! without spawning processes.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

use std::sync::Mutex;

use crate::keymap;
use crate::session::{
    KeyChord, MouseButton, Screenshot, ScrollDirection, VmInfo, VmSession, VmState,
};
use crate::vnc::VncInputClient;

#[derive(Debug)]
pub struct LibvirtSession {
    /// libvirt connection URI, e.g. `qemu:///system` or `qemu:///session`.
    /// `None` lets virsh pick its default (usually `qemu:///session` for users).
    pub uri: Option<String>,
    /// Last (x, y) we asked the cursor to go to, per VM name. Read-back via
    /// `cursor_position()`. Cheap and accurate because RFB has no return
    /// channel for cursor location without the cursor pseudo-encoding.
    last_cursor: Mutex<std::collections::HashMap<String, (i32, i32)>>,
}

impl LibvirtSession {
    pub fn new() -> Self {
        Self {
            uri: std::env::var("LIBVIRT_DEFAULT_URI").ok(),
            last_cursor: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn with_uri(uri: impl Into<String>) -> Self {
        Self {
            uri: Some(uri.into()),
            last_cursor: Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn remember_cursor(&self, name: &str, x: i32, y: i32) {
        // In-process cache.
        let mut m = self.last_cursor.lock().unwrap();
        m.insert(name.to_string(), (x, y));
        // Persist to disk so out-of-process consumers (the live watcher,
        // other tools) can render the cursor overlay on screenshots.
        let dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join(".x0ne")
            .join("vm");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("cursor-{name}.json"));
        let body = format!("{{\"x\":{x},\"y\":{y}}}");
        let _ = std::fs::write(path, body);
    }

    fn read_persisted_cursor(&self, name: &str) -> Option<(i32, i32)> {
        let dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join(".x0ne")
            .join("vm");
        let raw = std::fs::read_to_string(dir.join(format!("cursor-{name}.json"))).ok()?;
        // very small JSON, parse by hand to avoid serde_json overhead
        let x_str = raw.split("\"x\":").nth(1)?.split(',').next()?.trim();
        let y_str = raw.split("\"y\":").nth(1)?.split('}').next()?.trim();
        Some((x_str.parse().ok()?, y_str.parse().ok()?))
    }

    /// Run `virsh <args>` and return stdout as a String, or error with stderr.
    async fn virsh(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("virsh");
        if let Some(uri) = &self.uri {
            cmd.arg("-c").arg(uri);
        }
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let out = cmd
            .output()
            .await
            .with_context(|| format!("failed to spawn virsh {args:?}"))?;
        if !out.status.success() {
            bail!(
                "virsh {} exited {}: {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    async fn vnc_endpoint(&self, name: &str) -> Result<String> {
        let raw = self.virsh(&["vncdisplay", name]).await?;
        parse_vnc_display(&raw).ok_or_else(|| anyhow!("could not parse vncdisplay output: {raw:?}"))
    }

    async fn open_vnc(&self, name: &str) -> Result<VncInputClient> {
        let ep = self.vnc_endpoint(name).await?;
        VncInputClient::connect(&ep).await
    }
}

impl Default for LibvirtSession {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VmSession for LibvirtSession {
    async fn list(&self) -> Result<Vec<VmInfo>> {
        let raw = self.virsh(&["list", "--all"]).await?;
        Ok(parse_list(&raw))
    }

    async fn info(&self, name: &str) -> Result<VmInfo> {
        let raw = self.virsh(&["dominfo", name]).await?;
        let mut info =
            parse_dominfo(&raw).ok_or_else(|| anyhow!("could not parse dominfo for {name}"))?;
        if info.state == VmState::Running {
            info.vnc_endpoint = self.vnc_endpoint(name).await.ok();
        }
        Ok(info)
    }

    async fn start(&self, name: &str) -> Result<()> {
        self.virsh(&["start", name]).await.map(|_| ())
    }

    async fn shutdown(&self, name: &str) -> Result<()> {
        self.virsh(&["shutdown", name]).await.map(|_| ())
    }

    async fn force_off(&self, name: &str) -> Result<()> {
        self.virsh(&["destroy", name]).await.map(|_| ())
    }

    async fn reboot(&self, name: &str) -> Result<()> {
        self.virsh(&["reboot", name]).await.map(|_| ())
    }

    async fn screenshot(&self, name: &str) -> Result<Screenshot> {
        let tmp = tempfile_path("x0ne-vm-screenshot", "ppm");
        // virsh screenshot writes whatever the host hypervisor produces — for
        // QEMU/KVM that's PPM (P6). We convert to PNG via the `image` crate.
        let raw = self
            .virsh(&["screenshot", name, "--file", tmp.to_string_lossy().as_ref()])
            .await?;
        let bytes = tokio::fs::read(&tmp)
            .await
            .with_context(|| format!("read screenshot file {:?} (virsh said: {raw})", tmp))?;
        // Best-effort cleanup — don't fail the call if it sticks.
        let _ = tokio::fs::remove_file(&tmp).await;
        encode_png_from_any(&bytes)
    }

    async fn mouse_move(&self, name: &str, x: i32, y: i32) -> Result<()> {
        let mut c = self.open_vnc(name).await?;
        c.pointer_move(x, y).await?;
        self.remember_cursor(name, x, y);
        Ok(())
    }

    async fn mouse_click(
        &self,
        name: &str,
        x: i32,
        y: i32,
        button: MouseButton,
        double: bool,
    ) -> Result<()> {
        let mut c = self.open_vnc(name).await?;
        c.pointer_click(x, y, button.mask()).await?;
        if double {
            c.pointer_click(x, y, button.mask()).await?;
        }
        self.remember_cursor(name, x, y);
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
    ) -> Result<()> {
        let mut c = self.open_vnc(name).await?;
        c.pointer_drag(x1, y1, x2, y2, button.mask()).await?;
        self.remember_cursor(name, x2, y2);
        Ok(())
    }

    async fn mouse_scroll(
        &self,
        name: &str,
        direction: ScrollDirection,
        clicks: u32,
    ) -> Result<()> {
        let mut c = self.open_vnc(name).await?;
        c.pointer_scroll(direction.mask(), clicks).await
    }

    async fn cursor_position(&self, name: &str) -> Result<Option<(i32, i32)>> {
        // Prefer the in-process cache, fall back to the disk file so out-of-
        // process callers (e.g. the live watcher) get a useful answer too.
        {
            let m = self.last_cursor.lock().unwrap();
            if let Some(&p) = m.get(name) {
                return Ok(Some(p));
            }
        }
        Ok(self.read_persisted_cursor(name))
    }

    async fn type_text(&self, name: &str, text: &str) -> Result<()> {
        let mut c = self.open_vnc(name).await?;
        const SHIFT_L: u32 = 0xffe1;
        for ch in text.chars() {
            let keysym = keymap::char_keysym(ch).ok_or_else(|| {
                anyhow!("non-ASCII character {:?} cannot be typed in v0 — set guest layout to US and ASCII text", ch)
            })?;
            // QEMU's keysym→scancode mapping assumes a US keyboard: a keysym
            // like '&' (0x26) lives on the same key as '7', so without Shift
            // down we get the unshifted scancode. Wrap shifted chars manually.
            if keymap::ascii_char_needs_shift(ch) {
                c.key_chord(&[SHIFT_L], keysym).await?;
            } else {
                c.key_tap(keysym).await?;
            }
        }
        Ok(())
    }

    async fn key_chord(&self, name: &str, chord: &KeyChord) -> Result<()> {
        let main =
            keymap::resolve(&chord.key).ok_or_else(|| anyhow!("unknown key: {:?}", chord.key))?;
        let mods: Vec<u32> = chord
            .modifiers
            .iter()
            .map(|m| keymap::resolve(m).ok_or_else(|| anyhow!("unknown modifier: {:?}", m)))
            .collect::<Result<_>>()?;
        let mut c = self.open_vnc(name).await?;
        c.key_chord(&mods, main).await
    }

    async fn snapshot_create(&self, name: &str, snap: &str) -> Result<()> {
        self.virsh(&["snapshot-create-as", name, snap])
            .await
            .map(|_| ())
    }

    async fn snapshot_restore(&self, name: &str, snap: &str) -> Result<()> {
        self.virsh(&["snapshot-revert", name, snap])
            .await
            .map(|_| ())
    }

    async fn snapshot_list(&self, name: &str) -> Result<Vec<String>> {
        let raw = self.virsh(&["snapshot-list", name, "--name"]).await?;
        Ok(raw
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }

    async fn snapshot_delete(&self, name: &str, snap: &str) -> Result<()> {
        self.virsh(&["snapshot-delete", name, snap])
            .await
            .map(|_| ())
    }

    /// Linked clone via qemu-img backing chain + virsh dumpxml manipulation.
    /// Steps:
    /// 1. Get base VM's primary disk path.
    /// 2. Create a fresh qcow2 overlay with backing_file=base_disk.
    /// 3. Dump base XML, rewrite name + disk + UUID + MAC, define the new VM.
    /// Works while the base VM is running (CoW guarantees consistency, modulo
    /// the in-RAM state of the base — clones boot from disk-state-at-creation).
    async fn clone_vm(&self, base: &str, new_name: &str) -> Result<()> {
        if base == new_name {
            bail!("clone base and new_name must differ");
        }
        // SECURITY: `new_name` is interpolated into filesystem paths (the overlay
        // qcow2 under ~/.x0ne/vm/disks/ and the temp clone XML) and the libvirt
        // domain name. A value like "../../etc/cron.d/x" or an absolute path
        // escapes the disk dir (Path::join replaces the base on an absolute/`..`
        // component), giving a write-controlled-path + domain-define primitive.
        // `base` is passed as a bare argv token to virsh, so a leading '-' could
        // be parsed as a flag. Restrict both to plain VM identifiers.
        if !is_valid_vm_name(new_name) {
            bail!("invalid new_name: must be 1-64 chars of [A-Za-z0-9_-]");
        }
        if !is_valid_vm_name(base) {
            bail!("invalid base name: must be 1-64 chars of [A-Za-z0-9_-]");
        }
        // Refuse if target already exists.
        let existing = self.virsh(&["list", "--all", "--name"]).await?;
        if existing.lines().any(|l| l.trim() == new_name) {
            bail!("VM '{new_name}' already exists — pick a fresh name");
        }

        // 1. Inspect base XML to find its first disk path.
        let base_xml = self.virsh(&["dumpxml", base]).await?;
        let base_disk = parse_first_disk_path(&base_xml)
            .ok_or_else(|| anyhow!("could not find <disk> path in {base}'s XML"))?;

        // 2. Create overlay qcow2 pointing at base.
        let disk_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join(".x0ne")
            .join("vm")
            .join("disks");
        tokio::fs::create_dir_all(&disk_dir).await?;
        let overlay = disk_dir.join(format!("{new_name}.qcow2"));
        if overlay.exists() {
            bail!("overlay disk {:?} already exists", overlay);
        }
        let mut img = Command::new("qemu-img");
        img.arg("create")
            .arg("-f")
            .arg("qcow2")
            .arg("-F")
            .arg("qcow2")
            .arg("-b")
            .arg(&base_disk)
            .arg(&overlay)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = img.output().await.context("qemu-img create")?;
        if !out.status.success() {
            bail!(
                "qemu-img create failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }

        // 3. Build a clone XML: rewrite name, uuid, disk source, drop the MAC.
        let clone_xml = rewrite_xml_for_clone(&base_xml, new_name, &overlay.to_string_lossy())
            .ok_or_else(|| anyhow!("xml rewrite failed"))?;
        let tmp = std::env::temp_dir().join(format!("x0ne-vm-clone-{new_name}.xml"));
        tokio::fs::write(&tmp, clone_xml).await?;
        let res = self
            .virsh(&["define", tmp.to_string_lossy().as_ref()])
            .await;
        let _ = tokio::fs::remove_file(&tmp).await;
        res?;
        Ok(())
    }

    async fn destroy_clone(&self, name: &str) -> Result<()> {
        // Safety: only touch VMs whose disk is under ~/.x0ne/vm/disks/.
        let xml = self.virsh(&["dumpxml", name]).await?;
        let disk = parse_first_disk_path(&xml)
            .ok_or_else(|| anyhow!("could not find disk path for {name}"))?;
        let safe_root = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join(".x0ne")
            .join("vm")
            .join("disks");
        let safe_root_str = safe_root.to_string_lossy().to_string();
        if !disk.starts_with(&safe_root_str) {
            bail!(
                "refusing to destroy '{name}': its disk {disk:?} is outside {safe_root_str} — \
                 destroy_clone only operates on VMs created by clone_from"
            );
        }
        // Try a graceful shutdown first, then force off, then undefine.
        let _ = self.virsh(&["destroy", name]).await; // ignore if already off
        let _ = self
            .virsh(&["undefine", name, "--remove-all-storage"])
            .await;
        // If --remove-all-storage didn't get the overlay (unusual), nuke it.
        let _ = tokio::fs::remove_file(&disk).await;
        Ok(())
    }
}

/// True if `name` is a safe VM identifier: 1-64 chars of `[A-Za-z0-9_-]`,
/// the first one a letter or a digit. Used to keep agent-supplied VM and
/// snapshot names out of path-traversal / argv-flag positions: every tool
/// hands them to virsh as bare words, and a word that starts with `-` is an
/// option (`--memspec=file=<path>` writes guest memory there). Pure for unit
/// testing. (Audit of 2026-10-30, H1-5.)
pub fn is_valid_vm_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Parse the first `<source file='...'/>` inside a `<disk>` block from a
/// libvirt domain XML. Pure for unit testing.
pub fn parse_first_disk_path(xml: &str) -> Option<String> {
    // We're inside <disk>...</disk> sections; find the first source file.
    let in_disk = xml.split("<disk").nth(1)?;
    let until_close = in_disk.split("</disk>").next()?;
    let attr = "<source file='";
    let start = until_close.find(attr)? + attr.len();
    let tail = &until_close[start..];
    let end = tail.find('\'')?;
    Some(tail[..end].to_string())
}

/// Rewrite a base domain XML for a linked clone:
/// - replace the domain name with `new_name`
/// - replace the first disk source path with `new_disk`
/// - drop the `<uuid>` element so libvirt assigns a fresh one
/// - drop `<mac address='...'/>` so libvirt assigns a fresh MAC
/// Pure string ops — keeps the function unit-testable without a real virsh.
pub fn rewrite_xml_for_clone(base_xml: &str, new_name: &str, new_disk: &str) -> Option<String> {
    let mut out = base_xml.to_string();

    // Replace <name>...</name>.
    let nstart = out.find("<name>")? + "<name>".len();
    let nend = out[nstart..].find("</name>")? + nstart;
    out.replace_range(nstart..nend, new_name);

    // Replace first <source file='...'/> inside a <disk> block.
    let dstart = out.find("<disk")?;
    let attr = "<source file='";
    let abs_start = out[dstart..].find(attr)? + dstart + attr.len();
    let abs_end = out[abs_start..].find('\'')? + abs_start;
    out.replace_range(abs_start..abs_end, new_disk);

    // Strip <uuid>...</uuid> and <mac address='...'/> entries so libvirt
    // re-generates them. (Multiple MACs may exist if base has many NICs.)
    out = strip_xml_element(&out, "uuid");
    out = strip_xml_attr_element(&out, "mac");
    // Strip cdrom disk blocks — pool clones don't need install media, and
    // lightweight guest kernels (microVMs) often lack SATA/IDE drivers entirely.
    // Also avoids libvirt's exclusive-lock contention on the ISO file when
    // multiple clones boot concurrently.
    out = strip_xml_disk_with_device(&out, "cdrom");

    Some(out)
}

/// Strip every `<disk ... device='<device>' ...>...</disk>` block.
/// Used to remove cdrom drives from clone XML — they're useless once the OS
/// is installed and they cause exclusive-lock contention if N clones share
/// the same ISO.
pub fn strip_xml_disk_with_device(xml: &str, device: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    let needle = format!("device='{device}'");
    while let Some(pos) = rest.find("<disk") {
        // Read the opening tag up to its '>'
        let open_end = match rest[pos..].find('>') {
            Some(e) => pos + e + 1,
            None => break,
        };
        let opening = &rest[pos..open_end];
        if opening.contains(&needle) {
            // Find </disk> closing
            let close = match rest[open_end..].find("</disk>") {
                Some(c) => open_end + c + "</disk>".len(),
                None => break,
            };
            out.push_str(&rest[..pos]);
            // Skip any leading whitespace + newline that preceded the block,
            // to keep the XML tidy.
            let trimmed_tail = out.trim_end_matches([' ', '\t']);
            let kept = trimmed_tail.len();
            out.truncate(kept);
            rest = &rest[close..];
        } else {
            // Different device — keep this disk, advance past its opener.
            out.push_str(&rest[..open_end]);
            rest = &rest[open_end..];
        }
    }
    out.push_str(rest);
    out
}

fn strip_xml_element(xml: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(o) = rest.find(&open) {
        out.push_str(&rest[..o]);
        let after = &rest[o..];
        if let Some(c) = after.find(&close) {
            rest = &after[c + close.len()..];
        } else {
            out.push_str(after);
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

fn strip_xml_attr_element(xml: &str, tag: &str) -> String {
    // remove `<tag ...attrs.../>` self-closing forms
    let open = format!("<{tag}");
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(o) = rest.find(&open) {
        out.push_str(&rest[..o]);
        let after = &rest[o..];
        if let Some(c) = after.find("/>") {
            rest = &after[c + 2..];
        } else if let Some(c) = after.find('>') {
            rest = &after[c + 1..];
        } else {
            out.push_str(after);
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

fn tempfile_path(prefix: &str, ext: &str) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{prefix}-{ts}.{ext}"))
}

/// Re-encode any image format we can decode (PPM, PNG, etc.) as PNG so MCP
/// clients always receive a stable format.
fn encode_png_from_any(bytes: &[u8]) -> Result<Screenshot> {
    use image::ImageReader;
    use std::io::Cursor;
    let img = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("guess screenshot format")?
        .decode()
        .context("decode screenshot")?;
    let width = img.width();
    let height = img.height();
    let mut out = Vec::with_capacity(bytes.len());
    img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .context("re-encode screenshot as PNG")?;
    Ok(Screenshot {
        png: out,
        width,
        height,
    })
}

// ─── parsers (pure functions for unit testing) ──────────────────────────────

/// Parse `virsh list --all` output into [`VmInfo`] rows. Tolerant to header
/// formatting variations across virsh versions.
pub fn parse_list(raw: &str) -> Vec<VmInfo> {
    let mut out = Vec::new();
    let mut in_body = false;
    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // The separator is a line of dashes — skip it and flip into body mode.
        if t.chars().all(|c| c == '-' || c.is_whitespace()) {
            in_body = true;
            continue;
        }
        if !in_body {
            continue;
        }
        // Whitespace-separated columns: Id, Name, State (state may contain a
        // space — "shut off"). Collapse runs of whitespace, then take Id +
        // Name + rest-joined as state.
        let cols: Vec<&str> = t.split_whitespace().collect();
        if cols.len() < 3 {
            continue;
        }
        let name = cols[1].to_string();
        let state = VmState::parse_virsh(&cols[2..].join(" "));
        out.push(VmInfo {
            name,
            uuid: None,
            state,
            vcpu: None,
            max_mem_kib: None,
            used_mem_kib: None,
            vnc_endpoint: None,
        });
    }
    out
}

/// Parse `virsh dominfo <name>` output. Tolerant to blank lines and to
/// new fields appearing in future virsh versions (skips unknown keys).
pub fn parse_dominfo(raw: &str) -> Option<VmInfo> {
    let mut name = None;
    let mut uuid = None;
    let mut state = None;
    let mut vcpu = None;
    let mut max_mem_kib = None;
    let mut used_mem_kib = None;
    for line in raw.lines() {
        let Some((k, v)) = line.split_once(':').map(|(k, v)| (k.trim(), v.trim())) else {
            continue;
        };
        match k {
            "Name" => name = Some(v.to_string()),
            "UUID" => uuid = Some(v.to_string()),
            "State" => state = Some(VmState::parse_virsh(v)),
            "CPU(s)" => vcpu = v.parse().ok(),
            "Max memory" => max_mem_kib = parse_mem_kib(v),
            "Used memory" => used_mem_kib = parse_mem_kib(v),
            _ => {}
        }
    }
    Some(VmInfo {
        name: name?,
        uuid,
        state: state.unwrap_or(VmState::Other),
        vcpu,
        max_mem_kib,
        used_mem_kib,
        vnc_endpoint: None,
    })
}

/// Parse `virsh vncdisplay <name>` output into a `host:port` endpoint.
/// Forms: `:0`, `127.0.0.1:0`, `:1` → `127.0.0.1:5900`, `127.0.0.1:5900`, etc.
pub fn parse_vnc_display(raw: &str) -> Option<String> {
    let line = raw.lines().find(|l| !l.trim().is_empty())?.trim();
    let idx = line.rfind(':')?;
    let (h, d) = line.split_at(idx);
    let h = h.trim();
    let display = &d[1..]; // skip ':'
    let host = if h.is_empty() { "127.0.0.1" } else { h };
    let display: u16 = display.trim().parse().ok()?;
    Some(format!("{host}:{}", 5900 + display))
}

fn parse_mem_kib(s: &str) -> Option<u64> {
    // "2097152 KiB" → 2097152
    s.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_flag_vm_names() {
        // SECURITY: clone_vm guards against path traversal / argv-flag injection.
        assert!(is_valid_vm_name("ubuntu-24"));
        assert!(is_valid_vm_name("clone_1"));
        assert!(!is_valid_vm_name("../../etc/cron.d/x"));
        assert!(!is_valid_vm_name("/etc/passwd"));
        assert!(!is_valid_vm_name("-oProxyCommand=x"));
        assert!(!is_valid_vm_name("a/b"));
        assert!(!is_valid_vm_name("name with space"));
        assert!(!is_valid_vm_name(""));
        assert!(!is_valid_vm_name(&"x".repeat(65)));
    }

    /// H1-5 (audit of 2026-10-30): every lifecycle and snapshot tool hands
    /// the agent's `name` and `snapshot` to virsh as bare argv words.
    /// `snapshot-create-as <vm> --memspec=file=<path>` writes the guest's
    /// memory where that word says. A name starts with a letter or a digit;
    /// a word that starts with `-` is an option.
    #[test]
    fn a_word_that_reads_as_a_virsh_option_is_not_a_name() {
        assert!(!is_valid_vm_name("--help"));
        assert!(!is_valid_vm_name("--memspec"));
        assert!(!is_valid_vm_name("-x"));
        assert!(is_valid_vm_name("snap-1"));
        assert!(is_valid_vm_name("0day"));
    }

    #[test]
    fn parses_list_with_two_rows() {
        let raw = "\
 Id   Name        State
-------------------------------
 -    win11       shut off
 3    ubuntu-24   running
";
        let vms = parse_list(raw);
        assert_eq!(vms.len(), 2);
        assert_eq!(vms[0].name, "win11");
        assert_eq!(vms[0].state, VmState::ShutOff);
        assert_eq!(vms[1].name, "ubuntu-24");
        assert_eq!(vms[1].state, VmState::Running);
    }

    #[test]
    fn parses_empty_list() {
        let raw = "\
 Id   Name   State
--------------------
";
        assert!(parse_list(raw).is_empty());
    }

    #[test]
    fn parses_dominfo() {
        // Real virsh 12.x output, including the "Autostart Once" line that
        // doesn't exist in older versions, plus a stray blank line that
        // would have killed an over-strict parser.
        let raw = "\
Id:             3
Name:           ubuntu-24
UUID:           1234-abcd
OS Type:        hvm

State:          running
CPU(s):         4
CPU time:       12.3s
Max memory:     8388608 KiB
Used memory:    4194304 KiB
Persistent:     yes
Autostart:      disable
Autostart Once: disable
Managed save:   no
Security model: none
Security DOI:   0
";
        let info = parse_dominfo(raw).unwrap();
        assert_eq!(info.name, "ubuntu-24");
        assert_eq!(info.uuid.as_deref(), Some("1234-abcd"));
        assert_eq!(info.state, VmState::Running);
        assert_eq!(info.vcpu, Some(4));
        assert_eq!(info.max_mem_kib, Some(8_388_608));
        assert_eq!(info.used_mem_kib, Some(4_194_304));
    }

    #[test]
    fn parses_vncdisplay_short_form() {
        assert_eq!(parse_vnc_display(":0\n"), Some("127.0.0.1:5900".into()));
        assert_eq!(parse_vnc_display(" :1 \n"), Some("127.0.0.1:5901".into()));
    }

    #[test]
    fn parses_vncdisplay_full_form() {
        assert_eq!(
            parse_vnc_display("127.0.0.1:2\n"),
            Some("127.0.0.1:5902".into())
        );
    }

    #[test]
    fn parses_vncdisplay_rejects_garbage() {
        assert_eq!(parse_vnc_display(""), None);
        assert_eq!(parse_vnc_display("no colon here"), None);
    }

    #[test]
    fn first_disk_path_extracted() {
        let xml = r#"
<domain>
  <name>foo</name>
  <devices>
    <disk type='file' device='disk'>
      <source file='/home/u/.x0ne/vm/disks/foo.qcow2' index='1'/>
    </disk>
    <disk type='file' device='cdrom'>
      <source file='/some/other.iso'/>
    </disk>
  </devices>
</domain>"#;
        assert_eq!(
            parse_first_disk_path(xml).as_deref(),
            Some("/home/u/.x0ne/vm/disks/foo.qcow2")
        );
    }

    #[test]
    fn strip_cdrom_removes_only_cdrom_disks() {
        let xml = r#"
<domain>
  <devices>
    <disk type='file' device='disk'>
      <source file='/main.qcow2'/>
      <target dev='vda' bus='virtio'/>
    </disk>
    <disk type='file' device='cdrom'>
      <driver name='qemu' type='raw'/>
      <source file='/install.iso'/>
      <target dev='sda' bus='sata'/>
      <readonly/>
    </disk>
    <disk type='file' device='disk'>
      <source file='/data.qcow2'/>
    </disk>
  </devices>
</domain>"#;
        let out = strip_xml_disk_with_device(xml, "cdrom");
        assert!(out.contains("/main.qcow2"));
        assert!(out.contains("/data.qcow2"));
        assert!(!out.contains("/install.iso"), "cdrom source should be gone");
        assert!(
            !out.contains("device='cdrom'"),
            "cdrom device declaration should be gone"
        );
        // The 2 disk blocks should remain
        assert_eq!(out.matches("<disk").count(), 2);
    }

    #[test]
    fn xml_rewrite_replaces_name_disk_and_strips_uuid_mac() {
        let xml = r#"<domain type='kvm'>
  <name>base</name>
  <uuid>11111111-1111-1111-1111-111111111111</uuid>
  <devices>
    <disk type='file' device='disk'>
      <source file='/base.qcow2'/>
    </disk>
    <interface type='user'>
      <mac address='52:54:00:aa:bb:cc'/>
      <model type='virtio'/>
    </interface>
  </devices>
</domain>"#;
        let out = rewrite_xml_for_clone(xml, "clone-1", "/clone-1.qcow2").unwrap();
        assert!(out.contains("<name>clone-1</name>"));
        assert!(out.contains("/clone-1.qcow2"));
        assert!(!out.contains("/base.qcow2"));
        assert!(!out.contains("<uuid>"));
        assert!(!out.contains("<mac"));
        // model survived
        assert!(out.contains("<model type='virtio'/>"));
    }

    #[test]
    fn full_clone_rewrite_strips_cdrom_too() {
        let xml = r#"<domain type='kvm'>
  <name>base</name>
  <uuid>11111111-1111-1111-1111-111111111111</uuid>
  <devices>
    <disk type='file' device='disk'>
      <source file='/base.qcow2'/>
    </disk>
    <disk type='file' device='cdrom'>
      <source file='/install.iso'/>
    </disk>
  </devices>
</domain>"#;
        let out = rewrite_xml_for_clone(xml, "clone", "/clone.qcow2").unwrap();
        assert!(out.contains("/clone.qcow2"));
        assert!(
            !out.contains("/install.iso"),
            "cdrom must be stripped from clone"
        );
    }
}
