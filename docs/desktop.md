# Desktop Automation Setup (Wayland/Sway)

X0NE supports desktop automation on Wayland compositors — specifically **Sway**. This guide covers installation, configuration, and how the vision loop works.

---

## Required tools

X0NE does not bundle any input simulation. It calls external Wayland tools:

| Tool | Used for | Install |
|---|---|---|
| `grim` | Screenshots | `apt install grim` |
| `wlrctl` | Mouse movement, clicks, scroll | `apt install wlrctl` |
| `wtype` | Keyboard typing and key presses | `apt install wtype` |
| `swaymsg` | Window focus, window tree | included with Sway |
| `wl-copy` / `wl-paste` | Clipboard read/write | `apt install wl-clipboard` |
| `notify-send` | Desktop notifications | `apt install libnotify-bin` |

### Installing on Debian/Ubuntu with Sway

```bash
sudo apt install grim wlrctl wtype wl-clipboard libnotify-bin sway
```

### Verify installation

```bash
which grim wtype swaymsg wl-copy wl-paste notify-send wlrctl
# Should print a path for each
```

X0NE checks each binary at action time (not at startup). If a binary is missing, the action returns:
```json
{"success":false,"error":{"kind":"MissingDependency","message":"Required binary 'grim' not found."}}
```

---

## Enabling desktop in config

Desktop actions are **disabled by default**. To enable:

```toml
# ~/.config/x0ne/x0ne-runtime.toml

[desktop]
enabled = true

# Minimum ms between input actions (anti-runaway)
input_rate_limit_ms = 100

# Hard cap on input actions per minute
max_input_actions_per_minute = 120

# Require Confirm before taking screenshots
screenshot_confirm = false

# Where screenshots are saved
screenshot_dir = "~/.x0ne/screenshots"

# Optional: confine gestures, screenshots, window reads and agent commands to these apps (see below)
# allowed_apps = ["firefox", "alacritty", "code"]
```

Also enable the individual actions in `[actions]`:

```toml
[actions]
# Desktop — enable only what you need
Screenshot          = true
GetWindowTree       = true
GetFocusedWindow    = true
FocusWindow         = true
MoveMouse           = false  # enable with care
Click               = false  # enable with care
DoubleClick         = false  # enable with care
Scroll              = false
TypeText            = false  # enable with care
PressKey            = false
PressKeyCombination = false
SendNotification    = true
GetClipboard        = true
SetClipboard        = false
```

---

## Enable from orchestrator

When starting the orchestrator, the system prompt includes desktop actions only if `X0NE_DESKTOP=1`:

```bash
X0NE_DESKTOP=1 x0ne-orchestrator
```

This prevents the LLM from attempting desktop actions in headless environments.

---

## How the vision loop works

The vision loop lets the LLM see the screen and act on what it sees:

```
1. LLM requests Screenshot
   → {"type":"Screenshot"}

2. x0ne-runtime runs: grim /tmp/x0ne/screenshot_20260407_142300.png
   → Returns path + dimensions

3. Orchestrator detects screenshot result
   → Reads PNG from disk
   → Encodes to base64
   → Injects into next LLM message as image content part

4. Multimodal LLM (Claude/Llama with vision) sees the screenshot
   → Describes what it sees
   → Can issue Click/TypeText/etc based on visual content

5. Loop continues
```

### Requirements for vision

- The LLM must support multimodal input (Claude 3+, Llava, etc.)
- The screenshot directory must be readable by the orchestrator
- The orchestrator must be configured with `X0NE_DESKTOP=1`

### Vision with Claude

Claude Sonnet and Opus natively support image input. The orchestrator automatically encodes the screenshot and adds it to the messages array as a `image_url` or `image` content block depending on the API version.

### Vision with Ollama

Requires a vision-capable model (e.g., `llava`, `bakllava`, `llama3.2-vision`). The orchestrator uses the same multipart message format for OpenAI-compatible APIs.

---

## Rate limiting

All mouse and keyboard actions share a rate limiter — a single `Mutex<Instant>` tracking the last input event.

**How it works:**
```
action requested
    │
    ├── elapsed < input_rate_limit_ms?
    │       └── Yes → Deny (rate limit exceeded)
    │
    └── No → execute → update timestamp
```

**Why it exists:** Without rate limiting, a buggy LLM response could generate hundreds of click/type actions per second, effectively bricking the UI. At 100ms per action (the default), the maximum speed is 10 actions/second — fast enough for automation, observable by a human.

**Override:** Lower the limit carefully. 50ms is reasonable; below 20ms can cause dropped events on some systems.

```toml
[desktop]
input_rate_limit_ms = 100         # per-action minimum
max_input_actions_per_minute = 120  # secondary cap
```

---

## Allowed apps whitelist

By default, desktop gestures reach any window. To confine them:

```toml
[desktop]
allowed_apps = ["firefox", "Alacritty", "code", "kitty"]
```

An entry is compared, case-insensitively and whole, with the window's `app_id` (Wayland) or X11 class, as `GetWindowTree` reports them. The title is never used: any program sets its own. A window with no identity is refused.

When `allowed_apps` is non-empty, the runtime reads the window tree before each gesture and refuses (`PolicyDenied`) unless:

| Gesture | Condition |
|---|---|
| `Click`, `DoubleClick`, `MoveMouse` | Coordinates are given, and every visible window under the point is listed. Checked before and after the pointer warp, and between the two clicks of a `DoubleClick`. Without coordinates the pointer position is unknown: refused. |
| `Scroll` | The focused window is listed; the pointer is moved to its centre, which is re-checked before each wheel step. |
| `TypeText` | The focused window is listed. Text goes out in 32-character chunks, the focus re-read before each; typing stops if it moved. |
| `PressKey`, `PressKeyCombination` | The focused window is listed, and no key is a compositor key (`super`, `meta`, `hyper`, `logo`, `win`, `Super_*`, `Meta_*`, `Hyper_*`, `XF86*`). |
| `FocusWindow` | The match is resolved in the tree to a listed window, focused by its id, and the focus is confirmed afterwards. |
| `SetClipboard` | The focused window is listed. |
| `GetClipboard` | Always refused: the clipboard holds what any app copied last. |

After `TypeText`, `PressKey`, `PressKeyCombination` and `FocusWindow`, the focus is read again: if it left the checked window during the gesture, the action fails and says so, since part of the input may have reached another app.

Reads:

| Action | Under the list |
|---|---|
| `Screenshot` | The tree is read before and after the capture. The returned image is black except the listed views that are visible and identical (id, `app_id`, geometry) in both reads; every unlisted visible view of either read is then blacked out, so a floating window over a listed one is masked. Bars, wallpaper, notifications and anything else outside the tree stay black. Refused, with no file left behind, when the tree is unreadable or gives no geometry for the area (`-o` output, `-g` region, or the whole layout). The unmasked capture is a hidden file in `screenshot_dir`, deleted before the action returns. |
| `GetWindowTree`, `GetFocusedWindow` | A window outside the list keeps its id, `app_id` and geometry (needed to understand a refusal); its title is blanked. |

Agent commands. With the list set, `RunCommand`, `Listen`, `SafeTest` and `ArtifactServe` run only under bubblewrap: under `--isolation none` or `--isolation container` they are refused (`missing_dependency`, message naming `allowed_apps`). Under bubblewrap the child does not see the session: `/run/user/<uid>` (Wayland, sway and i3 IPC, D-Bus, at-spi), the paths named by `SWAYSOCK`, `I3SOCK`, `WAYLAND_DISPLAY`, `XAUTHORITY` and `YDOTOOL_SOCKET` in the runtime's environment, and `/tmp/.X11-unix` and `/tmp/.ydotool_socket` are hidden; `/dev` is bwrap's minimal one, without `/dev/uinput` or `/dev/input`; abstract sockets (X11) are out of reach through the network namespace or, with the network shared, Landlock scoping. The barrier is what the child sees, not a list of tool names, so it holds for `ydotool`, `dotool`, `wtype`, `wlrctl`, `swaymsg`, `i3-msg`, `xdotool`, `xte`, `wl-copy`/`wl-paste`, `xclip`, `grim`, `gdbus` and any interpreter or renamed copy of them. Refused as well under the list: a session socket inside a write root (a file action could rename it out of the hidden path), and a shared network on a kernel whose Landlock is older than ABI 6. Attacks in `x0ne-runtime/tests/desktop_out_of_reach.rs`.

Limits. Sway has no "send this event to window N": between the last check and the event there is a window of a few milliseconds that the checks narrow but do not close. `app_id` is chosen by the client, so a local program can claim a listed one. Keyboard focus held by a layer-shell surface (launcher, lock screen) is not in the tree, and a layer-shell overlay or a popup drawn over a listed window is captured with it. Compositor bindings on `alt` or `ctrl` are not known to the runtime. A session socket the runtime's environment does not name, outside `/run/user/<uid>` and `/tmp`, is not hidden. `SendNotification` is not confined.

---

## Privacy considerations

**Screenshots contain your screen.** If you are using a cloud LLM (Claude, GPT-4 Vision):
- Every `Screenshot` action sends your screen contents to the LLM provider
- This includes any sensitive information visible on screen (passwords, private messages, code)
- Screenshots are also saved to `~/.x0ne/screenshots/` — `MoveToTrash` them when done

**Mitigation options:**
- Use `screenshot_confirm = true` to require explicit confirmation before each screenshot
- Use a local multimodal model (Ollama + LLaVA) so images stay on-device
- Require `FocusWindow` first, then screenshot only the focused window using `--output` flag
- Review `~/.x0ne/screenshots/` periodically and clean up

**Example: screenshot with confirm**
```toml
[desktop]
screenshot_confirm = true
```

The orchestrator will issue a `Confirm` action before `Screenshot`, giving the user a chance to close sensitive windows first.

---

## Example session

```
User: take a screenshot and tell me what's on screen

[ACTION] Screenshot
[RESULT] ✓ /home/user/.x0ne/screenshots/20260407_143200.png (2560x1440, 882 KB)

[Vision] <screenshot injected into LLM context>

I can see your terminal with the x0ne-orchestrator running, and Firefox open with a GitHub
tab showing the X0NE repository. The terminal shows "64 passing" from the last test run.

User: click the star button on GitHub

[ACTION] GetFocusedWindow
[RESULT] ✓ Firefox — x0ne/x0ne — GitHub

[ACTION] Screenshot
[RESULT] ✓ screenshot captured

[Reasoning] I can see the star button in the top-right area near the repo name.
Based on the screenshot dimensions and typical GitHub layout, the star button
should be around x=1180, y=200.

[ACTION] Click {"x": 1180, "y": 200, "button": "left"}
[RESULT] ✓ clicked

[ACTION] Screenshot
[RESULT] ✓ screenshot captured
The star is now yellow — repository starred successfully.
```
