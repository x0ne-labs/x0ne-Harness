//! Built-in connector templates. All disabled; none carries a credential.
//! `read` lists are allowlists: a tool absent from them is asked before it
//! runs, so a renamed or new tool upstream fails safe.

use std::collections::BTreeMap;

use super::spec::{ConnectorConfig, Transport};

fn strings(v: &[&str]) -> Option<Vec<String>> {
    Some(v.iter().map(|s| s.to_string()).collect())
}

fn map(v: &[(&str, &str)]) -> Option<BTreeMap<String, String>> {
    Some(
        v.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

pub fn all() -> BTreeMap<String, ConnectorConfig> {
    let mut t = BTreeMap::new();
    t.insert(
        "github".to_string(),
        ConnectorConfig {
            enabled: Some(false),
            transport: Some(Transport::Stdio),
            command: Some("github-mcp-server".into()),
            args: strings(&["stdio"]),
            env: map(&[("GITHUB_PERSONAL_ACCESS_TOKEN", "${secret:github_token}")]),
            read: strings(&["get_*", "list_*", "search_*", "*_read"]),
            description: Some("GitHub: repositories, issues, pull requests".into()),
            source: Some("https://github.com/github/github-mcp-server".into()),
            ..Default::default()
        },
    );
    t.insert(
        "gmail".to_string(),
        ConnectorConfig {
            enabled: Some(false),
            transport: Some(Transport::Stdio),
            command: Some("uvx".into()),
            args: strings(&["workspace-mcp", "--tools", "gmail"]),
            env: map(&[
                ("GOOGLE_OAUTH_CLIENT_ID", "${secret:google_oauth_client_id}"),
                (
                    "GOOGLE_OAUTH_CLIENT_SECRET",
                    "${secret:google_oauth_client_secret}",
                ),
            ]),
            read: strings(&["search_*", "get_*", "list_*"]),
            description: Some("Gmail, through Google Workspace MCP".into()),
            source: Some("https://github.com/taylorwilsdon/google_workspace_mcp".into()),
            ..Default::default()
        },
    );
    t.insert(
        "gcalendar".to_string(),
        ConnectorConfig {
            enabled: Some(false),
            transport: Some(Transport::Stdio),
            command: Some("uvx".into()),
            args: strings(&["workspace-mcp", "--tools", "calendar"]),
            env: map(&[
                ("GOOGLE_OAUTH_CLIENT_ID", "${secret:google_oauth_client_id}"),
                (
                    "GOOGLE_OAUTH_CLIENT_SECRET",
                    "${secret:google_oauth_client_secret}",
                ),
            ]),
            read: strings(&["get_*", "list_*", "query_*"]),
            description: Some("Google Calendar, through Google Workspace MCP".into()),
            source: Some("https://github.com/taylorwilsdon/google_workspace_mcp".into()),
            ..Default::default()
        },
    );
    t.insert(
        "slack".to_string(),
        ConnectorConfig {
            enabled: Some(false),
            transport: Some(Transport::Stdio),
            command: Some("npx".into()),
            args: strings(&["-y", "@zencoderai/slack-mcp-server"]),
            env: map(&[
                ("SLACK_BOT_TOKEN", "${secret:slack_bot_token}"),
                ("SLACK_TEAM_ID", "${secret:slack_team_id}"),
            ]),
            read: strings(&["slack_list_*", "slack_get_*"]),
            description: Some("Slack: channels, history, messages".into()),
            source: Some("https://github.com/zencoderai/slack-mcp-server".into()),
            ..Default::default()
        },
    );
    t.insert(
        "filesystem".to_string(),
        ConnectorConfig {
            enabled: Some(false),
            transport: Some(Transport::Stdio),
            command: Some("npx".into()),
            args: strings(&["-y", "@modelcontextprotocol/server-filesystem", "."]),
            read: strings(&[
                "read_*",
                "list_*",
                "directory_tree",
                "search_files",
                "get_file_info",
            ]),
            description: Some("MCP reference filesystem server, rooted at `.`".into()),
            source: Some(
                "https://github.com/modelcontextprotocol/servers/tree/main/src/filesystem".into(),
            ),
            ..Default::default()
        },
    );
    t.insert(
        "fetch".to_string(),
        ConnectorConfig {
            enabled: Some(false),
            transport: Some(Transport::Stdio),
            command: Some("uvx".into()),
            args: strings(&["mcp-server-fetch"]),
            // Empty on purpose: a URL can carry data out, so every fetch asks.
            read: strings(&[]),
            description: Some("MCP reference fetch server (web page to markdown)".into()),
            source: Some(
                "https://github.com/modelcontextprotocol/servers/tree/main/src/fetch".into(),
            ),
            ..Default::default()
        },
    );
    t
}
