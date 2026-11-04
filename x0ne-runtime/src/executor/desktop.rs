/// Desktop action executor for Wayland/Sway environments.
///
/// All actions require `desktop.enabled = true` in config.
/// Policy enforcement is done in policy.rs before this executor is reached,
/// except `desktop.allowed_apps`, which depends on the live window tree and is
/// checked here, as close to each injected event as sway IPC allows.
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::config::AppConfig;
use crate::result::{ActionOutput, ActionResult, ErrorKind, WindowInfo};

// ── Rate limiter ──────────────────────────────────────────────────────────────

/// Shared last-input timestamp for rate limiting desktop input actions.
pub type RateLimiter = Arc<Mutex<Instant>>;

#[allow(dead_code)] // only called from #[cfg(test)] in pipeline.rs
pub(crate) fn new_rate_limiter() -> RateLimiter {
    Arc::new(Mutex::new(Instant::now() - Duration::from_secs(60)))
}

async fn check_rate_limit(limiter: &RateLimiter, min_ms: u64) -> bool {
    let last = limiter.lock().await;
    last.elapsed().as_millis() as u64 >= min_ms
}

async fn update_rate_limit(limiter: &RateLimiter) {
    let mut last = limiter.lock().await;
    *last = Instant::now();
}

// ── Binary availability ────────────────────────────────────────────────────────

fn check_binary(name: &str) -> Result<(), ActionResult> {
    let mut cmd = Command::new("which");
    // SECURITY: cleared environment (`crate::child_env`). Every spawn in this
    // file goes through it — `which` included, so the scan in
    // `every_desktop_spawn_clears_the_environment` can be a simple one.
    crate::child_env::harden(&mut cmd);
    let status = cmd.arg(name).output();
    match status {
        Ok(out) if out.status.success() => Ok(()),
        _ => Err(ActionResult::err(
            name,
            ErrorKind::MissingDependency,
            format!(
                "Required binary '{}' not found. Install it to use desktop actions.",
                name
            ),
            0,
        )),
    }
}

fn run_cmd(program: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new(program);
    // SECURITY: cleared environment. The desktop family needs DISPLAY,
    // WAYLAND_DISPLAY, XAUTHORITY, XDG_RUNTIME_DIR and DBUS_SESSION_BUS_ADDRESS
    // to reach a session at all, so those are on the allowlist by name and with
    // that justification (`crate::child_env`); nothing else is.
    crate::child_env::harden(&mut cmd);
    cmd.args(args)
        .output()
        .map_err(|e| e.to_string())
        .and_then(|out| {
            if out.status.success() {
                Ok(String::from_utf8_lossy(&out.stdout).to_string())
            } else {
                Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
            }
        })
}

// ── Backend ───────────────────────────────────────────────────────────────────

/// Every process the input and window gestures start goes through this, so a
/// test can stand in for the compositor and see exactly what would have run.
pub(crate) trait Backend: Sync {
    fn require(&self, bin: &str) -> Result<(), ActionResult>;
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String>;
    fn write_clipboard(&self, content: &str) -> Result<(), String>;

    /// `grim <args>`; the last argument is the file it writes.
    fn capture(&self, args: &[&str]) -> Result<(), String> {
        self.run("grim", args).map(|_| ())
    }

    fn tree(&self) -> Result<Value, String> {
        let raw = self.run("swaymsg", &["-t", "get_tree"])?;
        serde_json::from_str(&raw).map_err(|e| e.to_string())
    }
}

/// The live session: `swaymsg`, `wlrctl`, `wtype`, `wl-copy`.
pub(crate) struct Sway;

impl Backend for Sway {
    fn require(&self, bin: &str) -> Result<(), ActionResult> {
        check_binary(bin)
    }

    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        run_cmd(program, args)
    }

    fn write_clipboard(&self, content: &str) -> Result<(), String> {
        use std::io::Write;
        let mut cmd = Command::new("wl-copy");
        // SECURITY: cleared environment, same class as every spawn here.
        crate::child_env::harden(&mut cmd);
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(content.as_bytes());
        }
        let _ = child.wait();
        Ok(())
    }
}

// ── allowed_apps ──────────────────────────────────────────────────────────────

/// Characters per `wtype` call; the focused window is re-read before each one.
pub(crate) const TYPE_CHUNK: usize = 32;

macro_rules! gate {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
}

fn allowed_list(config: &AppConfig) -> Option<&[String]> {
    let list = config.desktop.allowed_apps.as_slice();
    (!list.is_empty()).then_some(list)
}

/// Identity is the app_id (or X11 class), compared whole. Never the title:
/// any client sets its own, a terminal from one escape sequence.
fn is_allowed(allowed: &[String], w: &WindowInfo) -> bool {
    !w.app_id.is_empty() && allowed.iter().any(|a| a.eq_ignore_ascii_case(&w.app_id))
}

fn label(w: &WindowInfo) -> &str {
    if w.app_id.is_empty() {
        "<no app_id>"
    } else {
        &w.app_id
    }
}

fn refuse(gesture: &str, why: impl std::fmt::Display) -> ActionResult {
    ActionResult::err(
        gesture,
        ErrorKind::PolicyDenied,
        format!("{why}; refused by desktop.allowed_apps."),
        0,
    )
}

fn read_views(d: &dyn Backend, gesture: &str) -> Result<Vec<WindowInfo>, ActionResult> {
    d.tree()
        .map(|t| views(&t))
        .map_err(|e| refuse(gesture, format!("window tree unreadable ({e})")))
}

/// The focused view, required to be a listed app.
fn focused_allowed(
    d: &dyn Backend,
    allowed: &[String],
    gesture: &str,
) -> Result<WindowInfo, ActionResult> {
    let views = read_views(d, gesture)?;
    match views.into_iter().find(|w| w.focused) {
        None => Err(refuse(gesture, "no application window holds the focus")),
        Some(w) if is_allowed(allowed, &w) => Ok(w),
        Some(w) => Err(refuse(
            gesture,
            format!("the focused window belongs to '{}'", label(&w)),
        )),
    }
}

/// Re-read after the input went out: the same listed view must still hold
/// the focus, or the input may have reached another one.
fn still_focused(
    d: &dyn Backend,
    allowed: &[String],
    gesture: &str,
    id: i64,
) -> Result<(), ActionResult> {
    let w = focused_allowed(d, allowed, gesture).map_err(|_| {
        refuse(
            gesture,
            "the focus left the checked window during the gesture; input may have reached another app",
        )
    })?;
    if w.id != id {
        return Err(refuse(
            gesture,
            format!(
                "the focus moved to another '{}' window during the gesture",
                label(&w)
            ),
        ));
    }
    Ok(())
}

/// Every visible view under (x, y) must be a listed app, and there must be one.
fn point_allowed(
    d: &dyn Backend,
    allowed: &[String],
    gesture: &str,
    x: i32,
    y: i32,
) -> Result<(), ActionResult> {
    let under: Vec<WindowInfo> = read_views(d, gesture)?
        .into_iter()
        .filter(|w| w.visible && x >= w.x && y >= w.y && x < w.x + w.width && y < w.y + w.height)
        .collect();
    if under.is_empty() {
        return Err(refuse(
            gesture,
            format!("no application window under ({x}, {y})"),
        ));
    }
    if let Some(w) = under.iter().find(|w| !is_allowed(allowed, w)) {
        return Err(refuse(
            gesture,
            format!("'{}' is under ({x}, {y})", label(w)),
        ));
    }
    Ok(())
}

/// Check the point, warp, check again: the window under it can change
/// between the first read and the warp.
fn aim(
    d: &dyn Backend,
    allowed: &[String],
    gesture: &str,
    x: i32,
    y: i32,
) -> Result<(), ActionResult> {
    point_allowed(d, allowed, gesture, x, y)?;
    move_cursor_abs(d, x, y)
        .map_err(|e| ActionResult::err(gesture, ErrorKind::ExecutionFailed, e, 0))?;
    point_allowed(d, allowed, gesture, x, y)
}

/// Keys the compositor binds rather than the window (launcher, exit, power).
fn compositor_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["super", "meta", "hyper", "logo", "win", "mod4"].contains(&k.as_str())
        || ["super_", "meta_", "hyper_", "xf86"]
            .iter()
            .any(|p| k.starts_with(p))
}

/// Split for `wtype`, never starting a later chunk with '-' (a lone "-k" is
/// read as an option).
pub(crate) fn type_chunks(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut n = 0;
    for (i, c) in text.char_indices() {
        if n >= TYPE_CHUNK && c != '-' {
            out.push(&text[start..i]);
            start = i;
            n = 0;
        }
        n += 1;
    }
    if start < text.len() || out.is_empty() {
        out.push(&text[start..]);
    }
    out
}

// ── Screenshot ────────────────────────────────────────────────────────────────

pub async fn screenshot(
    output: Option<&str>,
    region: Option<&str>,
    config: &AppConfig,
) -> ActionResult {
    screenshot_on(&Sway, output, region, config).await
}

pub(crate) async fn screenshot_on(
    d: &dyn Backend,
    output: Option<&str>,
    region: Option<&str>,
    config: &AppConfig,
) -> ActionResult {
    const OP: &str = "Screenshot";
    if let Err(r) = d.require("grim") {
        return r;
    }

    let dir = config.desktop.resolved_screenshot_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return ActionResult::err(OP, ErrorKind::Internal, e.to_string(), 0);
    }

    let ts = Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let filename = dir.join(format!("{}.png", ts));
    let path_str = filename.to_string_lossy().to_string();

    let allowed = allowed_list(config);
    // Under the list, grim writes a hidden file that is masked into the
    // returned one; nothing unmasked is left under the returned name.
    let (before, area) = match allowed {
        None => (None, None),
        Some(_) => {
            let before = match d.tree() {
                Ok(t) => t,
                Err(e) => return refuse(OP, format!("window tree unreadable ({e})")),
            };
            let area = match capture_area(&before, output, region) {
                Ok(a) => a,
                Err(e) => return refuse(OP, e),
            };
            (Some(before), Some(area))
        }
    };
    let raw = match allowed {
        None => filename.clone(),
        Some(_) => dir.join(format!(".{ts}.unmasked.png")),
    };

    let mut args: Vec<String> = Vec::new();
    if let Some(out) = output {
        args.push("-o".into());
        args.push(out.to_string());
    }
    if let Some(reg) = region {
        args.push("-g".into());
        args.push(reg.to_string());
    }
    args.push(raw.to_string_lossy().to_string());

    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    if let Err(e) = d.capture(&arg_refs) {
        let _ = std::fs::remove_file(&raw);
        return ActionResult::err(OP, ErrorKind::ExecutionFailed, e, 0);
    }

    let (w, h) = match (allowed, before, area) {
        (Some(list), Some(before), Some(area)) => {
            let masked = d
                .tree()
                .map_err(|e| format!("window tree unreadable after the capture ({e})"))
                .and_then(|after| mask_capture(&raw, &filename, list, &before, &after, area));
            let _ = std::fs::remove_file(&raw);
            match masked {
                Ok(dims) => dims,
                Err(e) => {
                    let _ = std::fs::remove_file(&filename);
                    return refuse(OP, e);
                }
            }
        }
        _ => parse_image_dims(&path_str),
    };
    let size_bytes = std::fs::metadata(&filename).map(|m| m.len()).unwrap_or(0);
    ActionResult::ok(
        OP,
        ActionOutput::ScreenshotTaken {
            path: path_str,
            width: w,
            height: h,
            size_bytes,
        },
        0,
    )
}

/// A layout rectangle: x, y, width, height.
type Area = (i32, i32, i32, i32);

/// The layout area the image covers: the region, the named output, or the
/// root (every output). Unknown: refused, since nothing could be mapped.
fn capture_area(tree: &Value, output: Option<&str>, region: Option<&str>) -> Result<Area, String> {
    let rect = |n: &Value| -> Option<Area> {
        let r = n.get("rect")?;
        let f = |k: &str| r.get(k).and_then(|v| v.as_i64()).map(|v| v as i32);
        let a = (f("x")?, f("y")?, f("width")?, f("height")?);
        (a.2 > 0 && a.3 > 0).then_some(a)
    };
    if let Some(g) = region {
        let parse = || -> Option<Area> {
            let (pos, size) = g.trim().split_once(' ')?;
            let (x, y) = pos.split_once(',')?;
            let (w, h) = size.split_once('x')?;
            let a = (
                x.trim().parse().ok()?,
                y.trim().parse().ok()?,
                w.trim().parse().ok()?,
                h.trim().parse().ok()?,
            );
            (a.2 > 0 && a.3 > 0).then_some(a)
        };
        return parse().ok_or_else(|| format!("region '{g}' is not 'x,y wxh'"));
    }
    if let Some(name) = output {
        return tree
            .get("nodes")
            .and_then(|n| n.as_array())
            .into_iter()
            .flatten()
            .find(|n| {
                n.get("type").and_then(|t| t.as_str()) == Some("output")
                    && n.get("name").and_then(|t| t.as_str()) == Some(name)
            })
            .and_then(rect)
            .ok_or_else(|| format!("output '{name}' has no geometry in the window tree"));
    }
    rect(tree).ok_or_else(|| "the window tree gives no screen geometry".to_string())
}

/// Copy only the listed views, identical in the trees read before and after
/// the capture, onto a black image; then black out every unlisted visible
/// view of either tree (a floating one may lie over a listed one). Bars,
/// wallpaper and anything not in the tree stay black.
fn mask_capture(
    raw: &std::path::Path,
    out: &std::path::Path,
    allowed: &[String],
    before: &Value,
    after: &Value,
    area: Area,
) -> Result<(u32, u32), String> {
    let src = image::open(raw)
        .map_err(|e| format!("capture unreadable ({e})"))?
        .to_rgba8();
    let (iw, ih) = src.dimensions();
    let (sx, sy) = (
        f64::from(iw) / f64::from(area.2),
        f64::from(ih) / f64::from(area.3),
    );
    // Layout rect -> pixel span; `outer` rounds outwards (what is blacked
    // out), otherwise inwards (what is copied).
    let span = |w: &WindowInfo, outer: bool| -> (u32, u32, u32, u32) {
        let conv = |v: i32, o: i32, s: f64, max: u32, up: bool| -> u32 {
            let f = f64::from(v - o) * s;
            let r = if up { f.ceil() } else { f.floor() };
            r.clamp(0.0, f64::from(max)) as u32
        };
        (
            conv(w.x, area.0, sx, iw, !outer),
            conv(w.y, area.1, sy, ih, !outer),
            conv(w.x + w.width, area.0, sx, iw, outer),
            conv(w.y + w.height, area.1, sy, ih, outer),
        )
    };
    let (vb, va) = (views(before), views(after));
    let same = |w: &WindowInfo| {
        va.iter().any(|a| {
            a.id == w.id
                && (a.x, a.y, a.width, a.height) == (w.x, w.y, w.width, w.height)
                && a.app_id == w.app_id
                && a.visible
        })
    };
    let mut img = image::RgbaImage::from_pixel(iw, ih, image::Rgba([0, 0, 0, 255]));
    for w in vb
        .iter()
        .filter(|w| w.visible && is_allowed(allowed, w) && same(w))
    {
        let (x0, y0, x1, y1) = span(w, false);
        for y in y0..y1 {
            for x in x0..x1 {
                img.put_pixel(x, y, *src.get_pixel(x, y));
            }
        }
    }
    for w in vb
        .iter()
        .chain(va.iter())
        .filter(|w| w.visible && !is_allowed(allowed, w))
    {
        let (x0, y0, x1, y1) = span(w, true);
        for y in y0..y1 {
            for x in x0..x1 {
                img.put_pixel(x, y, image::Rgba([0, 0, 0, 255]));
            }
        }
    }
    img.save_with_format(out, image::ImageFormat::Png)
        .map_err(|e| format!("masked image not written ({e})"))?;
    Ok((iw, ih))
}

fn parse_image_dims(path: &str) -> (u32, u32) {
    // Try `identify -format "%wx%h" path` from ImageMagick
    let mut cmd = Command::new("identify");
    // SECURITY: cleared environment, same class as every spawn here.
    crate::child_env::harden(&mut cmd);
    cmd.args(["-format", "%wx%h", path])
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).to_string();
            let mut parts = s.trim().splitn(2, 'x');
            let w: u32 = parts.next()?.parse().ok()?;
            let h: u32 = parts.next()?.parse().ok()?;
            Some((w, h))
        })
        .unwrap_or((0, 0))
}

// ── Window tree ───────────────────────────────────────────────────────────────

/// Under the list, a window outside it keeps its id, app_id and geometry
/// (needed to understand a refusal) but loses its title, which carries
/// content: a page title, a file name, a terminal's last command.
fn redact(mut w: WindowInfo, allowed: Option<&[String]>) -> WindowInfo {
    if allowed.is_some_and(|list| !is_allowed(list, &w)) {
        w.name.clear();
    }
    w
}

pub async fn get_window_tree(config: &AppConfig) -> ActionResult {
    get_window_tree_on(&Sway, config).await
}

pub(crate) async fn get_window_tree_on(d: &dyn Backend, config: &AppConfig) -> ActionResult {
    if let Err(r) = d.require("swaymsg") {
        return r;
    }
    match d.tree() {
        Ok(tree) => {
            let allowed = allowed_list(config);
            let windows = flatten_tree(&tree)
                .into_iter()
                .map(|w| redact(w, allowed))
                .collect();
            ActionResult::ok("GetWindowTree", ActionOutput::WindowTree { windows }, 0)
        }
        Err(e) => ActionResult::err("GetWindowTree", ErrorKind::ExecutionFailed, e, 0),
    }
}

pub async fn get_focused_window(config: &AppConfig) -> ActionResult {
    get_focused_window_on(&Sway, config).await
}

pub(crate) async fn get_focused_window_on(d: &dyn Backend, config: &AppConfig) -> ActionResult {
    if let Err(r) = d.require("swaymsg") {
        return r;
    }
    match d.tree() {
        Ok(tree) => {
            let focused = find_focused(&tree).map(|w| redact(w, allowed_list(config)));
            ActionResult::ok(
                "GetFocusedWindow",
                ActionOutput::FocusedWindow { window: focused },
                0,
            )
        }
        Err(e) => ActionResult::err("GetFocusedWindow", ErrorKind::ExecutionFailed, e, 0),
    }
}

fn flatten_tree(node: &Value) -> Vec<WindowInfo> {
    let mut result = Vec::new();
    collect_windows(node, &mut result);
    result
}

fn collect_windows(node: &Value, out: &mut Vec<WindowInfo>) {
    // A window node has "name" and "rect"
    let is_window = node
        .get("type")
        .and_then(|t| t.as_str())
        .map(|t| t == "con" || t == "floating_con")
        .unwrap_or(false)
        && node.get("name").and_then(|n| n.as_str()).is_some();

    if is_window {
        out.push(window_info(node));
    }

    if let Some(nodes) = node.get("nodes").and_then(|n| n.as_array()) {
        for child in nodes {
            collect_windows(child, out);
        }
    }
    if let Some(nodes) = node.get("floating_nodes").and_then(|n| n.as_array()) {
        for child in nodes {
            collect_windows(child, out);
        }
    }
}

fn window_info(node: &Value) -> WindowInfo {
    let rect = node.get("rect").cloned().unwrap_or(Value::Null);
    WindowInfo {
        id: node.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
        name: node
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        app_id: node
            .get("app_id")
            .and_then(|v| v.as_str())
            .or_else(|| {
                node.get("window_properties")
                    .and_then(|p| p.get("class"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("")
            .to_string(),
        x: rect.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
        y: rect.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
        width: rect.get("width").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
        height: rect.get("height").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
        focused: node
            .get("focused")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        visible: node
            .get("visible")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
    }
}

/// Leaf containers only: the windows of applications. A split container has
/// no app identity, so counting it would refuse every nested layout.
fn views(node: &Value) -> Vec<WindowInfo> {
    let mut out = Vec::new();
    collect_views(node, &mut out);
    out
}

fn collect_views(node: &Value, out: &mut Vec<WindowInfo>) {
    let children = |k: &str| {
        node.get(k)
            .and_then(|n| n.as_array())
            .cloned()
            .unwrap_or_default()
    };
    let (tiled, floating) = (children("nodes"), children("floating_nodes"));
    let is_con = matches!(
        node.get("type").and_then(|t| t.as_str()),
        Some("con" | "floating_con")
    );
    if is_con && tiled.is_empty() && floating.is_empty() {
        out.push(window_info(node));
    }
    for child in tiled.iter().chain(floating.iter()) {
        collect_views(child, out);
    }
}

fn find_focused(node: &Value) -> Option<WindowInfo> {
    let mut all = Vec::new();
    collect_windows(node, &mut all);
    all.into_iter().find(|w| w.focused)
}

// ── Mouse ─────────────────────────────────────────────────────────────────────

/// Move cursor to absolute (x, y) using swaymsg (preferred) or wlrctl fallback.
fn move_cursor_abs(d: &dyn Backend, x: i32, y: i32) -> Result<(), String> {
    let xs = x.to_string();
    let ys = y.to_string();
    if d.require("swaymsg").is_ok() {
        let cmd = format!("seat - cursor set {} {}", xs, ys);
        d.run("swaymsg", &[&cmd]).map(|_| ())
    } else {
        d.run("wlrctl", &["pointer", "move", &xs, &ys]).map(|_| ())
    }
}

pub async fn move_mouse(x: i32, y: i32, config: &AppConfig, limiter: &RateLimiter) -> ActionResult {
    move_mouse_on(&Sway, x, y, config, limiter).await
}

pub(crate) async fn move_mouse_on(
    d: &dyn Backend,
    x: i32,
    y: i32,
    config: &AppConfig,
    limiter: &RateLimiter,
) -> ActionResult {
    if !check_rate_limit(limiter, config.desktop.input_rate_limit_ms).await {
        return ActionResult::err(
            "MoveMouse",
            ErrorKind::PolicyDenied,
            "Input rate limit exceeded.",
            0,
        );
    }
    if let Some(allowed) = allowed_list(config) {
        gate!(aim(d, allowed, "MoveMouse", x, y));
        update_rate_limit(limiter).await;
        return ActionResult::ok("MoveMouse", ActionOutput::MouseMoved { x, y }, 0);
    }
    match move_cursor_abs(d, x, y) {
        Ok(_) => {
            update_rate_limit(limiter).await;
            ActionResult::ok("MoveMouse", ActionOutput::MouseMoved { x, y }, 0)
        }
        Err(e) => ActionResult::err("MoveMouse", ErrorKind::ExecutionFailed, e, 0),
    }
}

pub async fn click(
    x: Option<i32>,
    y: Option<i32>,
    button: Option<&str>,
    config: &AppConfig,
    limiter: &RateLimiter,
) -> ActionResult {
    click_on(&Sway, x, y, button, config, limiter).await
}

pub(crate) async fn click_on(
    d: &dyn Backend,
    x: Option<i32>,
    y: Option<i32>,
    button: Option<&str>,
    config: &AppConfig,
    limiter: &RateLimiter,
) -> ActionResult {
    if !check_rate_limit(limiter, config.desktop.input_rate_limit_ms).await {
        return ActionResult::err(
            "Click",
            ErrorKind::PolicyDenied,
            "Input rate limit exceeded.",
            0,
        );
    }
    if let Err(r) = d.require("wlrctl") {
        return r;
    }
    let btn = button.unwrap_or("left");
    if let Some(allowed) = allowed_list(config) {
        let (Some(px), Some(py)) = (x, y) else {
            return refuse(
                "Click",
                "no coordinates, so the window under the pointer is unknown",
            );
        };
        gate!(aim(d, allowed, "Click", px, py));
    } else if let (Some(px), Some(py)) = (x, y) {
        if let Err(e) = move_cursor_abs(d, px, py) {
            return ActionResult::err("Click", ErrorKind::ExecutionFailed, e, 0);
        }
    }
    match d.run("wlrctl", &["pointer", "click", btn]) {
        Ok(_) => {
            update_rate_limit(limiter).await;
            ActionResult::ok(
                "Click",
                ActionOutput::MouseClicked {
                    x,
                    y,
                    button: btn.to_string(),
                    double: false,
                },
                0,
            )
        }
        Err(e) => ActionResult::err("Click", ErrorKind::ExecutionFailed, e, 0),
    }
}

pub async fn double_click(
    x: Option<i32>,
    y: Option<i32>,
    config: &AppConfig,
    limiter: &RateLimiter,
) -> ActionResult {
    double_click_on(&Sway, x, y, config, limiter).await
}

pub(crate) async fn double_click_on(
    d: &dyn Backend,
    x: Option<i32>,
    y: Option<i32>,
    config: &AppConfig,
    limiter: &RateLimiter,
) -> ActionResult {
    if !check_rate_limit(limiter, config.desktop.input_rate_limit_ms).await {
        return ActionResult::err(
            "DoubleClick",
            ErrorKind::PolicyDenied,
            "Input rate limit exceeded.",
            0,
        );
    }
    if let Err(r) = d.require("wlrctl") {
        return r;
    }
    if let Some(allowed) = allowed_list(config) {
        let (Some(px), Some(py)) = (x, y) else {
            return refuse(
                "DoubleClick",
                "no coordinates, so the window under the pointer is unknown",
            );
        };
        gate!(aim(d, allowed, "DoubleClick", px, py));
        let _ = d.run("wlrctl", &["pointer", "click", "left"]);
        gate!(point_allowed(d, allowed, "DoubleClick", px, py));
        let _ = d.run("wlrctl", &["pointer", "click", "left"]);
    } else {
        if let (Some(px), Some(py)) = (x, y) {
            let _ = move_cursor_abs(d, px, py);
        }
        let _ = d.run("wlrctl", &["pointer", "click", "left"]);
        let _ = d.run("wlrctl", &["pointer", "click", "left"]);
    }
    update_rate_limit(limiter).await;
    ActionResult::ok(
        "DoubleClick",
        ActionOutput::MouseClicked {
            x,
            y,
            button: "left".into(),
            double: true,
        },
        0,
    )
}

pub async fn scroll(
    direction: &str,
    amount: Option<i32>,
    config: &AppConfig,
    limiter: &RateLimiter,
) -> ActionResult {
    scroll_on(&Sway, direction, amount, config, limiter).await
}

pub(crate) async fn scroll_on(
    d: &dyn Backend,
    direction: &str,
    amount: Option<i32>,
    config: &AppConfig,
    limiter: &RateLimiter,
) -> ActionResult {
    if !check_rate_limit(limiter, config.desktop.input_rate_limit_ms).await {
        return ActionResult::err(
            "Scroll",
            ErrorKind::PolicyDenied,
            "Input rate limit exceeded.",
            0,
        );
    }
    if let Err(r) = d.require("wlrctl") {
        return r;
    }
    let n = amount.unwrap_or(3);
    if let Some(allowed) = allowed_list(config) {
        // Wheel events go to the window under the pointer, whose position is
        // unknown: scroll at the centre of the focused listed window instead.
        let w = gate!(focused_allowed(d, allowed, "Scroll"));
        let (cx, cy) = (w.x + w.width / 2, w.y + w.height / 2);
        gate!(aim(d, allowed, "Scroll", cx, cy));
        for step in 0..n {
            if step > 0 {
                gate!(point_allowed(d, allowed, "Scroll", cx, cy));
            }
            let _ = d.run("wlrctl", &["pointer", "scroll", direction, "1"]);
        }
    } else {
        for _ in 0..n {
            let _ = d.run("wlrctl", &["pointer", "scroll", direction, "1"]);
        }
    }
    update_rate_limit(limiter).await;
    ActionResult::ok(
        "Scroll",
        ActionOutput::Scrolled {
            direction: direction.to_string(),
            amount: n,
        },
        0,
    )
}

// ── Keyboard ──────────────────────────────────────────────────────────────────

pub async fn type_text(text: &str, config: &AppConfig, limiter: &RateLimiter) -> ActionResult {
    type_text_on(&Sway, text, config, limiter).await
}

pub(crate) async fn type_text_on(
    d: &dyn Backend,
    text: &str,
    config: &AppConfig,
    limiter: &RateLimiter,
) -> ActionResult {
    if !check_rate_limit(limiter, config.desktop.input_rate_limit_ms).await {
        return ActionResult::err(
            "TypeText",
            ErrorKind::PolicyDenied,
            "Input rate limit exceeded.",
            0,
        );
    }
    if let Err(r) = d.require("wtype") {
        return r;
    }
    if let Some(allowed) = allowed_list(config) {
        let mut target: Option<i64> = None;
        let mut sent = 0;
        for chunk in type_chunks(text) {
            let w = gate!(focused_allowed(d, allowed, "TypeText"));
            if target.is_some_and(|id| id != w.id) {
                return refuse(
                    "TypeText",
                    format!(
                        "the focus moved to another window after {sent} of {} bytes",
                        text.len()
                    ),
                );
            }
            target = Some(w.id);
            if let Err(e) = d.run("wtype", &[chunk]) {
                return ActionResult::err("TypeText", ErrorKind::ExecutionFailed, e, 0);
            }
            sent += chunk.len();
        }
        if let Some(id) = target {
            gate!(still_focused(d, allowed, "TypeText", id));
        }
        update_rate_limit(limiter).await;
        return ActionResult::ok(
            "TypeText",
            ActionOutput::TextTyped { length: text.len() },
            0,
        );
    }
    match d.run("wtype", &[text]) {
        Ok(_) => {
            update_rate_limit(limiter).await;
            ActionResult::ok(
                "TypeText",
                ActionOutput::TextTyped { length: text.len() },
                0,
            )
        }
        Err(e) => ActionResult::err("TypeText", ErrorKind::ExecutionFailed, e, 0),
    }
}

pub async fn press_key(key: &str, config: &AppConfig, limiter: &RateLimiter) -> ActionResult {
    press_key_on(&Sway, key, config, limiter).await
}

pub(crate) async fn press_key_on(
    d: &dyn Backend,
    key: &str,
    config: &AppConfig,
    limiter: &RateLimiter,
) -> ActionResult {
    if !check_rate_limit(limiter, config.desktop.input_rate_limit_ms).await {
        return ActionResult::err(
            "PressKey",
            ErrorKind::PolicyDenied,
            "Input rate limit exceeded.",
            0,
        );
    }
    if let Err(r) = d.require("wtype") {
        return r;
    }
    let target = match allowed_list(config) {
        Some(allowed) => {
            if compositor_key(key) {
                return refuse("PressKey", format!("'{key}' is a compositor key"));
            }
            Some((allowed, gate!(focused_allowed(d, allowed, "PressKey")).id))
        }
        None => None,
    };
    match d.run("wtype", &["-k", key]) {
        Ok(_) => {
            if let Some((allowed, id)) = target {
                gate!(still_focused(d, allowed, "PressKey", id));
            }
            update_rate_limit(limiter).await;
            ActionResult::ok(
                "PressKey",
                ActionOutput::KeyPressed {
                    keys: vec![key.to_string()],
                },
                0,
            )
        }
        Err(e) => ActionResult::err("PressKey", ErrorKind::ExecutionFailed, e, 0),
    }
}

pub async fn press_key_combination(
    keys: &[String],
    config: &AppConfig,
    limiter: &RateLimiter,
) -> ActionResult {
    press_key_combination_on(&Sway, keys, config, limiter).await
}

pub(crate) async fn press_key_combination_on(
    d: &dyn Backend,
    keys: &[String],
    config: &AppConfig,
    limiter: &RateLimiter,
) -> ActionResult {
    if !check_rate_limit(limiter, config.desktop.input_rate_limit_ms).await {
        return ActionResult::err(
            "PressKeyCombination",
            ErrorKind::PolicyDenied,
            "Input rate limit exceeded.",
            0,
        );
    }
    if let Err(r) = d.require("wtype") {
        return r;
    }
    let target = match allowed_list(config) {
        Some(allowed) => {
            if let Some(k) = keys.iter().find(|k| compositor_key(k)) {
                return refuse("PressKeyCombination", format!("'{k}' is a compositor key"));
            }
            Some((
                allowed,
                gate!(focused_allowed(d, allowed, "PressKeyCombination")).id,
            ))
        }
        None => None,
    };
    // wtype -M ctrl -k s
    let modifiers = ["ctrl", "alt", "shift", "super", "meta"];
    let mut args: Vec<String> = Vec::new();
    let mut main_key: Option<&str> = None;
    for k in keys {
        if modifiers.contains(&k.as_str()) {
            args.push("-M".into());
            args.push(k.clone());
        } else {
            main_key = Some(k.as_str());
        }
    }
    if let Some(k) = main_key {
        args.push("-k".into());
        args.push(k.to_string());
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match d.run("wtype", &arg_refs) {
        Ok(_) => {
            if let Some((allowed, id)) = target {
                gate!(still_focused(d, allowed, "PressKeyCombination", id));
            }
            update_rate_limit(limiter).await;
            ActionResult::ok(
                "PressKeyCombination",
                ActionOutput::KeyPressed {
                    keys: keys.to_vec(),
                },
                0,
            )
        }
        Err(e) => ActionResult::err("PressKeyCombination", ErrorKind::ExecutionFailed, e, 0),
    }
}

// ── Windows ───────────────────────────────────────────────────────────────────

pub async fn focus_window(match_str: &str, config: &AppConfig) -> ActionResult {
    focus_window_on(&Sway, match_str, config).await
}

pub(crate) async fn focus_window_on(
    d: &dyn Backend,
    match_str: &str,
    config: &AppConfig,
) -> ActionResult {
    if let Err(r) = d.require("swaymsg") {
        return r;
    }
    // Security: sanitize match_str to prevent sway IPC criteria injection.
    // Only allow safe characters (alphanumeric, space, hyphen, underscore, dot).
    let sanitized: String = match_str
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_' || *c == '.')
        .collect();
    if sanitized.is_empty() {
        return ActionResult::err(
            "FocusWindow",
            ErrorKind::InvalidInput,
            "Window match string is empty after sanitization.",
            0,
        );
    }
    if let Some(allowed) = allowed_list(config) {
        return focus_listed(d, allowed, &sanitized);
    }
    // Try title match first
    let criteria = format!("[title=\".*{}.*\"]", sanitized);
    let cmd = format!("{} focus", criteria);
    match d.run("swaymsg", &[&cmd]) {
        Ok(_) => ActionResult::ok(
            "FocusWindow",
            ActionOutput::WindowFocused {
                window_name: sanitized.clone(),
                window_id: 0,
            },
            0,
        ),
        Err(_) => {
            // Try app_id match
            let criteria2 = format!("[app_id=\".*{}.*\"]", sanitized);
            let cmd2 = format!("{} focus", criteria2);
            match d.run("swaymsg", &[&cmd2]) {
                Ok(_) => ActionResult::ok(
                    "FocusWindow",
                    ActionOutput::WindowFocused {
                        window_name: sanitized,
                        window_id: 0,
                    },
                    0,
                ),
                Err(e) => ActionResult::err(
                    "FocusWindow",
                    ErrorKind::ExecutionFailed,
                    format!("No window matching '{}': {}", sanitized, e),
                    0,
                ),
            }
        }
    }
}

/// FocusWindow under `allowed_apps`: pick a listed view by its tree id, so
/// the sway criteria can never select an unlisted window, then confirm it.
fn focus_listed(d: &dyn Backend, allowed: &[String], wanted: &str) -> ActionResult {
    let views = gate!(read_views(d, "FocusWindow"));
    let needle = wanted.to_lowercase();
    let matching: Vec<&WindowInfo> = views
        .iter()
        .filter(|w| {
            w.name.to_lowercase().contains(&needle) || w.app_id.to_lowercase().contains(&needle)
        })
        .collect();
    let Some(target) = matching.iter().find(|w| is_allowed(allowed, w)) else {
        if matching.is_empty() {
            return ActionResult::err(
                "FocusWindow",
                ErrorKind::ExecutionFailed,
                format!("No window matching '{wanted}'"),
                0,
            );
        }
        let apps: Vec<&str> = matching.iter().map(|w| label(w)).collect();
        return refuse(
            "FocusWindow",
            format!("'{wanted}' matches only {}", apps.join(", ")),
        );
    };
    let cmd = format!("[con_id={}] focus", target.id);
    if let Err(e) = d.run("swaymsg", &[&cmd]) {
        return ActionResult::err("FocusWindow", ErrorKind::ExecutionFailed, e, 0);
    }
    gate!(still_focused(d, allowed, "FocusWindow", target.id));
    ActionResult::ok(
        "FocusWindow",
        ActionOutput::WindowFocused {
            window_name: target.name.clone(),
            window_id: target.id,
        },
        0,
    )
}

// ── Notifications / Clipboard ─────────────────────────────────────────────────

pub async fn send_notification(title: &str, body: &str, urgency: Option<&str>) -> ActionResult {
    if let Err(r) = check_binary("notify-send") {
        return r;
    }
    let urg = urgency.unwrap_or("normal");
    match run_cmd("notify-send", &["-u", urg, title, body]) {
        Ok(_) => ActionResult::ok(
            "SendNotification",
            ActionOutput::NotificationSent { sent: true },
            0,
        ),
        Err(e) => ActionResult::err("SendNotification", ErrorKind::ExecutionFailed, e, 0),
    }
}

pub async fn get_clipboard(config: &AppConfig) -> ActionResult {
    get_clipboard_on(&Sway, config).await
}

pub(crate) async fn get_clipboard_on(d: &dyn Backend, config: &AppConfig) -> ActionResult {
    // The clipboard holds what any app copied last, a password manager
    // included; the focused window says nothing about where it came from.
    if allowed_list(config).is_some() {
        return refuse("GetClipboard", "the clipboard is shared by every app");
    }
    if let Err(r) = d.require("wl-paste") {
        return r;
    }
    match d.run("wl-paste", &[]) {
        Ok(content) => {
            let length = content.len();
            ActionResult::ok(
                "GetClipboard",
                ActionOutput::ClipboardContent { content, length },
                0,
            )
        }
        Err(e) => ActionResult::err("GetClipboard", ErrorKind::ExecutionFailed, e, 0),
    }
}

pub async fn set_clipboard(content: &str, config: &AppConfig) -> ActionResult {
    set_clipboard_on(&Sway, content, config).await
}

pub(crate) async fn set_clipboard_on(
    d: &dyn Backend,
    content: &str,
    config: &AppConfig,
) -> ActionResult {
    if let Err(r) = d.require("wl-copy") {
        return r;
    }
    if let Some(allowed) = allowed_list(config) {
        gate!(focused_allowed(d, allowed, "SetClipboard"));
    }
    if let Err(e) = d.write_clipboard(content) {
        return ActionResult::err("SetClipboard", ErrorKind::ExecutionFailed, e, 0);
    }
    ActionResult::ok(
        "SetClipboard",
        ActionOutput::ClipboardSet {
            set: true,
            length: content.len(),
        },
        0,
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::policy::{DenyReason, PolicyEffect, PolicyEngine};

    #[test]
    fn desktop_disabled_blocks_screenshot_at_policy_level() {
        // desktop.enabled = false is checked in policy.rs, not here.
        // Confirm the policy rejects all desktop actions when disabled.
        let cfg = AppConfig::defaults(); // desktop disabled by default
        let policy = PolicyEngine::new(cfg.clone(), cfg.actions.clone());
        let action = crate::action::Action::Screenshot {
            output: None,
            region: None,
        };
        assert!(matches!(
            policy.evaluate(&action),
            PolicyEffect::Deny(DenyReason::DesktopDisabled)
        ));
    }

    #[test]
    fn parse_swaymsg_window_tree() {
        // Mock swaymsg JSON output
        let mock_tree = serde_json::json!({
            "type": "root",
            "id": 1,
            "nodes": [{
                "type": "output",
                "id": 2,
                "nodes": [{
                    "type": "workspace",
                    "id": 3,
                    "nodes": [{
                        "type": "con",
                        "id": 4,
                        "name": "Firefox",
                        "app_id": "firefox",
                        "focused": true,
                        "visible": true,
                        "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080}
                    }]
                }]
            }]
        });

        let windows = flatten_tree(&mock_tree);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].name, "Firefox");
        assert_eq!(windows[0].app_id, "firefox");
        assert!(windows[0].focused);
        assert_eq!(windows[0].width, 1920);

        let focused = find_focused(&mock_tree);
        assert!(focused.is_some());
        assert_eq!(focused.unwrap().name, "Firefox");
    }
}

/// `desktop.allowed_apps` attacked gesture by gesture: a window outside the
/// list is put in front by something other than `FocusWindow`, then every
/// input gesture is aimed at it.
#[cfg(test)]
mod allowed_apps_attacks {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    /// Stands in for sway: serves a scripted sequence of trees (the last one
    /// repeats) and records every other command instead of running it.
    struct Fake {
        trees: StdMutex<VecDeque<Result<Value, String>>>,
        last: StdMutex<Option<Result<Value, String>>>,
        calls: StdMutex<Vec<String>>,
        /// Size of the all-white image `capture` writes.
        shot: StdMutex<(u32, u32)>,
    }

    impl Fake {
        fn new(trees: Vec<Result<Value, String>>) -> Self {
            Self {
                trees: StdMutex::new(trees.into()),
                last: StdMutex::new(None),
                calls: StdMutex::new(Vec::new()),
                shot: StdMutex::new((1280, 800)),
            }
        }
        fn one(tree: Value) -> Self {
            Self::new(vec![Ok(tree)])
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        /// Commands that reach a window: input injection, cursor warps,
        /// focus changes, clipboard reads and writes.
        fn injected(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter(|c| !c.starts_with("swaymsg -t get_tree"))
                .collect()
        }
    }

    impl Backend for Fake {
        fn require(&self, _bin: &str) -> Result<(), ActionResult> {
            Ok(())
        }
        fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
            let line = format!("{} {}", program, args.join(" "));
            self.calls.lock().unwrap().push(line);
            if program == "swaymsg" && args == ["-t", "get_tree"] {
                let next = self.trees.lock().unwrap().pop_front();
                let mut last = self.last.lock().unwrap();
                if let Some(t) = next {
                    *last = Some(t);
                }
                return match last.clone().expect("a scripted tree") {
                    Ok(v) => Ok(v.to_string()),
                    Err(e) => Err(e),
                };
            }
            Ok(String::new())
        }
        fn write_clipboard(&self, content: &str) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("wl-copy {}", content.len()));
            Ok(())
        }
        fn capture(&self, args: &[&str]) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("grim {}", args.join(" ")));
            let (w, h) = *self.shot.lock().unwrap();
            let path = args.last().expect("an output file");
            image::RgbaImage::from_pixel(w, h, image::Rgba(WHITE))
                .save(path)
                .map_err(|e| e.to_string())
        }
    }

    struct W {
        id: i64,
        app: &'static str,
        title: &'static str,
        focused: bool,
        rect: (i32, i32, i32, i32),
        floating: bool,
    }

    fn w(id: i64, app: &'static str, focused: bool, rect: (i32, i32, i32, i32)) -> W {
        W {
            id,
            app,
            title: app,
            focused,
            rect,
            floating: false,
        }
    }

    fn node(w: &W) -> Value {
        serde_json::json!({
            "type": if w.floating { "floating_con" } else { "con" },
            "id": w.id,
            "name": w.title,
            "app_id": if w.app.is_empty() { Value::Null } else { Value::from(w.app) },
            "focused": w.focused,
            "visible": true,
            "rect": {"x": w.rect.0, "y": w.rect.1, "width": w.rect.2, "height": w.rect.3},
            "nodes": [],
            "floating_nodes": []
        })
    }

    fn tree(ws: &[W]) -> Value {
        let tiled: Vec<Value> = ws.iter().filter(|w| !w.floating).map(node).collect();
        let floating: Vec<Value> = ws.iter().filter(|w| w.floating).map(node).collect();
        serde_json::json!({
            "type": "root", "id": 1, "name": "root",
            "rect": {"x": 0, "y": 0, "width": 1280, "height": 800},
            "nodes": [{
                "type": "output", "id": 2, "name": "HEADLESS-1",
                "rect": {"x": 0, "y": 0, "width": 1280, "height": 800},
                "nodes": [{
                    "type": "workspace", "id": 3, "name": "1",
                    "focused": ws.iter().all(|w| !w.focused),
                    "nodes": tiled,
                    "floating_nodes": floating
                }]
            }]
        })
    }

    const FULL: (i32, i32, i32, i32) = (0, 0, 1280, 800);
    const LEFT: (i32, i32, i32, i32) = (0, 0, 640, 800);
    const RIGHT: (i32, i32, i32, i32) = (640, 0, 640, 800);

    /// The user opened a terminal and it holds the focus; firefox is the only
    /// allowed app.
    fn terminal_in_front() -> Value {
        tree(&[w(10, "kitty", true, FULL)])
    }

    fn firefox_in_front() -> Value {
        tree(&[w(20, "firefox", true, FULL)])
    }

    fn confined() -> AppConfig {
        let mut cfg = AppConfig::defaults();
        cfg.desktop.enabled = true;
        cfg.desktop.input_rate_limit_ms = 0;
        cfg.desktop.allowed_apps = vec!["firefox".into()];
        cfg
    }

    fn denied(r: &ActionResult) -> bool {
        !r.success
            && r.error
                .as_ref()
                .is_some_and(|e| e.kind == ErrorKind::PolicyDenied)
    }

    fn lim() -> RateLimiter {
        new_rate_limiter()
    }

    /// Every input gesture, aimed at whatever the tree says is in front.
    async fn every_gesture(d: &Fake, cfg: &AppConfig) -> Vec<(&'static str, ActionResult)> {
        vec![
            (
                "Click",
                click_on(d, Some(100), Some(100), None, cfg, &lim()).await,
            ),
            (
                "DoubleClick",
                double_click_on(d, Some(100), Some(100), cfg, &lim()).await,
            ),
            ("MoveMouse", move_mouse_on(d, 100, 100, cfg, &lim()).await),
            ("Scroll", scroll_on(d, "down", Some(2), cfg, &lim()).await),
            ("TypeText", type_text_on(d, "rm -rf ~\n", cfg, &lim()).await),
            ("PressKey", press_key_on(d, "Return", cfg, &lim()).await),
            (
                "PressKeyCombination",
                press_key_combination_on(d, &["ctrl".into(), "w".into()], cfg, &lim()).await,
            ),
            ("SetClipboard", set_clipboard_on(d, "payload", cfg).await),
            ("GetClipboard", get_clipboard_on(d, cfg).await),
        ]
    }

    // ── Reads: Screenshot, GetWindowTree, GetFocusedWindow ──────────────────

    const WHITE: [u8; 4] = [255, 255, 255, 255];
    const BLACK: [u8; 4] = [0, 0, 0, 255];

    fn shots(dir: &std::path::Path) -> AppConfig {
        let mut cfg = confined();
        cfg.desktop.screenshot_dir = dir.to_string_lossy().into_owned();
        cfg
    }

    fn files(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// The returned image, and that it is the only file left behind.
    fn shot_of(r: &ActionResult, dir: &std::path::Path) -> image::RgbaImage {
        let Some(ActionOutput::ScreenshotTaken { path, .. }) = &r.output else {
            panic!("no screenshot: {r:?}");
        };
        assert_eq!(files(dir).len(), 1, "files left: {:?}", files(dir));
        image::open(path).unwrap().to_rgba8()
    }

    fn px(img: &image::RgbaImage, x: u32, y: u32) -> [u8; 4] {
        img.get_pixel(x, y).0
    }

    fn side_by_side() -> Value {
        tree(&[
            w(20, "firefox", true, LEFT),
            W {
                title: "~/notes/passwords.txt - vim",
                ..w(10, "kitty", false, RIGHT)
            },
        ])
    }

    #[tokio::test]
    async fn a_screenshot_shows_only_the_listed_windows() {
        let dir = tempfile::tempdir().unwrap();
        let r = screenshot_on(&Fake::one(side_by_side()), None, None, &shots(dir.path())).await;
        let img = shot_of(&r, dir.path());
        assert_eq!(px(&img, 100, 100), WHITE, "firefox masked");
        assert_eq!(px(&img, 639, 799), WHITE, "firefox edge masked");
        assert_eq!(px(&img, 640, 0), BLACK, "kitty visible");
        assert_eq!(px(&img, 1279, 799), BLACK, "kitty visible");
    }

    #[tokio::test]
    async fn a_floating_unlisted_window_over_a_listed_one_is_masked() {
        let dir = tempfile::tempdir().unwrap();
        let t = tree(&[
            w(20, "firefox", true, FULL),
            W {
                floating: true,
                ..w(10, "kitty", false, (100, 100, 200, 200))
            },
        ]);
        let r = screenshot_on(&Fake::one(t), None, None, &shots(dir.path())).await;
        let img = shot_of(&r, dir.path());
        assert_eq!(px(&img, 150, 150), BLACK, "floating kitty visible");
        assert_eq!(px(&img, 600, 600), WHITE);
    }

    #[tokio::test]
    async fn an_unreadable_tree_refuses_the_screenshot_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let d = Fake::new(vec![Err("ipc down".into())]);
        let r = screenshot_on(&d, None, None, &shots(dir.path())).await;
        assert!(denied(&r), "{r:?}");
        assert!(
            d.calls().iter().all(|c| !c.starts_with("grim")),
            "{:?}",
            d.calls()
        );
        assert!(files(dir.path()).is_empty(), "{:?}", files(dir.path()));

        let d = Fake::new(vec![Ok(side_by_side()), Err("ipc down".into())]);
        let r = screenshot_on(&d, None, None, &shots(dir.path())).await;
        assert!(denied(&r), "{r:?}");
        assert!(files(dir.path()).is_empty(), "{:?}", files(dir.path()));

        // A tree without geometry cannot be mapped onto the image.
        let mut t = side_by_side();
        t.as_object_mut().unwrap().remove("rect");
        let r = screenshot_on(&Fake::one(t), None, None, &shots(dir.path())).await;
        assert!(denied(&r), "{r:?}");
        assert!(files(dir.path()).is_empty(), "{:?}", files(dir.path()));
    }

    #[tokio::test]
    async fn a_window_moved_during_the_capture_is_not_shown() {
        let dir = tempfile::tempdir().unwrap();
        let swapped = tree(&[w(20, "firefox", true, RIGHT), w(10, "kitty", false, LEFT)]);
        let d = Fake::new(vec![Ok(side_by_side()), Ok(swapped)]);
        let r = screenshot_on(&d, None, None, &shots(dir.path())).await;
        let img = shot_of(&r, dir.path());
        for (x, y) in [(100, 100), (900, 100)] {
            assert_eq!(px(&img, x, y), BLACK, "({x}, {y}) shown after a move");
        }
        std::fs::remove_dir_all(dir.path()).unwrap();

        // Firefox closed during the capture: what replaced it is not in the tree.
        let d = Fake::new(vec![
            Ok(tree(&[w(20, "firefox", true, FULL)])),
            Ok(tree(&[])),
        ]);
        let img = shot_of(
            &screenshot_on(&d, None, None, &shots(dir.path())).await,
            dir.path(),
        );
        assert_eq!(px(&img, 100, 100), BLACK, "a closed window's area shown");
    }

    #[tokio::test]
    async fn what_no_listed_window_covers_stays_black() {
        let dir = tempfile::tempdir().unwrap();
        let t = tree(&[w(20, "firefox", true, LEFT)]);
        let img = shot_of(
            &screenshot_on(&Fake::one(t), None, None, &shots(dir.path())).await,
            dir.path(),
        );
        assert_eq!(px(&img, 100, 100), WHITE);
        assert_eq!(px(&img, 900, 100), BLACK, "wallpaper, bar or overlay shown");
    }

    #[tokio::test]
    async fn the_mask_follows_scale_region_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = shots(dir.path());
        // Scale 2: grim writes 2560x1600 for a 1280x800 layout.
        let d = Fake::one(side_by_side());
        *d.shot.lock().unwrap() = (2560, 1600);
        let img = shot_of(&screenshot_on(&d, None, None, &cfg).await, dir.path());
        assert_eq!(px(&img, 1270, 200), WHITE);
        assert_eq!(px(&img, 1290, 200), BLACK, "kitty visible at scale 2");
        std::fs::remove_dir_all(dir.path()).unwrap();

        // A region over kitty only.
        let d = Fake::one(side_by_side());
        *d.shot.lock().unwrap() = (640, 800);
        let r = screenshot_on(&d, None, Some("640,0 640x800"), &cfg).await;
        let img = shot_of(&r, dir.path());
        assert_eq!(px(&img, 10, 10), BLACK, "kitty visible through a region");
        std::fs::remove_dir_all(dir.path()).unwrap();

        // An output by name.
        let d = Fake::one(side_by_side());
        let r = screenshot_on(&d, Some("HEADLESS-1"), None, &cfg).await;
        let img = shot_of(&r, dir.path());
        assert_eq!(px(&img, 900, 10), BLACK, "kitty visible through -o");
        std::fs::remove_dir_all(dir.path()).unwrap();
        let r = screenshot_on(&Fake::one(side_by_side()), Some("NOPE-9"), None, &cfg).await;
        assert!(denied(&r), "{r:?}");
    }

    #[tokio::test]
    async fn an_empty_list_leaves_the_screenshot_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = shots(dir.path());
        cfg.desktop.allowed_apps.clear();
        let r = screenshot_on(&Fake::one(side_by_side()), None, None, &cfg).await;
        let Some(ActionOutput::ScreenshotTaken { path, .. }) = &r.output else {
            panic!("{r:?}");
        };
        let img = image::open(path).unwrap().to_rgba8();
        assert_eq!(px(&img, 900, 100), WHITE);
    }

    #[tokio::test]
    async fn window_reads_do_not_reveal_unlisted_titles() {
        let cfg = confined();
        let r = get_window_tree_on(&Fake::one(side_by_side()), &cfg).await;
        let Some(ActionOutput::WindowTree { windows }) = &r.output else {
            panic!("{r:?}");
        };
        let text = serde_json::to_string(windows).unwrap();
        assert!(!text.contains("passwords"), "{text}");
        assert!(windows.iter().any(|w| w.app_id == "kitty"), "{text}");
        assert!(windows.iter().any(|w| w.name == "firefox"), "{text}");

        let t = tree(&[W {
            title: "~/notes/passwords.txt - vim",
            ..w(10, "kitty", true, FULL)
        }]);
        let r = get_focused_window_on(&Fake::one(t), &cfg).await;
        let text = serde_json::to_string(&r.output).unwrap();
        assert!(!text.contains("passwords"), "{text}");
        assert!(text.contains("kitty"), "{text}");
    }

    #[tokio::test]
    async fn every_gesture_is_refused_on_an_app_outside_the_list() {
        let cfg = confined();
        for (name, r) in every_gesture(&Fake::one(terminal_in_front()), &cfg).await {
            assert!(
                denied(&r),
                "{name} reached kitty with allowed_apps=[firefox]: {r:?}"
            );
        }
        // One fake per gesture, so a gesture that injected is named.
        let d = Fake::one(terminal_in_front());
        let _ = type_text_on(&d, "id\n", &cfg, &lim()).await;
        let _ = click_on(&d, Some(5), Some(5), None, &cfg, &lim()).await;
        let _ = scroll_on(&d, "down", Some(1), &cfg, &lim()).await;
        let _ = set_clipboard_on(&d, "x", &cfg).await;
        assert!(d.injected().is_empty(), "injected: {:?}", d.injected());
    }

    #[tokio::test]
    async fn the_list_lets_every_gesture_through_to_an_allowed_app() {
        let cfg = confined();
        for (name, r) in every_gesture(&Fake::one(firefox_in_front()), &cfg).await {
            if name == "GetClipboard" {
                continue; // refused while the list is set, see its own test
            }
            assert!(r.success, "{name} refused on firefox: {r:?}");
        }
    }

    #[tokio::test]
    async fn an_empty_list_keeps_the_old_behaviour() {
        let mut cfg = confined();
        cfg.desktop.allowed_apps.clear();
        for (name, r) in every_gesture(&Fake::one(terminal_in_front()), &cfg).await {
            assert!(r.success, "{name} refused with no allowed_apps: {r:?}");
        }
    }

    #[tokio::test]
    async fn a_click_on_an_unlisted_window_beside_the_focused_one_is_refused() {
        let cfg = confined();
        let split = || tree(&[w(20, "firefox", true, LEFT), w(10, "kitty", false, RIGHT)]);
        let d = Fake::one(split());
        let r = click_on(&d, Some(900), Some(400), None, &cfg, &lim()).await;
        assert!(denied(&r), "click landed on kitty: {r:?}");
        let r = move_mouse_on(&d, 900, 400, &cfg, &lim()).await;
        assert!(denied(&r), "pointer moved onto kitty: {r:?}");
        assert!(d.injected().is_empty(), "injected: {:?}", d.injected());

        let d = Fake::one(split());
        let r = click_on(&d, Some(100), Some(400), None, &cfg, &lim()).await;
        assert!(r.success, "click inside firefox refused: {r:?}");
    }

    #[tokio::test]
    async fn a_floating_unlisted_window_over_the_allowed_one_is_refused() {
        let cfg = confined();
        let mut over = w(10, "kitty", false, (100, 100, 300, 300));
        over.floating = true;
        let d = Fake::one(tree(&[w(20, "firefox", true, FULL), over]));
        let r = click_on(&d, Some(200), Some(200), None, &cfg, &lim()).await;
        assert!(denied(&r), "click went through the floating kitty: {r:?}");
        assert!(d.injected().is_empty(), "injected: {:?}", d.injected());
    }

    #[tokio::test]
    async fn a_click_with_no_coordinates_is_refused_where_the_pointer_is_unknown() {
        let cfg = confined();
        let d = Fake::one(firefox_in_front());
        let r = click_on(&d, None, None, None, &cfg, &lim()).await;
        assert!(denied(&r), "click at an unknown pointer position: {r:?}");
        let r = double_click_on(&d, None, Some(3), &cfg, &lim()).await;
        assert!(denied(&r), "double click at an unknown position: {r:?}");
        assert!(d.injected().is_empty(), "injected: {:?}", d.injected());
    }

    #[tokio::test]
    async fn a_window_without_identity_is_refused() {
        let cfg = confined();
        let cases = [
            ("no app_id", tree(&[w(10, "", true, FULL)])),
            ("empty workspace focused", tree(&[])),
        ];
        for (what, t) in cases {
            let d = Fake::one(t);
            let r = type_text_on(&d, "x", &cfg, &lim()).await;
            assert!(denied(&r), "{what}: typed anyway: {r:?}");
            let r = click_on(&d, Some(1), Some(1), None, &cfg, &lim()).await;
            assert!(denied(&r), "{what}: clicked anyway: {r:?}");
            assert!(d.injected().is_empty(), "{what}: {:?}", d.injected());
        }
        let d = Fake::new(vec![Err("swaymsg: unable to connect".into())]);
        let r = press_key_on(&d, "Return", &cfg, &lim()).await;
        assert!(denied(&r), "unreadable tree: key pressed anyway: {r:?}");
        assert!(d.injected().is_empty(), "injected: {:?}", d.injected());
    }

    #[tokio::test]
    async fn a_title_naming_an_allowed_app_is_not_an_identity() {
        let cfg = confined();
        let mut spoof = w(10, "kitty", true, FULL);
        spoof.title = "firefox";
        let d = Fake::one(tree(&[spoof]));
        let r = type_text_on(&d, "id\n", &cfg, &lim()).await;
        assert!(denied(&r), "a kitty titled firefox was typed into: {r:?}");
        assert!(d.injected().is_empty(), "injected: {:?}", d.injected());
    }

    #[tokio::test]
    async fn focus_window_never_targets_an_app_outside_the_list() {
        let cfg = confined();
        let d = Fake::one(tree(&[
            w(20, "firefox", true, LEFT),
            w(10, "kitty", false, RIGHT),
        ]));
        let r = focus_window_on(&d, "kitty", &cfg).await;
        assert!(denied(&r), "focused kitty: {r:?}");
        assert!(d.injected().is_empty(), "injected: {:?}", d.injected());

        // A kitty titled "firefox" must not be the one picked.
        let mut spoof = w(10, "kitty", false, LEFT);
        spoof.title = "firefox";
        let d = Fake::new(vec![
            Ok(tree(&[spoof, w(20, "firefox", false, RIGHT)])),
            Ok(tree(&[
                w(10, "kitty", false, LEFT),
                w(20, "firefox", true, RIGHT),
            ])),
        ]);
        let r = focus_window_on(&d, "firefox", &cfg).await;
        assert!(r.success, "{r:?}");
        assert_eq!(d.injected(), vec!["swaymsg [con_id=20] focus".to_string()]);
    }

    #[tokio::test]
    async fn focus_window_reports_a_focus_that_did_not_land() {
        let cfg = confined();
        let d = Fake::new(vec![
            Ok(tree(&[
                w(10, "kitty", true, LEFT),
                w(20, "firefox", false, RIGHT),
            ])),
            Ok(tree(&[
                w(10, "kitty", true, LEFT),
                w(20, "firefox", false, RIGHT),
            ])),
        ]);
        let r = focus_window_on(&d, "firefox", &cfg).await;
        assert!(!r.success, "focus stayed on kitty yet reported ok: {r:?}");
    }

    #[tokio::test]
    async fn compositor_shortcuts_are_refused_even_on_an_allowed_app() {
        let cfg = confined();
        let d = Fake::one(firefox_in_front());
        let combos: [&[&str]; 3] = [
            &["super", "Return"],
            &["meta", "d"],
            &["super", "shift", "e"],
        ];
        for keys in combos {
            let keys: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
            let r = press_key_combination_on(&d, &keys, &cfg, &lim()).await;
            assert!(denied(&r), "{keys:?} reached the compositor: {r:?}");
        }
        for key in ["Super_L", "Super_R", "Meta_L", "Hyper_L", "XF86PowerOff"] {
            let r = press_key_on(&d, key, &cfg, &lim()).await;
            assert!(denied(&r), "{key} reached the compositor: {r:?}");
        }
        assert!(d.injected().is_empty(), "injected: {:?}", d.injected());
    }

    #[tokio::test]
    async fn get_clipboard_is_refused_while_the_list_is_set() {
        // The clipboard holds whatever any app copied last (a password
        // manager included); the focused window says nothing about its origin.
        let cfg = confined();
        let d = Fake::one(firefox_in_front());
        let r = get_clipboard_on(&d, &cfg).await;
        assert!(denied(&r), "{r:?}");
        assert!(d.injected().is_empty(), "injected: {:?}", d.injected());
    }

    // ── Races: the window changes between the check and the gesture ──────────

    #[tokio::test]
    async fn typing_stops_when_the_focus_moves_mid_text() {
        let cfg = confined();
        let long = "a".repeat(TYPE_CHUNK * 3);
        let d = Fake::new(vec![Ok(firefox_in_front()), Ok(terminal_in_front())]);
        let r = type_text_on(&d, &long, &cfg, &lim()).await;
        assert!(!r.success, "kept typing after the focus moved: {r:?}");
        let typed: Vec<String> = d
            .injected()
            .into_iter()
            .filter(|c| c.starts_with("wtype"))
            .collect();
        assert_eq!(typed.len(), 1, "chunks typed into kitty: {typed:?}");
    }

    #[tokio::test]
    async fn a_focus_change_during_a_short_gesture_is_reported() {
        let cfg = confined();
        let d = Fake::new(vec![Ok(firefox_in_front()), Ok(terminal_in_front())]);
        let r = press_key_on(&d, "Return", &cfg, &lim()).await;
        assert!(
            !r.success,
            "focus moved under the key press, reported ok: {r:?}"
        );
        let d = Fake::new(vec![Ok(firefox_in_front()), Ok(terminal_in_front())]);
        let r = type_text_on(&d, "short", &cfg, &lim()).await;
        assert!(!r.success, "focus moved under the text, reported ok: {r:?}");
    }

    #[tokio::test]
    async fn a_window_moved_under_the_pointer_after_the_warp_gets_no_click() {
        let cfg = confined();
        let d = Fake::new(vec![Ok(firefox_in_front()), Ok(terminal_in_front())]);
        let r = click_on(&d, Some(100), Some(100), None, &cfg, &lim()).await;
        assert!(
            denied(&r),
            "clicked after kitty slid under the pointer: {r:?}"
        );
        let clicks: Vec<String> = d
            .injected()
            .into_iter()
            .filter(|c| c.starts_with("wlrctl pointer click"))
            .collect();
        assert!(clicks.is_empty(), "{clicks:?}");
    }

    #[tokio::test]
    async fn scrolling_stops_when_an_unlisted_window_takes_the_place() {
        let cfg = confined();
        let d = Fake::new(vec![
            Ok(firefox_in_front()),
            Ok(firefox_in_front()),
            Ok(terminal_in_front()),
        ]);
        let r = scroll_on(&d, "down", Some(5), &cfg, &lim()).await;
        assert!(!r.success, "kept scrolling over kitty: {r:?}");
        let n = d
            .injected()
            .into_iter()
            .filter(|c| c.starts_with("wlrctl pointer scroll"))
            .count();
        assert!(n < 5, "{n} scroll steps went out");
    }

    #[test]
    fn chunks_never_start_with_a_dash() {
        // wtype reads a lone "-M"/"-k" argument as an option.
        let text = format!("{}-k{}", "a".repeat(TYPE_CHUNK - 1), "b".repeat(5));
        let chunks = type_chunks(&text);
        assert_eq!(chunks.concat(), text);
        assert!(
            chunks.iter().skip(1).all(|c| !c.starts_with('-')),
            "{chunks:?}"
        );
        let dashes = "-".repeat(TYPE_CHUNK * 2 + 3);
        assert_eq!(type_chunks(&dashes).concat(), dashes);
    }
}
