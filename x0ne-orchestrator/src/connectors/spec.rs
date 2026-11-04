//! `[connectors.<name>]`: what the operator declares, merged over the
//! built-in templates (`templates.rs`). Read from the user's config only:
//! a project config cannot start a process (`PROJECT_SETTABLE`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How the orchestrator reaches the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// A child process speaking line-delimited JSON-RPC on stdin/stdout.
    Stdio,
    /// MCP streamable HTTP: JSON-RPC over POST, JSON or SSE answers.
    Http,
}

/// One `[connectors.<name>]` table. Every field is optional so a table can
/// override a template one key at a time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Child environment. Values may reference `${secret:NAME}`,
    /// `${keyring:NAME}` or `${env:NAME}`; every resolved value is masked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    /// HTTP headers, same references as `env`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// Tool-name patterns (`*` wildcard) that run without a question.
    /// Everything else is a publishing action and is asked, yolo included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<Vec<String>>,
    /// Tool-name patterns never shown to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hide: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where the template's server comes from (URL). Informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl ConnectorConfig {
    /// `self` over `base`, key by key.
    pub fn over(&self, base: &ConnectorConfig) -> ConnectorConfig {
        ConnectorConfig {
            enabled: self.enabled.or(base.enabled),
            transport: self.transport.or(base.transport),
            command: self.command.clone().or_else(|| base.command.clone()),
            args: self.args.clone().or_else(|| base.args.clone()),
            url: self.url.clone().or_else(|| base.url.clone()),
            env: self.env.clone().or_else(|| base.env.clone()),
            headers: self.headers.clone().or_else(|| base.headers.clone()),
            read: self.read.clone().or_else(|| base.read.clone()),
            hide: self.hide.clone().or_else(|| base.hide.clone()),
            description: self
                .description
                .clone()
                .or_else(|| base.description.clone()),
            source: self.source.clone().or_else(|| base.source.clone()),
        }
    }
}

/// A connector after merging, validated.
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectorSpec {
    pub name: String,
    pub enabled: bool,
    pub transport: Transport,
    pub command: String,
    pub args: Vec<String>,
    pub url: String,
    pub env: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub read: Vec<String>,
    pub hide: Vec<String>,
    pub description: String,
    pub source: String,
    /// True when this came from a built-in template the operator never declared.
    pub template_only: bool,
}

/// A connector name: becomes the tool prefix, so no dot and nothing exotic.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// A server-supplied tool name we are willing to expose.
pub fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl ConnectorSpec {
    pub fn from_config(
        name: &str,
        cfg: &ConnectorConfig,
        template_only: bool,
    ) -> Result<Self, String> {
        if !valid_name(name) {
            return Err(format!(
                "connector name `{name}`: use lowercase letters, digits, `_` or `-`"
            ));
        }
        let transport = cfg.transport.unwrap_or(Transport::Stdio);
        let command = cfg.command.clone().unwrap_or_default();
        let url = cfg.url.clone().unwrap_or_default();
        match transport {
            Transport::Stdio if command.trim().is_empty() => {
                return Err(format!("connector `{name}`: stdio needs `command`"))
            }
            Transport::Http if !(url.starts_with("https://") || url.starts_with("http://")) => {
                return Err(format!("connector `{name}`: http needs an http(s) `url`"))
            }
            _ => {}
        }
        Ok(Self {
            name: name.to_string(),
            enabled: cfg.enabled.unwrap_or(false),
            transport,
            command,
            args: cfg.args.clone().unwrap_or_default(),
            url,
            env: cfg.env.clone().unwrap_or_default(),
            headers: cfg.headers.clone().unwrap_or_default(),
            read: cfg.read.clone().unwrap_or_default(),
            hide: cfg.hide.clone().unwrap_or_default(),
            description: cfg.description.clone().unwrap_or_default(),
            source: cfg.source.clone().unwrap_or_default(),
            template_only,
        })
    }
}

/// Templates, then the operator's tables over them. Invalid entries are
/// returned as errors, never silently dropped.
pub fn resolve_all(
    declared: &BTreeMap<String, ConnectorConfig>,
) -> (BTreeMap<String, ConnectorSpec>, Vec<String>) {
    let templates = super::templates::all();
    let mut out = BTreeMap::new();
    let mut errors = Vec::new();
    for (name, tpl) in &templates {
        if declared.contains_key(name) {
            continue;
        }
        match ConnectorSpec::from_config(name, tpl, true) {
            Ok(s) => {
                out.insert(name.clone(), s);
            }
            Err(e) => errors.push(e),
        }
    }
    for (name, user) in declared {
        let merged = match templates.get(name) {
            Some(tpl) => user.over(tpl),
            None => user.clone(),
        };
        match ConnectorSpec::from_config(name, &merged, false) {
            Ok(s) => {
                out.insert(name.clone(), s);
            }
            Err(e) => errors.push(e),
        }
    }
    (out, errors)
}

/// `*` matches any run of characters; everything else is literal.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    fn go(p: &[u8], n: &[u8]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some((b'*', rest)) => (0..=n.len()).any(|i| go(rest, &n[i..])),
            Some((c, rest)) => n.first() == Some(c) && go(rest, &n[1..]),
        }
    }
    go(pattern.as_bytes(), name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_could_forge_a_prefix_are_refused() {
        assert!(valid_name("github"));
        assert!(valid_name("my-gh_2"));
        for bad in ["", "GitHub", "a.b", "1gh", "a b", "x/y"] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(valid_tool_name("create_issue"));
        for bad in ["", "a.b", "a b", "x\u{1b}[2K"] {
            assert!(!valid_tool_name(bad), "{bad}");
        }
    }

    #[test]
    fn globs() {
        assert!(glob_match("get_*", "get_me"));
        assert!(glob_match("*_read", "issue_read"));
        assert!(glob_match("fetch", "fetch"));
        assert!(!glob_match("get_*", "create_issue"));
        assert!(!glob_match("fetch", "fetch2"));
    }

    #[test]
    fn templates_are_off_and_a_table_overrides_one_key() {
        let mut declared = BTreeMap::new();
        declared.insert(
            "github".to_string(),
            ConnectorConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );
        let (specs, errors) = resolve_all(&declared);
        assert!(errors.is_empty(), "{errors:?}");
        let gh = &specs["github"];
        assert!(gh.enabled && !gh.template_only);
        assert_eq!(gh.command, "github-mcp-server");
        for (name, s) in &specs {
            if name != "github" {
                assert!(!s.enabled, "template {name} is on by default");
                assert!(s.template_only);
            }
        }
    }
}
