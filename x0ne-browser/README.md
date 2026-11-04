# x0ne-browser

> CDP-driven Chromium tools for AI agents that develop and test web frontends.
> The agent gets its own isolated browser — never touches yours. 14 MCP tools.

```
   Claude Code / x0ne-code               x0ne-browser-mcp           Chromium
   ─────────────────────                 ─────────────────          ────────
     "navigate to localhost:3000"  ─►   allowlist check  ─►
                                        rate limit                  open page
                                        audit log                ◄  page ready
     "screenshot"                  ─►                          ─►  capture
                                                              ◄    PNG bytes
     "click on .login-button"      ─►   wait_for selector   ─►  click
     "read_text .toast"            ─►                          ─►  innerText
                                                              ◄    string
```

## Why

You're developing a frontend. You want Claude to:
- Open it (`http://localhost:3000`)
- See it (`screenshot`)
- Drive it (`click`, `type`)
- Read it (`read_text`, `read_dom`)

Without:
- Touching your user's actual browser profile (cookies, sessions, saved passwords)
- Letting an agent navigate to phishing sites by accident
- Spamming your local server with infinite-loop click attempts
- Leaving no trace of what it actually did

`x0ne-browser` ticks all four.

## Compared to x0ne-vm

|  | x0ne-browser | x0ne-vm |
|---|---|---|
| Use case | Test localhost frontend | Generic computer-use, untrusted code |
| Per-call latency | **5-20 ms** (CDP) | 100ms (VNC) |
| Memory | ~250 MB (Chromium) | 256 MB+ per VM |
| Kernel isolation | ❌ shared with host | ✅ KVM |
| Snapshot/restore | ❌ | ✅ qcow2 |
| Visual cursor in screenshot | ✅ native CDP | synthetic overlay |
| Best for | iterating a webapp | generic agent body |

The two are complementary. You'll usually want both in your `.mcp.json`.

## Security (the non-negotiables)

1. **Allowlist** — every request the browser makes passes it, not only the address bar: the navigation, each image, frame, script, `fetch`, beacon. A request outside fails in the browser (`BlockedByClient`) and is journaled; a `navigate` outside is refused with `domain_not_allowed`. Default: `localhost`, `127.0.0.1`, `[::1]`, plus `about:`; `data:` is opt-in.
2. **Isolated profile** — each session gets `~/.x0ne/browser-profiles/<uuid>/` and is wiped at close. Your real Chrome / Firefox cookies are NEVER visible to the agent.
3. **Audit log** — every tool call is appended to `~/.x0ne/logs/browser/<date>.jsonl`, written 0600 in a 0700 directory, with args redacted: passwords/tokens replaced with `<redacted>`, and the text `browser_type` types into the page, whatever the field is called.
4. **Rate limit** — default 60 calls/min (configurable). Exceeding → `rate_limited` error with `retry_after_ms`.
5. **Abort switch** — `touch ~/.x0ne/browser-tools.kill` makes every subsequent call return `killed_by_operator`. Untouching the file resumes.
6. **No `eval_js` in v1** — categorical refusal; and the page's own scripts reach nothing outside the allowlist (rule 1), so a page the agent wrote cannot carry what it reads elsewhere.
7. **No file uploads in v1** — phase 2.
8. **No multi-tab** in v1 — one page per session; every target Chromium opens beside it (`window.open`, a `target=_blank` link) is closed as it appears and journaled, its requests pass the allowlist like the page's, and the popup blocker stays on.

## Tools (14 in v1)

| Tool | Purpose |
|---|---|
| `browser_navigate(url)` | Open a URL (gated by allowlist) |
| `browser_get_url()` | Current page URL |
| `browser_screenshot(full_page?, format?)` | PNG/JPEG base64 |
| `browser_click(selector)` | Click an element (CSS selector) |
| `browser_type(selector, text, clear?)` | Type into an input |
| `browser_press_key(key)` | One keystroke (Enter, Tab, …) |
| `browser_read_text(selector)` | innerText of element |
| `browser_read_dom(selector?)` | outerHTML of element (or html) |
| `browser_wait_for(selector, timeout_ms?)` | Poll until element appears |
| `browser_back()` / `browser_forward()` / `browser_reload()` | History/reload |
| `browser_close()` | Kill Chromium + wipe profile |
| `browser_status()` | Diagnostics |

Phase 2 stubs (return `not_implemented`): `browser_hover`, `browser_scroll`, `browser_list_tabs`, `browser_console_logs`, `browser_network_log`.

## Error codes (stable contract)

| code | meaning |
|---|---|
| `domain_not_allowed` | URL didn't match the allowlist |
| `selector_not_found` | CSS/XPath/text returned nothing |
| `timeout` | wait_for / navigation exceeded budget |
| `navigation_failed` | DNS / TLS / 5xx |
| `killed_by_operator` | abort switch file exists |
| `rate_limited` | per-minute budget consumed |
| `needs_confirmation` | cross-domain submit (retry with `force_*` flag) |
| `not_connected` | Chromium subprocess not running |
| `invalid_selector` | malformed selector |
| `not_implemented` | v2 feature |
| `internal` | other unexpected failure |

## Quickstart

```bash
# 1. Build
cargo build -p x0ne-browser --release

# 2. Wire into Claude Code
# .mcp.json — append:
#   "x0ne-browser": { "command": "x0ne-browser-mcp" }

# 3. (optional) tune via ~/.x0ne/browser-tools.toml
# See browser-tools.toml.example

# 4. From your agent — typical flow:
#    browser_navigate("http://localhost:3000")
#    browser_screenshot({ full_page: true })
#    browser_click("text:Sign in")
#    browser_type(".email", "agent@x.co")
#    browser_press_key("Enter")
#    browser_wait_for(".welcome", { timeout_ms: 5000 })
#    browser_read_text(".welcome h1")
```

## Configuration

`~/.x0ne/browser-tools.toml` (optional — defaults work fine). A file that does not parse stops the server rather than serving on the defaults: the defaults have no allowlist, and a typo must not lift yours.

```toml
mode = "headed"                 # or "headless"
rate_limit_per_minute = 60
default_timeout_ms = 10000
max_screenshot_bytes = 5242880   # 5MB
max_text_bytes = 262144          # 256KB
default_dom_depth = 8
keep_profile = false             # wipe at close

allowlist = [
  "http://127.0.0.1:*",
  "http://localhost:*",
  "https://*.example.org",
  "https://staging.myproduct.com",   # add your own
  "data:*",
  "about:*",
]
```

Env overrides:
- `X0NE_BROWSER_HEADLESS=1` → headless mode
- `X0NE_BROWSER_RATE_LIMIT=120` → cap per minute

## Tests

```bash
cargo test -p x0ne-browser
# 22 unit tests across allowlist / audit redaction / rate limit /
# selector parsing / config defaults — no Chromium needed.
CHROME=/usr/lib/chromium/chromium X0NE_REQUIRE_CHROMIUM=1 \
  cargo test -p x0ne-browser --test every_request_passes_the_allowlist
# end to end: an allowed page reaches nothing outside the allowlist. Needs a
# Chromium; without one the test prints SKIP and passes, unless
# X0NE_REQUIRE_CHROMIUM=1.
```

## Roadmap

- **v1** (this) — read + navigate + click + type + key + wait + close, all secured
- **v2** — multi-tab, hover, scroll, network log, console log, file upload (with extra gates)
- **v3** — accessibility tree, DOM snapshot diffing, video recording

## License

Apache-2.0
