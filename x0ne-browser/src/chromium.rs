//! ChromiumDriver — the actual chromiumoxide-backed implementation.
//!
//! Single-tab, single-session for v1. Lazy start: the browser is spawned on
//! the first call that needs it. Profile is `~/.x0ne/browser-profiles/<uuid>/`
//! and is wiped on Drop unless `keep_profile = true`.
//!
//! Every request the browser makes passes the allowlist, not only the
//! tab's URL: `Fetch` on the browser session pauses each one, the allowed
//! continue, the others fail with `BlockedByClient` and are journaled.
//! Every target Chromium opens beside the page (a popup, a `target=_blank`
//! link) is closed as it appears, and the popup blocker stays on (audit of
//! 2026-10-30, H4-1).

use crate::allowlist::Allowlist;
use crate::audit::{append as audit_append, AuditEntry};
use crate::error::{BrowserError, BrowserResult, ErrorCode};
use crate::rate_limit::RateLimiter;
use crate::selector::Selector;
use crate::session::BrowserConfig;
use chromiumoxide::cdp::browser_protocol::fetch::{
    self, ContinueRequestParams, EventRequestPaused, FailRequestParams, RequestPattern,
};
use chromiumoxide::cdp::browser_protocol::network::ErrorReason;
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::cdp::browser_protocol::target::{CloseTargetParams, EventTargetCreated};
use chromiumoxide::listeners::EventStream;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::{Browser, BrowserConfig as CdpConfig, Page};
use futures::StreamExt;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use uuid::{NoContext, Timestamp, Uuid};

fn new_session_id() -> String {
    // Workspace uuid only enables v7 — use it (timestamp-ordered, just as unique).
    let ts = Timestamp::now(NoContext);
    Uuid::new_v7(ts).to_string()
}

pub struct ChromiumDriver {
    pub session_id: String,
    pub config: BrowserConfig,
    pub allowlist: Allowlist,
    pub rate_limit: RateLimiter,
    profile_dir: PathBuf,
    state: Mutex<Option<DriverState>>,
}

struct DriverState {
    /// Shared with `popup_closer`, which closes targets at the browser
    /// level; `close` takes it back once the tasks are gone.
    browser: Arc<Browser>,
    page: Page,
    /// Spawned event loop. Dropped → handler ends → browser dies.
    _handler: JoinHandle<()>,
    /// Answers every paused request of the page (`spawn_interceptor`).
    interceptor: JoinHandle<()>,
    /// Closes every target opened beside the page (`spawn_popup_closer`).
    popup_closer: JoinHandle<()>,
}

/// chromiumoxide's launch flags (puppeteer's) without
/// `disable-popup-blocking`: a popup is a second tab the allowlist did not
/// see. The flags Chromium takes as `--key`.
const LAUNCH_FLAGS: &[&str] = &[
    "disable-background-networking",
    "disable-background-timer-throttling",
    "disable-backgrounding-occluded-windows",
    "disable-breakpad",
    "disable-client-side-phishing-detection",
    "disable-component-extensions-with-background-pages",
    "disable-default-apps",
    "disable-dev-shm-usage",
    "disable-hang-monitor",
    "disable-ipc-flooding-protection",
    "disable-prompt-on-repost",
    "disable-renderer-backgrounding",
    "disable-sync",
    "metrics-recording-only",
    "no-first-run",
    "no-default-browser-check",
    "enable-automation",
    "use-mock-keychain",
];

/// The flags Chromium takes as `--key=value`.
const LAUNCH_VALUES: &[(&str, &str)] = &[
    ("enable-features", "NetworkService,NetworkServiceInProcess"),
    ("disable-features", "TranslateUI,Translate"),
    ("force-color-profile", "srgb"),
    ("password-store", "basic"),
    ("enable-blink-features", "IdleDetection"),
    ("lang", "en_US"),
];

impl ChromiumDriver {
    pub fn new(mut config: BrowserConfig) -> Self {
        config.apply_env();
        let session_id = new_session_id();
        let profile_dir = config.profile_root.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".x0ne")
                .join("browser-profiles")
                .join(&session_id)
        });
        let allowlist = if config.allowlist.is_empty() {
            Allowlist::default()
        } else {
            Allowlist::new(config.allowlist.clone())
        };
        let rate_limit = RateLimiter::new(config.rate_limit_per_minute);
        Self {
            session_id,
            config,
            allowlist,
            rate_limit,
            profile_dir,
            state: Mutex::new(None),
        }
    }

    /// Lazy-start the browser if not already running.
    async fn ensure_started(&self) -> BrowserResult<()> {
        let mut guard = self.state.lock().await;
        if guard.is_some() {
            return Ok(());
        }
        tokio::fs::create_dir_all(&self.profile_dir).await.ok();

        let mut cfg_b = CdpConfig::builder()
            .user_data_dir(&self.profile_dir)
            .disable_default_args()
            .args(LAUNCH_FLAGS.iter().copied())
            .args(LAUNCH_VALUES.iter().copied());
        if !self.config.is_headless() {
            cfg_b = cfg_b.with_head();
        }
        let cfg = cfg_b
            .build()
            .map_err(|e| BrowserError::new(ErrorCode::Internal, format!("chrome config: {e}")))?;

        let (browser, mut handler) = Browser::launch(cfg)
            .await
            .map_err(|e| BrowserError::new(ErrorCode::Internal, format!("chrome launch: {e}")))?;
        let browser = Arc::new(browser);

        let handler_task = tokio::spawn(async move {
            while let Some(_evt) = handler.next().await {
                // Drain: the listeners below get their copies.
            }
        });

        // `Fetch` on the browser session pauses every request of every
        // target, a popup's first navigation included, before any page
        // session exists for it: a `target=_blank` link opened by a click
        // sent its navigation before a per-page `Fetch.enable` reached the
        // new target (measured 2026-10-31).
        browser
            .execute(
                fetch::EnableParams::builder()
                    .pattern(RequestPattern::builder().url_pattern("*").build())
                    .build(),
            )
            .await
            .map_err(|e| BrowserError::new(ErrorCode::Internal, format!("fetch enable: {e}")))?;
        let paused = browser
            .event_listener::<EventRequestPaused>()
            .await
            .map_err(|e| BrowserError::new(ErrorCode::Internal, format!("fetch events: {e}")))?;
        let created = browser
            .event_listener::<EventTargetCreated>()
            .await
            .map_err(|e| BrowserError::new(ErrorCode::Internal, format!("target events: {e}")))?;
        let interceptor = self.spawn_interceptor(&browser, paused);

        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|e| BrowserError::new(ErrorCode::Internal, format!("new page: {e}")))?;

        let popup_closer = self.spawn_popup_closer(&browser, &page, created);

        *guard = Some(DriverState {
            browser,
            page,
            _handler: handler_task,
            interceptor,
            popup_closer,
        });
        Ok(())
    }

    /// Answers every `Fetch.requestPaused` of the browser: a URL on the
    /// allowlist continues, any other fails with `BlockedByClient` and is
    /// journaled. A request nobody answers never leaves.
    fn spawn_interceptor(
        &self,
        browser: &Arc<Browser>,
        mut paused: EventStream<EventRequestPaused>,
    ) -> JoinHandle<()> {
        let browser = Arc::clone(browser);
        let allowlist = self.allowlist.clone();
        let session = self.session_id.clone();
        tokio::spawn(async move {
            while let Some(ev) = paused.next().await {
                let url = ev.request.url.as_str();
                if allowlist.allows(url) {
                    let _ = browser
                        .execute(ContinueRequestParams::new(ev.request_id.clone()))
                        .await;
                    continue;
                }
                let _ = browser
                    .execute(FailRequestParams::new(
                        ev.request_id.clone(),
                        ErrorReason::BlockedByClient,
                    ))
                    .await;
                let entry = AuditEntry::new(
                    &session,
                    "browser_request_blocked",
                    json!({
                        "url": url,
                        "resource_type": serde_json::to_value(&ev.resource_type)
                            .unwrap_or(Value::Null),
                    }),
                )
                .fail(
                    ErrorCode::DomainNotAllowed.as_str(),
                    "request to an origin outside the allowlist",
                );
                let _ = audit_append(&entry).await;
            }
        })
    }

    /// Closes every page target Chromium opens beside `page`: `window.open`,
    /// a `target=_blank` link, whatever the popup blocker lets through on a
    /// click. Its requests already pass the browser-level `Fetch`;
    /// `Target.targetCreated` names it, `Target.closeTarget` ends it, the
    /// journal keeps its URL.
    fn spawn_popup_closer(
        &self,
        browser: &Arc<Browser>,
        page: &Page,
        mut created: EventStream<EventTargetCreated>,
    ) -> JoinHandle<()> {
        let browser = Arc::clone(browser);
        let main_target = page.target_id().clone();
        let session = self.session_id.clone();
        tokio::spawn(async move {
            while let Some(ev) = created.next().await {
                let info = &ev.target_info;
                if info.r#type != "page" || info.target_id == main_target {
                    continue;
                }
                let closed = browser
                    .execute(CloseTargetParams::new(info.target_id.clone()))
                    .await;
                let entry = AuditEntry::new(
                    &session,
                    "browser_popup_closed",
                    json!({
                        "url": info.url,
                        "opener": serde_json::to_value(&info.opener_id).unwrap_or(Value::Null),
                    }),
                );
                let entry = match closed {
                    Ok(_) => entry,
                    Err(e) => entry.fail(ErrorCode::Internal.as_str(), &e.to_string()),
                };
                let _ = audit_append(&entry).await;
            }
        })
    }

    fn check_kill_switch(&self) -> BrowserResult<()> {
        if self.config.effective_kill_switch().exists() {
            return Err(BrowserError::new(
                ErrorCode::KilledByOperator,
                "kill switch file exists",
            ));
        }
        Ok(())
    }

    fn check_rate(&self) -> BrowserResult<()> {
        match self.rate_limit.try_consume() {
            Ok(()) => Ok(()),
            Err(retry_after) => Err(BrowserError::new(
                ErrorCode::RateLimited,
                format!(
                    "rate limit ({}/min) exceeded — retry in {}ms",
                    self.config.rate_limit_per_minute,
                    retry_after.as_millis()
                ),
            )
            .with_detail(json!({
                "limit_per_minute": self.config.rate_limit_per_minute,
                "retry_after_ms": retry_after.as_millis() as u64,
            }))),
        }
    }

    /// Wrap any call: check kill switch + rate-limit + log on entry, log result.
    /// All refusals (kill switch, rate limit, business error) are audited.
    async fn guarded<F, T, Fut>(&self, tool: &str, args: Value, f: F) -> BrowserResult<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = BrowserResult<T>>,
    {
        let entry = AuditEntry::new(&self.session_id, tool, args.clone());
        if let Err(e) = self.check_kill_switch() {
            let _ = audit_append(&entry.clone().fail(e.code.as_str(), &e.message)).await;
            return Err(e);
        }
        if let Err(e) = self.check_rate() {
            let _ = audit_append(&entry.clone().fail(e.code.as_str(), &e.message)).await;
            return Err(e);
        }
        // SECURITY: verify the page's CURRENT origin BEFORE running any tool.
        //
        // The post-navigation recheck only fires on navigate/back/forward/reload,
        // so an origin pivot by any other means — a click on a link, a JS
        // `location=` from a timer, an iframe-driven top-level navigation —
        // left the page sitting on a disallowed origin, and the next
        // read_dom/read_text/screenshot happily read it. Checking here closes
        // that: every guarded tool re-validates first, so content is rejected
        // before it can be read rather than after.
        if let Err(e) = self.recheck_origin_if_started().await {
            Self::audit_or_fail(entry.clone().fail(e.code.as_str(), &e.message)).await?;
            return Err(e);
        }

        let res = f().await;
        let final_entry = match &res {
            Ok(_) => entry,
            Err(e) => entry.fail(e.code.as_str(), &e.message),
        };
        // SECURITY: the audit trail is one of this crate's three advertised
        // guarantees. Previously the write result was discarded, so a full disk
        // or a bad permission silently produced un-audited browser actions.
        // Refuse the action rather than perform it unrecorded.
        Self::audit_or_fail(final_entry).await?;
        res
    }

    /// Write an audit entry, turning a write failure into a hard error.
    async fn audit_or_fail(entry: AuditEntry) -> BrowserResult<()> {
        audit_append(&entry).await.map_err(|e| {
            BrowserError::new(
                ErrorCode::Internal,
                format!("audit write failed, refusing to proceed unrecorded: {e}"),
            )
        })
    }

    /// Re-validate the live page's origin when a browser is running.
    ///
    /// No-op before the first navigation: there is no page, or it is
    /// about:blank, and nothing can have been read yet.
    async fn recheck_origin_if_started(&self) -> BrowserResult<()> {
        let guard = self.state.lock().await;
        let Some(state) = guard.as_ref() else {
            return Ok(());
        };
        self.recheck_current_origin(&state.page).await
    }

    // ─── tools ─────────────────────────────────────────────────────────────

    /// SECURITY: re-validate the page's CURRENT origin against the allowlist
    /// after any navigation. The pre-navigation check on `navigate()` only
    /// covers the requested URL — an HTTP redirect or in-page JS navigation can
    /// land on a different, disallowed origin. If that happens we blank the page
    /// (so subsequent screenshot/read_dom can't read or exfiltrate it) and
    /// surface an error. (A full fix would intercept every navigation at the CDP
    /// layer; this closes the redirect/JS-pivot reads.)
    async fn recheck_current_origin(&self, page: &Page) -> BrowserResult<()> {
        let current = page.url().await.ok().flatten().unwrap_or_default();
        if current.is_empty() || current == "about:blank" {
            return Ok(());
        }
        if !self.allowlist.allows(&current) {
            let _ = page.goto("about:blank").await;
            return Err(BrowserError::new(
                ErrorCode::DomainNotAllowed,
                format!(
                    "navigation landed on disallowed origin {current} (redirect/JS) — page blanked"
                ),
            )
            .with_detail(json!({ "allowlist": self.allowlist.rules() })));
        }
        Ok(())
    }

    pub async fn navigate(&self, url: &str) -> BrowserResult<Value> {
        self.guarded("browser_navigate", json!({"url": url}), || async {
            if !self.allowlist.allows(url) {
                return Err(BrowserError::new(
                    ErrorCode::DomainNotAllowed,
                    format!("navigation to {url} refused"),
                )
                .with_detail(json!({
                    "allowlist": self.allowlist.rules(),
                    "hint": "edit ~/.x0ne/browser-tools.toml to add this origin",
                })));
            }
            self.ensure_started().await?;
            let guard = self.state.lock().await;
            let state = guard.as_ref().ok_or_else(|| {
                BrowserError::new(ErrorCode::NotConnected, "chromium not running")
            })?;
            timeout(
                Duration::from_millis(self.config.default_timeout_ms),
                state.page.goto(url),
            )
            .await
            .map_err(|_| BrowserError::new(ErrorCode::Timeout, "navigation timed out"))?
            .map_err(|e| BrowserError::new(ErrorCode::NavigationFailed, e.to_string()))?;
            // Re-check after redirects settle — the goto target may have 30x'd
            // to a disallowed origin.
            self.recheck_current_origin(&state.page).await?;
            let landed = state
                .page
                .url()
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| url.to_string());
            Ok(json!({"ok": true, "url": landed}))
        })
        .await
    }

    pub async fn get_url(&self) -> BrowserResult<Value> {
        self.guarded("browser_get_url", json!({}), || async {
            self.ensure_started().await?;
            let guard = self.state.lock().await;
            let state = guard.as_ref().ok_or_else(|| {
                BrowserError::new(ErrorCode::NotConnected, "chromium not running")
            })?;
            let url = state
                .page
                .url()
                .await
                .map_err(|e| BrowserError::new(ErrorCode::Internal, e.to_string()))?
                .unwrap_or_default();
            Ok(json!({"ok": true, "url": url}))
        })
        .await
    }

    pub async fn screenshot(&self, full_page: bool, format: &str) -> BrowserResult<Value> {
        self.guarded(
            "browser_screenshot",
            json!({"full_page": full_page, "format": format}),
            || async {
                self.ensure_started().await?;
                let guard = self.state.lock().await;
                let state = guard.as_ref().ok_or_else(|| {
                    BrowserError::new(ErrorCode::NotConnected, "chromium not running")
                })?;
                let is_jpeg = format.eq_ignore_ascii_case("jpeg");
                let fmt = if is_jpeg {
                    CaptureScreenshotFormat::Jpeg
                } else {
                    CaptureScreenshotFormat::Png
                };
                let params = ScreenshotParams::builder()
                    .format(fmt)
                    .full_page(full_page)
                    .omit_background(false)
                    .build();
                let bytes = state
                    .page
                    .screenshot(params)
                    .await
                    .map_err(|e| BrowserError::new(ErrorCode::Internal, e.to_string()))?;
                if bytes.len() > self.config.max_screenshot_bytes {
                    return Err(BrowserError::new(
                        ErrorCode::Internal,
                        format!(
                            "screenshot too big: {} bytes > limit {}",
                            bytes.len(),
                            self.config.max_screenshot_bytes
                        ),
                    ));
                }
                use base64::Engine as _;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                Ok(json!({
                    "ok": true,
                    "format": if is_jpeg { "jpeg" } else { "png" },
                    "bytes": bytes.len(),
                    "data_base64": b64,
                }))
            },
        )
        .await
    }

    pub async fn click(&self, selector: &str) -> BrowserResult<Value> {
        self.guarded("browser_click", json!({"selector": selector}), || async {
            let sel = Selector::parse(selector)?;
            self.ensure_started().await?;
            let guard = self.state.lock().await;
            let state = guard.as_ref().ok_or_else(|| {
                BrowserError::new(ErrorCode::NotConnected, "chromium not running")
            })?;
            let css = sel.as_css().ok_or_else(|| {
                BrowserError::new(
                    ErrorCode::NotImplemented,
                    "xpath:/text: selectors are v2 — use a CSS selector for now",
                )
            })?;
            let el = state.page.find_element(css).await.map_err(|e| {
                BrowserError::new(ErrorCode::SelectorNotFound, format!("{css}: {e}"))
            })?;
            el.click()
                .await
                .map_err(|e| BrowserError::new(ErrorCode::Internal, e.to_string()))?;
            Ok(json!({"ok": true, "clicked": css}))
        })
        .await
    }

    pub async fn type_text(&self, selector: &str, text: &str, clear: bool) -> BrowserResult<Value> {
        self.guarded(
            "browser_type",
            json!({"selector": selector, "text": text, "clear": clear}),
            || async {
                let sel = Selector::parse(selector)?;
                self.ensure_started().await?;
                let guard = self.state.lock().await;
                let state = guard.as_ref().ok_or_else(|| {
                    BrowserError::new(ErrorCode::NotConnected, "chromium not running")
                })?;
                let css = sel.as_css().ok_or_else(|| {
                    BrowserError::new(ErrorCode::NotImplemented, "xpath:/text: selectors are v2")
                })?;
                let el = state.page.find_element(css).await.map_err(|e| {
                    BrowserError::new(ErrorCode::SelectorNotFound, format!("{css}: {e}"))
                })?;
                if clear {
                    el.click().await.ok();
                    state
                        .page
                        .evaluate(format!(r#"document.querySelector({css:?}).value = ""#))
                        .await
                        .ok();
                }
                el.type_str(text)
                    .await
                    .map_err(|e| BrowserError::new(ErrorCode::Internal, e.to_string()))?;
                Ok(json!({"ok": true, "typed_chars": text.chars().count()}))
            },
        )
        .await
    }

    pub async fn press_key(&self, key: &str) -> BrowserResult<Value> {
        self.guarded("browser_press_key", json!({"key": key}), || async {
            self.ensure_started().await?;
            let guard = self.state.lock().await;
            let state = guard.as_ref().ok_or_else(|| {
                BrowserError::new(ErrorCode::NotConnected, "chromium not running")
            })?;
            use chromiumoxide::cdp::browser_protocol::input::{
                DispatchKeyEventParams, DispatchKeyEventType,
            };
            let down = DispatchKeyEventParams::builder()
                .r#type(DispatchKeyEventType::KeyDown)
                .key(key.to_string())
                .build()
                .map_err(|e| {
                    BrowserError::new(ErrorCode::Internal, format!("key event build: {e}"))
                })?;
            let up = DispatchKeyEventParams::builder()
                .r#type(DispatchKeyEventType::KeyUp)
                .key(key.to_string())
                .build()
                .map_err(|e| {
                    BrowserError::new(ErrorCode::Internal, format!("key event build: {e}"))
                })?;
            state
                .page
                .execute(down)
                .await
                .map_err(|e| BrowserError::new(ErrorCode::Internal, e.to_string()))?;
            state
                .page
                .execute(up)
                .await
                .map_err(|e| BrowserError::new(ErrorCode::Internal, e.to_string()))?;
            Ok(json!({"ok": true, "key": key}))
        })
        .await
    }

    pub async fn read_text(&self, selector: &str) -> BrowserResult<Value> {
        self.guarded(
            "browser_read_text",
            json!({"selector": selector}),
            || async {
                let sel = Selector::parse(selector)?;
                self.ensure_started().await?;
                let guard = self.state.lock().await;
                let state = guard.as_ref().ok_or_else(|| {
                    BrowserError::new(ErrorCode::NotConnected, "chromium not running")
                })?;
                let css = sel.as_css().ok_or_else(|| {
                    BrowserError::new(ErrorCode::NotImplemented, "xpath:/text: selectors are v2")
                })?;
                let el = state.page.find_element(css).await.map_err(|e| {
                    BrowserError::new(ErrorCode::SelectorNotFound, format!("{css}: {e}"))
                })?;
                let text: String = el
                    .inner_text()
                    .await
                    .map_err(|e| BrowserError::new(ErrorCode::Internal, e.to_string()))?
                    .unwrap_or_default();
                let truncated = if text.len() > self.config.max_text_bytes {
                    let mut s = text.clone();
                    s.truncate(self.config.max_text_bytes);
                    s.push_str("…[truncated]");
                    s
                } else {
                    text
                };
                Ok(json!({"ok": true, "text": truncated}))
            },
        )
        .await
    }

    pub async fn read_dom(&self, selector: &str) -> BrowserResult<Value> {
        self.guarded(
            "browser_read_dom",
            json!({"selector": selector}),
            || async {
                let sel = Selector::parse(selector)?;
                self.ensure_started().await?;
                let guard = self.state.lock().await;
                let state = guard.as_ref().ok_or_else(|| {
                    BrowserError::new(ErrorCode::NotConnected, "chromium not running")
                })?;
                let css = sel.as_css().unwrap_or("html");
                let html: String = state.page
                    .evaluate(format!(r#"document.querySelector({css:?}) ? document.querySelector({css:?}).outerHTML : null"#))
                    .await
                    .map_err(|e| BrowserError::new(ErrorCode::Internal, e.to_string()))?
                    .into_value::<Option<String>>()
                    .map_err(|e| BrowserError::new(ErrorCode::Internal, e.to_string()))?
                    .ok_or_else(|| BrowserError::new(ErrorCode::SelectorNotFound, css.to_string()))?;
                let truncated = if html.len() > self.config.max_text_bytes {
                    let mut s = html.clone();
                    s.truncate(self.config.max_text_bytes);
                    s.push_str("…[truncated]");
                    s
                } else { html };
                Ok(json!({"ok": true, "html": truncated}))
            },
        ).await
    }

    pub async fn wait_for(&self, selector: &str, timeout_ms: u64) -> BrowserResult<Value> {
        self.guarded(
            "browser_wait_for",
            json!({"selector": selector, "timeout_ms": timeout_ms}),
            || async {
                let sel = Selector::parse(selector)?;
                self.ensure_started().await?;
                let guard = self.state.lock().await;
                let state = guard.as_ref().ok_or_else(|| {
                    BrowserError::new(ErrorCode::NotConnected, "chromium not running")
                })?;
                let css = sel.as_css().ok_or_else(|| {
                    BrowserError::new(ErrorCode::NotImplemented, "xpath:/text: selectors are v2")
                })?;
                let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
                loop {
                    if state.page.find_element(css).await.is_ok() {
                        return Ok(json!({"ok": true, "appeared": css}));
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(BrowserError::new(
                            ErrorCode::Timeout,
                            format!("{css} did not appear within {timeout_ms}ms"),
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        )
        .await
    }

    pub async fn back(&self) -> BrowserResult<Value> {
        self.guarded("browser_back", json!({}), || async {
            self.ensure_started().await?;
            let guard = self.state.lock().await;
            let state = guard.as_ref().ok_or_else(|| {
                BrowserError::new(ErrorCode::NotConnected, "chromium not running")
            })?;
            state
                .page
                .evaluate("history.back()")
                .await
                .map_err(|e| BrowserError::new(ErrorCode::Internal, e.to_string()))?;
            // history.back() navigates asynchronously — let it settle, then
            // re-validate the resulting origin against the allowlist.
            tokio::time::sleep(Duration::from_millis(300)).await;
            self.recheck_current_origin(&state.page).await?;
            Ok(json!({"ok": true}))
        })
        .await
    }

    pub async fn forward(&self) -> BrowserResult<Value> {
        self.guarded("browser_forward", json!({}), || async {
            self.ensure_started().await?;
            let guard = self.state.lock().await;
            let state = guard.as_ref().ok_or_else(|| {
                BrowserError::new(ErrorCode::NotConnected, "chromium not running")
            })?;
            state
                .page
                .evaluate("history.forward()")
                .await
                .map_err(|e| BrowserError::new(ErrorCode::Internal, e.to_string()))?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            self.recheck_current_origin(&state.page).await?;
            Ok(json!({"ok": true}))
        })
        .await
    }

    pub async fn reload(&self) -> BrowserResult<Value> {
        self.guarded("browser_reload", json!({}), || async {
            self.ensure_started().await?;
            let guard = self.state.lock().await;
            let state = guard.as_ref().ok_or_else(|| {
                BrowserError::new(ErrorCode::NotConnected, "chromium not running")
            })?;
            state
                .page
                .reload()
                .await
                .map_err(|e| BrowserError::new(ErrorCode::Internal, e.to_string()))?;
            self.recheck_current_origin(&state.page).await?;
            Ok(json!({"ok": true}))
        })
        .await
    }

    pub async fn close(&self) -> BrowserResult<Value> {
        let mut guard = self.state.lock().await;
        if let Some(state) = guard.take() {
            let DriverState {
                browser,
                page,
                _handler,
                interceptor,
                popup_closer,
            } = state;
            interceptor.abort();
            popup_closer.abort();
            let _ = interceptor.await;
            let _ = popup_closer.await;
            drop(page);
            // The tasks are gone: this is the last handle. Were it not, the
            // drop of the last one kills the process (`kill_on_drop`).
            if let Ok(mut browser) = Arc::try_unwrap(browser) {
                let _ = browser.close().await;
                let _ = browser.wait().await;
            }
        }
        if !self.config.keep_profile {
            let _ = tokio::fs::remove_dir_all(&self.profile_dir).await;
        }
        Ok(json!({"ok": true, "session": self.session_id}))
    }

    pub fn status(&self) -> Value {
        let (tokens, capacity) = self.rate_limit.snapshot();
        json!({
            "ok": true,
            "session": self.session_id,
            "headless": self.config.is_headless(),
            "profile_dir": self.profile_dir,
            "rate_limit": {
                "tokens_left": tokens.floor() as u32,
                "capacity_per_minute": capacity,
            },
            "allowlist": self.allowlist.rules(),
            "kill_switch": self.config.effective_kill_switch(),
        })
    }
}

/// Phase-2 stubs — registered as MCP tools but return `NotImplemented`.
/// Keeping them here makes the toolbelt complete from the agent's POV
/// (tools/list returns them) while flagging the work cleanly.
impl ChromiumDriver {
    pub async fn not_implemented(&self, what: &str) -> BrowserResult<Value> {
        Err(BrowserError::new(
            ErrorCode::NotImplemented,
            format!("{what} is planned for v2; v1 ships read+navigate+click+type+key+wait"),
        ))
    }
}
