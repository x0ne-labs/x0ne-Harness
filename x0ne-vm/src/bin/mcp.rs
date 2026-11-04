//! x0ne-vm-mcp — MCP stdio server exposing the [`VmSession`] API.
//!
//! Wire this into Claude Code (or any MCP client) via `.mcp.json`:
//! ```jsonc
//! {
//!   "mcpServers": {
//!     "x0ne-vm": { "command": "x0ne-vm-mcp" }
//!   }
//! }
//! ```
//!
//! Claude can then call `vm_list`, `vm_screenshot`, `vm_click`, `vm_type`,
//! `vm_key`, snapshot ops, etc.

use std::sync::Arc;

use base64::Engine as _;
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use x0ne_mcp::{JsonRpcRequest, JsonRpcResponse, StdioTransport, INVALID_PARAMS, METHOD_NOT_FOUND};
use x0ne_vm::session::{KeyChord, MouseButton, ScrollDirection, VmSession};
use x0ne_vm::{LibvirtSession, WaylandSession};

/// Select backend via `X0NE_VM_BACKEND` env var. Values: `libvirt` (default),
/// `wayland`. When set to `wayland`, the backend respects `X0NE_VM_STARTUP`
/// for auto-launching an app inside the spawned compositor.
fn pick_backend() -> Arc<dyn VmSession> {
    match std::env::var("X0NE_VM_BACKEND").as_deref() {
        Ok("wayland") => {
            let mut s = WaylandSession::new();
            if let Ok(cmd) = std::env::var("X0NE_VM_STARTUP") {
                s = s.with_startup_cmd(cmd);
            }
            tracing::info!(
                "backend=wayland nested={} size={:?} startup={:?}",
                s.nested,
                s.size,
                s.startup_cmd
            );
            Arc::new(s)
        }
        _ => {
            tracing::info!("backend=libvirt (qemu:///session)");
            Arc::new(LibvirtSession::new())
        }
    }
}

fn main() {
    init_tracing();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(run());
}

fn init_tracing() {
    // Log to stderr so MCP stdout stays clean for JSON-RPC.
    use tracing_subscriber::EnvFilter;
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("x0ne_vm=info,info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

async fn run() {
    let session: Arc<dyn VmSession> = pick_backend();
    let mut transport = StdioTransport::new();
    tracing::info!("x0ne-vm-mcp ready on stdio");

    loop {
        let req = match transport.read_message().await {
            Ok(r) => r,
            Err(e) if e.to_string() == "EOF" => break,
            Err(e) => {
                tracing::error!("read JSON-RPC: {e}");
                continue;
            }
        };
        if let Some(resp) = handle(&session, req).await {
            if let Err(e) = transport.write_message(&resp).await {
                tracing::error!("write JSON-RPC: {e}");
                break;
            }
        }
    }
}

async fn handle(session: &Arc<dyn VmSession>, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
    if req.is_notification() {
        return None;
    }
    let id = req.id.clone().unwrap_or(Value::Null);
    match req.method.as_str() {
        "initialize" => Some(JsonRpcResponse::ok(id, initialize_result())),
        "tools/list" => Some(JsonRpcResponse::ok(id, json!({ "tools": tool_specs() }))),
        "tools/call" => Some(handle_call(session, id, &req).await),
        "ping" => Some(JsonRpcResponse::ok(id, json!({}))),
        "resources/list" => Some(JsonRpcResponse::ok(id, json!({ "resources": [] }))),
        "prompts/list" => Some(JsonRpcResponse::ok(id, json!({ "prompts": [] }))),
        _ => Some(JsonRpcResponse::error(
            id,
            METHOD_NOT_FOUND,
            &format!("method not found: {}", req.method),
        )),
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "x0ne-vm",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// F5: tools that only observe. Every other tool is annotated as mutating.
const READ_ONLY_TOOLS: &[&str] = &[
    "vm_list",
    "vm_info",
    "vm_screenshot",
    "vm_cursor_position",
    "vm_wait",
    "vm_find_text",
    "vm_wait_for_text",
    "vm_snapshot_list",
    "vm_pool_status",
];

fn tool_specs() -> Vec<Value> {
    fn s(name: &str, desc: &str, schema: Value) -> Value {
        let hints = x0ne_mcp::tool_hints(READ_ONLY_TOOLS.contains(&name));
        json!({ "name": name, "description": desc, "inputSchema": schema, "annotations": hints })
    }
    fn obj(props: Value, required: &[&str]) -> Value {
        json!({ "type": "object", "properties": props, "required": required })
    }
    let str_prop = json!({ "type": "string" });
    let int_prop = json!({ "type": "integer" });

    vec![
        s(
            "vm_list",
            "List all virtual machines defined on the local libvirt host (running and stopped).",
            obj(json!({}), &[]),
        ),
        s(
            "vm_info",
            "Get detailed info about a VM: state, vcpu, memory, VNC endpoint if running.",
            obj(json!({ "name": str_prop }), &["name"]),
        ),
        s(
            "vm_start",
            "Boot a VM that is currently shut off.",
            obj(json!({ "name": str_prop }), &["name"]),
        ),
        s(
            "vm_shutdown",
            "Send ACPI shutdown to a running VM (graceful).",
            obj(json!({ "name": str_prop }), &["name"]),
        ),
        s(
            "vm_force_off",
            "Hard power-off a VM (virsh destroy). Use only when shutdown is stuck.",
            obj(json!({ "name": str_prop }), &["name"]),
        ),
        s(
            "vm_reboot",
            "Reboot a running VM.",
            obj(json!({ "name": str_prop }), &["name"]),
        ),
        s(
            "vm_screenshot",
            "Capture a PNG screenshot of the VM's primary display. Returns image content directly for vision models.",
            obj(json!({ "name": str_prop }), &["name"]),
        ),
        s(
            "vm_click",
            "Click at framebuffer coordinates (x, y). Optional `button` = left|middle|right (default left). `double=true` for a double-click.",
            obj(
                json!({
                    "name": str_prop,
                    "x": int_prop,
                    "y": int_prop,
                    "button": { "type": "string", "enum": ["left", "middle", "right"] },
                    "double": { "type": "boolean" }
                }),
                &["name", "x", "y"],
            ),
        ),
        s(
            "vm_move",
            "Move mouse cursor to (x, y) without clicking.",
            obj(
                json!({ "name": str_prop, "x": int_prop, "y": int_prop }),
                &["name", "x", "y"],
            ),
        ),
        s(
            "vm_drag",
            "Click-and-drag from (x1, y1) to (x2, y2). Optional `button` (default left).",
            obj(
                json!({
                    "name": str_prop,
                    "x1": int_prop, "y1": int_prop,
                    "x2": int_prop, "y2": int_prop,
                    "button": { "type": "string", "enum": ["left", "middle", "right"] }
                }),
                &["name", "x1", "y1", "x2", "y2"],
            ),
        ),
        s(
            "vm_scroll",
            "Scroll the mouse wheel `clicks` detents in `direction` (up/down/left/right) at the cursor's current position. Default clicks=3.",
            obj(
                json!({
                    "name": str_prop,
                    "direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                    "clicks": { "type": "integer", "minimum": 1, "maximum": 100 }
                }),
                &["name", "direction"],
            ),
        ),
        s(
            "vm_cursor_position",
            "Get the last-known (x, y) of the cursor as tracked by this backend. Returns null if the cursor has never been moved this session.",
            obj(json!({ "name": str_prop }), &["name"]),
        ),
        s(
            "vm_triple_click",
            "Triple-click at (x, y) — selects a whole line in most text widgets. Optional `button` (default left).",
            obj(
                json!({
                    "name": str_prop,
                    "x": int_prop,
                    "y": int_prop,
                    "button": { "type": "string", "enum": ["left", "middle", "right"] }
                }),
                &["name", "x", "y"],
            ),
        ),
        s(
            "vm_wait",
            "Sleep for `ms` milliseconds without doing anything else. Useful between actions when the agent needs the guest to settle (page load, animation).",
            obj(
                json!({
                    "ms": { "type": "integer", "minimum": 1, "maximum": 60000 }
                }),
                &["ms"],
            ),
        ),
        s(
            "vm_find_text",
            "OCR the current screen (tesseract) and return the bounding box of `text` — or null. `occurrence` (0-indexed) picks among multiple matches. No mouse motion.",
            obj(
                json!({
                    "name": str_prop,
                    "text": str_prop,
                    "occurrence": { "type": "integer", "minimum": 0 }
                }),
                &["name", "text"],
            ),
        ),
        s(
            "vm_click_text",
            "OCR the screen and click the center of the matched region. `text` can be a single word or a phrase. `button`/`double`/`occurrence` optional. Returns the matched bbox.",
            obj(
                json!({
                    "name": str_prop,
                    "text": str_prop,
                    "button": { "type": "string", "enum": ["left", "middle", "right"] },
                    "double": { "type": "boolean" },
                    "occurrence": { "type": "integer", "minimum": 0 }
                }),
                &["name", "text"],
            ),
        ),
        s(
            "vm_wait_for_text",
            "Poll OCR every `poll_ms` (default 500) until `text` appears or `timeout_ms` (default 10000) elapses. Returns the matched bbox or fails clearly.",
            obj(
                json!({
                    "name": str_prop,
                    "text": str_prop,
                    "timeout_ms": { "type": "integer", "minimum": 100, "maximum": 120000 },
                    "poll_ms": { "type": "integer", "minimum": 50, "maximum": 5000 }
                }),
                &["name", "text"],
            ),
        ),
        s(
            "vm_type",
            "Type ASCII text into the VM (per-char keystroke synthesis). Guest layout should be US for predictable results.",
            obj(
                json!({ "name": str_prop, "text": str_prop }),
                &["name", "text"],
            ),
        ),
        s(
            "vm_key",
            "Send a key chord like `return`, `ctrl+alt+t`, `shift+f10`, `super+l`.",
            obj(
                json!({ "name": str_prop, "key": str_prop }),
                &["name", "key"],
            ),
        ),
        s(
            "vm_snapshot_create",
            "Create a snapshot of the VM with the given name.",
            obj(
                json!({ "name": str_prop, "snapshot": str_prop }),
                &["name", "snapshot"],
            ),
        ),
        s(
            "vm_snapshot_restore",
            "Revert the VM to a previously created snapshot.",
            obj(
                json!({ "name": str_prop, "snapshot": str_prop }),
                &["name", "snapshot"],
            ),
        ),
        s(
            "vm_snapshot_list",
            "List all snapshots for a VM.",
            obj(json!({ "name": str_prop }), &["name"]),
        ),
        s(
            "vm_snapshot_delete",
            "Delete a named snapshot from a VM.",
            obj(
                json!({ "name": str_prop, "snapshot": str_prop }),
                &["name", "snapshot"],
            ),
        ),
        s(
            "vm_clone",
            "Create a linked clone of `base` as `new_name`. Uses qcow2 copy-on-write so the clone shares the base disk read-only. Foundation for the agent pool: spawn many short-lived agent bodies from one golden image in ~200ms each.",
            obj(
                json!({ "base": str_prop, "new_name": str_prop }),
                &["base", "new_name"],
            ),
        ),
        s(
            "vm_destroy_clone",
            "Tear down a cloned VM: shut it off, undefine, and delete its overlay disk. Safe-by-default: refuses to touch VMs whose disk is outside ~/.x0ne/vm/disks/, so user-defined VMs are never harmed.",
            obj(json!({ "name": str_prop }), &["name"]),
        ),
        s(
            "vm_pool_status",
            "Query the x0ne-vmd pool daemon — returns config + slot states + counts (free/acquired). Errors clearly if the daemon isn't running.",
            obj(json!({}), &[]),
        ),
        s(
            "vm_pool_acquire",
            "Ask the x0ne-vmd pool daemon for a fresh VM body. Returns the assigned VM name + VNC endpoint. The clone is already running (auto-started at pool refill).",
            obj(
                json!({ "by": str_prop }),
                &[],
            ),
        ),
        s(
            "vm_pool_release",
            "Return a pool-acquired VM to the daemon. The slot is destroyed in background and a fresh replacement spawned to keep the pool topped up.",
            obj(json!({ "vm": str_prop }), &["vm"]),
        ),
        s(
            "vm_pool_shutdown",
            "Tell x0ne-vmd to destroy every pool VM and exit. Use with care.",
            obj(json!({}), &[]),
        ),
    ]
}

/// Send one JSON command to the x0ne-vmd Unix socket and return the reply.
async fn vmd_call(req: Value) -> anyhow::Result<Value> {
    let sock = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("no HOME"))?
        .join(".x0ne")
        .join("vm")
        .join("vmd.sock");
    if !sock.exists() {
        anyhow::bail!(
            "x0ne-vmd not running — start it with: ./target/release/x0ne-vmd  (or: bash scripts/x0ne-vm-control-start.sh)"
        );
    }
    let stream = UnixStream::connect(&sock)
        .await
        .map_err(|e| anyhow::anyhow!("connect {:?}: {}", sock, e))?;
    let (rd, mut wr) = stream.into_split();
    let mut payload = serde_json::to_vec(&req)?;
    payload.push(b'\n');
    wr.write_all(&payload).await?;
    wr.flush().await?;
    let mut reader = BufReader::new(rd);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    if line.is_empty() {
        anyhow::bail!("x0ne-vmd closed connection with no reply");
    }
    Ok(serde_json::from_str(&line)?)
}

#[allow(dead_code)]
fn _vmd_socket_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".x0ne/vm/vmd.sock"))
        .unwrap_or_else(|| PathBuf::from("/tmp/vmd.sock"))
}

async fn handle_call(
    session: &Arc<dyn VmSession>,
    id: Value,
    req: &JsonRpcRequest,
) -> JsonRpcResponse {
    let params = match req.params.as_ref() {
        Some(p) => p,
        None => return JsonRpcResponse::error(id, INVALID_PARAMS, "missing params"),
    };
    let tool = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return JsonRpcResponse::error(id, INVALID_PARAMS, "missing tool name"),
    };
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let res = dispatch(session, tool, &args).await;
    match res {
        Ok(content) => JsonRpcResponse::ok(id, json!({ "content": content })),
        Err(e) => JsonRpcResponse::ok(
            id,
            json!({
                "content": [{ "type": "text", "text": format!("x0ne-vm error: {e}") }],
                "isError": true,
            }),
        ),
    }
}

async fn dispatch(
    session: &Arc<dyn VmSession>,
    tool: &str,
    args: &Value,
) -> anyhow::Result<Vec<Value>> {
    match tool {
        "vm_list" => {
            let vms = session.list().await?;
            Ok(vec![text(serde_json::to_string_pretty(&vms)?)])
        }
        "vm_info" => {
            let name = req_name(args, "name")?;
            let info = session.info(&name).await?;
            Ok(vec![text(serde_json::to_string_pretty(&info)?)])
        }
        "vm_start" => {
            session.start(&req_name(args, "name")?).await?;
            Ok(vec![text("started")])
        }
        "vm_shutdown" => {
            session.shutdown(&req_name(args, "name")?).await?;
            Ok(vec![text("shutdown requested")])
        }
        "vm_force_off" => {
            session.force_off(&req_name(args, "name")?).await?;
            Ok(vec![text("force-off complete")])
        }
        "vm_reboot" => {
            session.reboot(&req_name(args, "name")?).await?;
            Ok(vec![text("reboot requested")])
        }
        "vm_screenshot" => {
            let name = req_name(args, "name")?;
            let shot = session.screenshot(&name).await?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&shot.png);
            Ok(vec![
                json!({
                    "type": "image",
                    "data": b64,
                    "mimeType": "image/png",
                }),
                text(format!(
                    "captured {}x{} ({} bytes PNG)",
                    shot.width,
                    shot.height,
                    shot.png.len()
                )),
            ])
        }
        "vm_click" => {
            let name = req_name(args, "name")?;
            let x = req_i32(args, "x")?;
            let y = req_i32(args, "y")?;
            let button = parse_button(args.get("button").and_then(|v| v.as_str()));
            let double = args
                .get("double")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            session.mouse_click(&name, x, y, button, double).await?;
            Ok(vec![text(format!(
                "clicked {button:?} at ({x},{y}){}",
                if double { " (double)" } else { "" }
            ))])
        }
        "vm_move" => {
            let name = req_name(args, "name")?;
            let x = req_i32(args, "x")?;
            let y = req_i32(args, "y")?;
            session.mouse_move(&name, x, y).await?;
            Ok(vec![text(format!("moved to ({x},{y})"))])
        }
        "vm_drag" => {
            let name = req_name(args, "name")?;
            let x1 = req_i32(args, "x1")?;
            let y1 = req_i32(args, "y1")?;
            let x2 = req_i32(args, "x2")?;
            let y2 = req_i32(args, "y2")?;
            let button = parse_button(args.get("button").and_then(|v| v.as_str()));
            session.mouse_drag(&name, x1, y1, x2, y2, button).await?;
            Ok(vec![text(format!(
                "dragged {button:?} ({x1},{y1}) → ({x2},{y2})"
            ))])
        }
        "vm_scroll" => {
            let name = req_name(args, "name")?;
            let dir = args
                .get("direction")
                .and_then(|v| v.as_str())
                .and_then(ScrollDirection::parse)
                .ok_or_else(|| anyhow::anyhow!("missing/invalid 'direction'"))?;
            let clicks = args
                .get("clicks")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32)
                .unwrap_or(3);
            session.mouse_scroll(&name, dir, clicks).await?;
            Ok(vec![text(format!("scrolled {dir:?} × {clicks}"))])
        }
        "vm_cursor_position" => {
            let name = req_name(args, "name")?;
            match session.cursor_position(&name).await? {
                Some((x, y)) => Ok(vec![text(format!("{{\"x\":{x},\"y\":{y}}}"))]),
                None => Ok(vec![text(
                    "{\"x\":null,\"y\":null,\"note\":\"no recorded cursor — call vm_move first\"}",
                )]),
            }
        }
        "vm_triple_click" => {
            let name = req_name(args, "name")?;
            let x = req_i32(args, "x")?;
            let y = req_i32(args, "y")?;
            let button = parse_button(args.get("button").and_then(|v| v.as_str()));
            session.mouse_triple_click(&name, x, y, button).await?;
            Ok(vec![text(format!(
                "triple-clicked {button:?} at ({x},{y})"
            ))])
        }
        "vm_wait" => {
            let ms = args
                .get("ms")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow::anyhow!("missing required arg: ms"))?;
            // Clamp to 60s to keep agents responsive.
            let clamped = ms.min(60_000);
            tokio::time::sleep(std::time::Duration::from_millis(clamped)).await;
            Ok(vec![text(format!("waited {clamped} ms"))])
        }
        "vm_find_text" => {
            let name = req_name(args, "name")?;
            let target = req_str(args, "text")?;
            let occ = args.get("occurrence").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            match session.find_text(&name, &target, occ).await? {
                Some(r) => Ok(vec![text(format!(
                    "{{\"text\":{:?},\"x\":{},\"y\":{},\"w\":{},\"h\":{},\"cx\":{},\"cy\":{},\"conf\":{}}}",
                    r.text, r.x, r.y, r.w, r.h, r.center().0, r.center().1, r.conf
                ))]),
                None => Ok(vec![text(format!(
                    "{{\"found\":false,\"text\":{:?}}}",
                    target
                ))]),
            }
        }
        "vm_click_text" => {
            let name = req_name(args, "name")?;
            let target = req_str(args, "text")?;
            let button = parse_button(args.get("button").and_then(|v| v.as_str()));
            let double = args
                .get("double")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let occ = args.get("occurrence").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let region = session
                .click_text(&name, &target, button, double, occ)
                .await?;
            Ok(vec![text(format!(
                "{{\"clicked\":{:?},\"at\":[{},{}],\"bbox\":[{},{},{},{}],\"conf\":{}}}",
                region.text,
                region.center().0,
                region.center().1,
                region.x,
                region.y,
                region.w,
                region.h,
                region.conf
            ))])
        }
        "vm_wait_for_text" => {
            let name = req_name(args, "name")?;
            let target = req_str(args, "text")?;
            let timeout = args
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(10_000);
            let poll = args.get("poll_ms").and_then(|v| v.as_u64()).unwrap_or(500);
            let region = session.wait_for_text(&name, &target, timeout, poll).await?;
            Ok(vec![text(format!(
                "{{\"appeared\":{:?},\"at\":[{},{}],\"bbox\":[{},{},{},{}]}}",
                region.text,
                region.center().0,
                region.center().1,
                region.x,
                region.y,
                region.w,
                region.h
            ))])
        }
        "vm_type" => {
            let name = req_name(args, "name")?;
            let textv = req_str(args, "text")?;
            session.type_text(&name, &textv).await?;
            Ok(vec![text(format!("typed {} chars", textv.chars().count()))])
        }
        "vm_key" => {
            let name = req_name(args, "name")?;
            let key = req_str(args, "key")?;
            let chord = KeyChord::parse(&key)
                .ok_or_else(|| anyhow::anyhow!("could not parse key chord: {key:?}"))?;
            session.key_chord(&name, &chord).await?;
            Ok(vec![text(format!("sent chord {key}"))])
        }
        "vm_snapshot_create" => {
            let name = req_name(args, "name")?;
            let snap = req_name(args, "snapshot")?;
            session.snapshot_create(&name, &snap).await?;
            Ok(vec![text(format!("snapshot {snap} created"))])
        }
        "vm_snapshot_restore" => {
            let name = req_name(args, "name")?;
            let snap = req_name(args, "snapshot")?;
            session.snapshot_restore(&name, &snap).await?;
            Ok(vec![text(format!("reverted to {snap}"))])
        }
        "vm_snapshot_list" => {
            let name = req_name(args, "name")?;
            let snaps = session.snapshot_list(&name).await?;
            Ok(vec![text(serde_json::to_string_pretty(&snaps)?)])
        }
        "vm_snapshot_delete" => {
            let name = req_name(args, "name")?;
            let snap = req_name(args, "snapshot")?;
            session.snapshot_delete(&name, &snap).await?;
            Ok(vec![text(format!("snapshot {snap} deleted"))])
        }
        "vm_clone" => {
            let base = req_name(args, "base")?;
            let new_name = req_name(args, "new_name")?;
            session.clone_vm(&base, &new_name).await?;
            Ok(vec![text(format!(
                "{{\"cloned\":true,\"base\":{:?},\"new_name\":{:?},\"disk\":\"~/.x0ne/vm/disks/{}.qcow2\"}}",
                base, new_name, new_name
            ))])
        }
        "vm_destroy_clone" => {
            let name = req_name(args, "name")?;
            session.destroy_clone(&name).await?;
            Ok(vec![text(format!("destroyed clone {name}"))])
        }
        "vm_pool_status" => {
            let res = vmd_call(json!({"cmd": "status"})).await?;
            Ok(vec![text(serde_json::to_string_pretty(&res)?)])
        }
        "vm_pool_acquire" => {
            let by = args
                .get("by")
                .and_then(|v| v.as_str())
                .unwrap_or("agent")
                .to_string();
            let res = vmd_call(json!({"cmd": "acquire", "by": by})).await?;
            Ok(vec![text(serde_json::to_string(&res)?)])
        }
        "vm_pool_release" => {
            let vm = req_name(args, "vm")?;
            let res = vmd_call(json!({"cmd": "release", "vm": vm})).await?;
            Ok(vec![text(serde_json::to_string(&res)?)])
        }
        "vm_pool_shutdown" => {
            let res = vmd_call(json!({"cmd": "shutdown"})).await?;
            Ok(vec![text(serde_json::to_string(&res)?)])
        }
        other => anyhow::bail!("unknown tool: {other}"),
    }
}

fn text(s: impl Into<String>) -> Value {
    json!({ "type": "text", "text": s.into() })
}

fn req_str(args: &Value, key: &str) -> anyhow::Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("missing required arg: {key}"))
}

/// A VM or snapshot name from `args`: a word virsh takes as a positional and
/// never as an option. Checked here, before any tool hands it to virsh.
/// (Audit of 2026-10-30, H1-5.)
fn req_name(args: &Value, key: &str) -> anyhow::Result<String> {
    let name = req_str(args, key)?;
    if !x0ne_vm::virsh::is_valid_vm_name(&name) {
        anyhow::bail!(
            "invalid {key}: 1-64 chars of [A-Za-z0-9_-], starting with a letter or a digit"
        );
    }
    Ok(name)
}

fn req_i32(args: &Value, key: &str) -> anyhow::Result<i32> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| anyhow::anyhow!("missing/invalid integer arg: {key}"))
}

fn parse_button(s: Option<&str>) -> MouseButton {
    match s.unwrap_or("left").to_ascii_lowercase().as_str() {
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        _ => MouseButton::Left,
    }
}

#[cfg(test)]
mod annotation_tests {
    /// H1-5: a snapshot or VM name that virsh would read as an option is
    /// refused by the tool, before virsh sees it.
    #[test]
    fn a_name_that_is_a_virsh_option_is_refused() {
        use serde_json::json;
        assert!(
            super::req_name(&json!({"snapshot": "--memspec=file=/tmp/x"}), "snapshot").is_err()
        );
        assert!(super::req_name(&json!({"name": "-help"}), "name").is_err());
        assert_eq!(
            super::req_name(&json!({"name": "ubuntu-24"}), "name").unwrap(),
            "ubuntu-24"
        );
    }

    #[test]
    fn every_tool_is_annotated() {
        let tools = super::tool_specs();
        x0ne_mcp::check_hints(&tools, super::READ_ONLY_TOOLS).unwrap();
    }
}
