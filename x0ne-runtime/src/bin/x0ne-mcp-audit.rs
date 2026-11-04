use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use x0ne_runtime::config::AppConfig;
use x0ne_runtime::mcp::registry::ToolRegistry;

fn main() {
    let registry = ToolRegistry::new();
    let runtime_tools = registry.tool_count();
    let mutable_tools = registry.tools().iter().filter(|tool| tool.mutable).count();
    let readonly_tools = runtime_tools.saturating_sub(mutable_tools);

    let mut categories = BTreeMap::<&str, usize>::new();
    for tool in registry.tools() {
        *categories.entry(tool.category.as_str()).or_insert(0) += 1;
    }

    let repo_root = repo_root();
    let orchestrator_tools = count_orchestrator_native_tools(&repo_root);
    let docs_claim = read_docs_claim(&repo_root);
    let claude_status = claude_mcp_status();
    let defaults = AppConfig::defaults();

    println!("X0NE MCP Audit");
    println!("Runtime tools: {runtime_tools}");
    println!("Runtime read-only tools: {readonly_tools}");
    println!("Runtime mutable tools: {mutable_tools}");
    match orchestrator_tools {
        Some(count) => println!("Orchestrator native tools: {count}"),
        None => println!("Orchestrator native tools: unavailable (source not found)"),
    }
    println!(
        "Docs claim: {}",
        docs_claim.as_deref().unwrap_or("not found")
    );
    println!("Policy default mode: {}", defaults.policy.mode);
    println!("Policy default effect: {}", defaults.policy.default_effect);
    println!(
        "Dangerous actions gated: {}",
        if defaults.policy.mode == "confirm" && defaults.policy.default_effect == "deny" {
            "yes"
        } else {
            "check config"
        }
    );
    println!(
        "Snapshot actions available: {}",
        has_snapshot_tools(&registry)
    );
    println!("Claude config: {claude_status}");
    println!("Categories:");
    for (category, count) in categories {
        println!("  {category}: {count}");
    }

    if let (Some(orchestrator), Some(claim)) = (orchestrator_tools, docs_claim.as_deref()) {
        let total = runtime_tools + orchestrator;
        let runtime_ok = claim.contains(&runtime_tools.to_string());
        let total_ok = claim.contains(&total.to_string());
        if runtime_ok || total_ok {
            println!("Result: ok");
        } else {
            println!("Result: docs mismatch (runtime={runtime_tools}, gateway_total={total})");
        }
    } else {
        println!("Result: incomplete audit");
    }
}

fn has_snapshot_tools(registry: &ToolRegistry) -> &'static str {
    let has_create = registry
        .tools()
        .iter()
        .any(|tool| tool.name == "create_snapshot");
    let has_restore = registry
        .tools()
        .iter()
        .any(|tool| tool.name == "restore_snapshot");
    if has_create && has_restore {
        "yes"
    } else {
        "no"
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn count_orchestrator_native_tools(repo_root: &PathBuf) -> Option<usize> {
    let source =
        std::fs::read_to_string(repo_root.join("x0ne-orchestrator/src/mcp_server.rs")).ok()?;
    let start = source.find("fn native_tools()")?;
    let tail = &source[start..];
    let end = tail.find("\nfn is_native_tool")?;
    Some(tail[..end].matches("NativeTool {").count())
}

fn read_docs_claim(repo_root: &PathBuf) -> Option<String> {
    let readme = std::fs::read_to_string(repo_root.join("README.md")).ok()?;
    readme
        .lines()
        .find(|line| line.contains("MCP tools") && line.contains("typed"))
        .map(|line| line.trim().to_string())
}

fn claude_mcp_status() -> String {
    let Ok(output) = Command::new("claude").args(["mcp", "list"]).output() else {
        return "claude CLI not found".to_string();
    };

    if !output.status.success() {
        return "claude mcp list failed".to_string();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout
        .lines()
        .any(|line| line.starts_with("x0ne:") || line.starts_with("x0ne "))
    {
        "installed".to_string()
    } else {
        "not installed".to_string()
    }
}
