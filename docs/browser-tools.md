# Browser-tools: X0NE's safe browser primitive for frontend dev agents

## Motivation

When Claude Code (or any agent built on X0NE) is helping you build a web
frontend, it needs to *see* the result and *drive* the UI. Until v1.7, the
options were:

1. **Anthropic's computer-use** — drives the host's actual desktop. Powerful
   but risky: the agent could click anywhere, including your bank tab.
2. **`x0ne-vm`** — drives a full QEMU/KVM virtual machine. Safe (kernel
   isolation, snapshot rewind), but heavy: 256 MB+ per agent body, 30s boot.
3. **No browser primitive** — the agent debugs by reading source and guessing.

`x0ne-browser` is the missing primitive: a dedicated Chromium controlled
purely via CDP, isolated from the user's real browser, with a defense-in-
depth security model designed for one job: **let an agent test the frontend
you're developing**.

## Architecture

```
Claude Code workspace          x0ne-browser-mcp              Chromium subprocess
─────────────────────          ────────────────              ───────────────────
  agent: navigate ──►          allowlist ──►                 (chrome --user-data-dir=
                               rate-limit                     ~/.x0ne/browser-profiles/
                               audit log                      <uuid>/  --no-first-run)
                               CDP client ──►                ◄── DevTools Protocol
  agent: screenshot ◄──        base64 png response
```

- Single tab per session: every target Chromium opens beside it
  (`window.open`, a `target=_blank` link) is closed as it appears.
- Every request the browser makes passes the allowlist (CDP `Fetch` on the
  browser session), not only the address bar.
- Lazy start: Chromium spawned on first call that needs it.
- Headed by default (you see what the agent sees). Headless via
  `X0NE_BROWSER_HEADLESS=1` or `mode = "headless"`.
- Profile wiped on `browser_close()` unless `keep_profile = true`.

## Default allowlist

```
http://127.0.0.1:*       http://localhost:*       http://[::1]:*
https://localhost:*      about:*                  (`data:*` is opt-in)
```

Override in `~/.x0ne/browser-tools.toml` (see `browser-tools.toml.example`). A
file that does not parse stops `x0ne-browser-mcp`: the defaults have no
allowlist, and a typo in yours must not lift it.

## Tool → use case map

| Tool | Typical use |
|---|---|
| `browser_navigate("http://localhost:3000")` | Open the dev server |
| `browser_screenshot({full_page: true})` | "What does the page look like right now?" |
| `browser_click("text:Sign in")` | Click the visible "Sign in" link |
| `browser_type(".email", "agent@x.co")` | Fill a form field |
| `browser_press_key("Enter")` | Submit, navigate, dismiss |
| `browser_wait_for(".loaded", {timeout_ms: 5000})` | Block until the app finished a route change |
| `browser_read_text(".toast")` | Read the result of an action |
| `browser_read_dom(".chat-history")` | Get the structure of a complex component for analysis |
| `browser_get_url()` | "Did the navigation actually land on /success?" |
| `browser_back()` / `browser_forward()` / `browser_reload()` | History ops |
| `browser_close()` | End of test — kill chromium, wipe profile |
| `browser_status()` | Diagnostics: rate limit left, allowlist, profile path |

## Typical session

```javascript
// (pseudo-code — the agent uses MCP tools/call under the hood)
browser_navigate("http://localhost:3000/admin")
browser_screenshot({ full_page: true })                      // initial state
browser_click("text:Operator")                                // open the chat
browser_type(".x0w-input", "/test")
browser_press_key("Enter")
browser_wait_for(".x0w-msg-assistant", { timeout_ms: 5000 })
browser_screenshot({ target: ".x0w-box" })                    // result of the /test
const reply = browser_read_text(".x0w-msg-assistant:last-child")
// agent decides whether the test passed
browser_close()
```

## Security: the seven gates

1. **Allowlist** — every request the browser makes passes it: the navigation,
   each image, frame, script, `fetch`, beacon. A request outside fails in
   the browser (`BlockedByClient`) and is journaled as
   `browser_request_blocked`; a `navigate` outside is refused with
   `domain_not_allowed` and the error returns the current allowlist so the
   agent can suggest an edit; every `click`, `type` re-checks the current
   URL. Every target opened beside the page (`window.open`, `target=_blank`)
   is closed as it appears and journaled as `browser_popup_closed`.
2. **Isolated profile** — `~/.x0ne/browser-profiles/<uuid>/`, never your real
   Chrome profile. Cookies/passwords/session storage live and die with the
   session.
3. **Audit log** — `~/.x0ne/logs/browser/<date>.jsonl`, one JSON line per
   call, written 0600 in a 0700 directory. `args` go through a regex-based
   redactor that masks fields whose key matches
   `password|token|secret|api_key|auth|credential|bearer` (case insensitive),
   and the `text` of `browser_type`: whatever was typed into the page.
4. **Rate limit** — token bucket, default 60/min, refills at capacity/60
   tokens per second. Exceeding → `rate_limited` with `retry_after_ms`.
5. **Abort switch** — `~/.x0ne/browser-tools.kill` (file). When it exists,
   every call returns `killed_by_operator`. Delete the file to resume. This
   is your "emergency stop" — kill the agent's browser without killing X0NE.
6. **Size caps** — screenshot ≤ 5 MB, text payload ≤ 256 KB, DOM depth
   ≤ 8 by default. Configurable via TOML.
7. **No `eval_js`, no upload** — categorical refusal in v1. Both are
   bypass vectors for everything above.

## Error codes (stable contract)

The agent receives a `code` field on failure that's stable across versions:

| code | when |
|---|---|
| `domain_not_allowed` | navigate/interact refused by allowlist |
| `selector_not_found` | CSS/XPath/text matched nothing |
| `timeout` | wait_for / navigation exceeded the budget |
| `navigation_failed` | DNS / TLS / 5xx |
| `killed_by_operator` | abort switch active |
| `rate_limited` | per-minute budget consumed |
| `needs_confirmation` | cross-domain submit, retry with `force_cross_domain: true` |
| `not_connected` | Chromium not started or crashed |
| `invalid_selector` | syntactically bad selector |
| `not_implemented` | v2 feature requested |
| `internal` | anything else (with message) |

## Limitations (known + intentional)

- **Single tab**: simpler model. `browser_list_tabs` returns `not_implemented` in v1.
- **No JS eval**: hard-coded refusal. Use `browser_read_dom` + your own analysis.
- **No file upload**: phase 2.
- **No cookies/storage inspection**: phase 2 via `browser_dump_storage`.
- **No multi-session within one MCP server process**: each `x0ne-browser-mcp`
  invocation owns one session. Spawn N of them if you need N parallel browsers.
- **Selectors**: CSS works in v1. `xpath:` and `text:` prefixes are parsed
  but the chromiumoxide adapter only supports CSS — they return
  `not_implemented` in v1 (the parsing itself is unit-tested for v2 readiness).

## Operations: how to monitor / debug

- **What just happened?**  `tail -f ~/.x0ne/logs/browser/$(date +%F).jsonl | jq`
- **Pause the agent**:  `touch ~/.x0ne/browser-tools.kill`
- **Resume**:  `rm ~/.x0ne/browser-tools.kill`
- **Reset rate limit**:  restart the MCP server — bucket is in-memory.
- **Inspect the agent's view**:  it's a real Chromium window — bring it to
  front, attach DevTools yourself, etc.

## Where this lives in the X0NE architecture

```
                           x0ne-orchestrator --mcp
                                    │
                       ChildMcpClient::spawn N children
            ┌─────────────────┬─────┴─────┬─────────────────┐
            ▼                 ▼           ▼                 ▼
       x0ne-runtime      x0ne-vm-mcp   x0ne-browser-mcp   (future)
       (host actions)    (VM bodies)   (web testing)
            │                 │           │
           ─┴─                ─┴─        ─┴─
       133 actions       29 vm_* tools  14 browser_* tools
```

Each downstream is independently activatable. `x0ne-browser` doesn't
require `x0ne-vm` or vice versa.

## Roadmap

- **v1** (this PR) — navigate, screenshot, click, type, key, wait_for,
  read_text/dom, back/forward/reload, close, status. Allowlist + audit +
  rate limit + abort switch + isolated profile.
- **v2** — multi-tab, hover, scroll, network_log, console_logs, file
  upload (gated), `xpath:` and `text:` selector backends, dump_storage,
  `force_cross_domain` to convert `needs_confirmation` to an executed call.
- **v3** — video recording of the session, accessibility-tree snapshot
  + diff for regression detection, headless-with-virtual-display option.
