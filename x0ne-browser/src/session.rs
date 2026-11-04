//! High-level configuration for a browser session.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserConfig {
    /// One of: "headed" (default, window visible) or "headless".
    /// Override at runtime via env X0NE_BROWSER_HEADLESS=1.
    #[serde(default = "default_mode")]
    pub mode: String,

    /// Where to put the per-session Chromium profile. Default:
    /// `~/.x0ne/browser-profiles/<session-uuid>/`. Wiped at session end
    /// unless `keep_profile = true`.
    pub profile_root: Option<PathBuf>,
    #[serde(default)]
    pub keep_profile: bool,

    /// Max actions per minute. Soft-throttled past that.
    #[serde(default = "default_rate")]
    pub rate_limit_per_minute: u32,

    /// Default timeout for wait_for and navigation.
    #[serde(default = "default_timeout")]
    pub default_timeout_ms: u64,

    /// Max screenshot bytes (downsample beyond this).
    #[serde(default = "default_screenshot_max")]
    pub max_screenshot_bytes: usize,

    /// Max read_text payload, in bytes (truncated past this).
    #[serde(default = "default_text_max")]
    pub max_text_bytes: usize,

    /// read_dom default depth.
    #[serde(default = "default_dom_depth")]
    pub default_dom_depth: u32,

    /// Allowlist of url-glob patterns. See `allowlist.rs`.
    #[serde(default)]
    pub allowlist: Vec<String>,

    /// Path to the optional kill switch file. When it exists, all calls
    /// return KilledByOperator. Default `~/.x0ne/browser-tools.kill`.
    pub kill_switch_path: Option<PathBuf>,
}

fn default_mode() -> String {
    "headed".into()
}
fn default_rate() -> u32 {
    60
}
fn default_timeout() -> u64 {
    10_000
}
fn default_screenshot_max() -> usize {
    5 * 1024 * 1024
} // 5 MB
fn default_text_max() -> usize {
    256 * 1024
} // 256 KB
fn default_dom_depth() -> u32 {
    8
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            mode: default_mode(),
            profile_root: None,
            keep_profile: false,
            rate_limit_per_minute: default_rate(),
            default_timeout_ms: default_timeout(),
            max_screenshot_bytes: default_screenshot_max(),
            max_text_bytes: default_text_max(),
            default_dom_depth: default_dom_depth(),
            allowlist: vec![],
            kill_switch_path: None,
        }
    }
}

impl BrowserConfig {
    /// Resolve overrides from environment variables. Mutates self in-place.
    pub fn apply_env(&mut self) {
        if std::env::var("X0NE_BROWSER_HEADLESS").as_deref() == Ok("1") {
            self.mode = "headless".into();
        }
        if let Ok(n) = std::env::var("X0NE_BROWSER_RATE_LIMIT") {
            if let Ok(n) = n.parse() {
                self.rate_limit_per_minute = n;
            }
        }
    }

    pub fn is_headless(&self) -> bool {
        self.mode.eq_ignore_ascii_case("headless")
    }

    /// `~/.x0ne/browser-tools.toml` when there is one, else the defaults.
    ///
    /// A file that does not read or parse is an error, not the defaults: the
    /// defaults have no allowlist, so falling back on them lifted whatever
    /// the operator had restricted, silently. (Audit of 2026-10-30, H4-5.)
    pub fn from_default_path() -> Result<Self, String> {
        dirs::home_dir().map_or_else(
            || Ok(Self::default()),
            |h| Self::from_path(&h.join(".x0ne").join("browser-tools.toml")),
        )
    }

    /// The configuration at `path`; the defaults when there is no file there.
    pub fn from_path(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let s = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        toml::from_str(&s).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn effective_kill_switch(&self) -> PathBuf {
        self.kill_switch_path.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".x0ne")
                .join("browser-tools.kill")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = BrowserConfig::default();
        assert_eq!(c.mode, "headed");
        assert_eq!(c.rate_limit_per_minute, 60);
        assert_eq!(c.max_screenshot_bytes, 5 * 1024 * 1024);
        assert_eq!(c.default_dom_depth, 8);
        assert!(!c.is_headless());
    }

    /// H4-5 (audit of 2026-10-30): the operator pinned the browser to the
    /// intranet and lost a bracket. The defaults have no allowlist, so a
    /// parse failure that fell back on them lifted the restriction without a
    /// word. A file that does not parse is a refusal, not a configuration.
    #[test]
    fn a_browser_tools_toml_that_does_not_parse_is_an_error_not_the_defaults() {
        let dir = std::env::temp_dir().join(format!("x0ne-browser-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("browser-tools.toml");
        std::fs::write(
            &p,
            "allowlist = [\"https://intranet.example/*\"\nmode = \"headless\"\n",
        )
        .unwrap();
        let r = BrowserConfig::from_path(&p);
        let absent = BrowserConfig::from_path(&dir.join("absent.toml"));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            r.is_err(),
            "a file that does not parse came back as a configuration: {r:?}"
        );
        assert!(
            absent.is_ok_and(|c| c.allowlist.is_empty()),
            "no file at all is still the defaults"
        );
    }

    #[test]
    fn env_override_headless() {
        // Save/restore so we don't pollute the test suite.
        let prev = std::env::var("X0NE_BROWSER_HEADLESS").ok();
        std::env::set_var("X0NE_BROWSER_HEADLESS", "1");
        let mut c = BrowserConfig::default();
        c.apply_env();
        assert!(c.is_headless());
        match prev {
            Some(v) => std::env::set_var("X0NE_BROWSER_HEADLESS", v),
            None => std::env::remove_var("X0NE_BROWSER_HEADLESS"),
        }
    }

    #[test]
    fn rate_limit_env_override() {
        let prev = std::env::var("X0NE_BROWSER_RATE_LIMIT").ok();
        std::env::set_var("X0NE_BROWSER_RATE_LIMIT", "120");
        let mut c = BrowserConfig::default();
        c.apply_env();
        assert_eq!(c.rate_limit_per_minute, 120);
        match prev {
            Some(v) => std::env::set_var("X0NE_BROWSER_RATE_LIMIT", v),
            None => std::env::remove_var("X0NE_BROWSER_RATE_LIMIT"),
        }
    }
}
