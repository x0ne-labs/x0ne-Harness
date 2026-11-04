// `x0ne-dataset schema` — emit machine-readable specs of the Action enum.
//
//   ~/.x0ne/dataset/action_schema.json   — JSON Schema (anyOf, 58+ strict
//     branches plus a permissive catch-all) for SFT system prompts,
//     structured-output APIs, and offline validation.
//
//   ~/.x0ne/dataset/action.gbnf          — llama.cpp constrained-generation
//     grammar. Locks the `type` field to known PascalCase variants — any
//     model sampled with this grammar physically cannot hallucinate a tool
//     name.
//
// Source of truth: the MCP `ToolRegistry`. We pascalize snake_case tool names
// ("read_file" → "ReadFile") to match `Action`'s `#[serde(rename_all = "PascalCase")]`.

use crate::mcp::registry::ToolRegistry;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::Write;
use x0ne_dataset::{pascalize, x0ne_home};

/// Build one anyOf branch for a tool: `{ type: const, ...input_schema }`.
fn variant_schema(pascal: &str, input_schema: &Value) -> Value {
    let mut props = input_schema
        .get("properties")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let Some(obj) = props.as_object_mut() {
        obj.insert("type".to_string(), json!({ "const": pascal }));
    }
    let mut required: Vec<String> = input_schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if !required.contains(&"type".to_string()) {
        required.insert(0, "type".to_string());
    }
    // additionalProperties: true (override registry's common `false`) because
    // `to_action()` accepts more fields than the MCP schema advertises.
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": true,
    })
}

fn build_json_schema(pairs: &[(String, Value)]) -> Value {
    let mut one_of: Vec<Value> = pairs
        .iter()
        .map(|(pascal, sch)| variant_schema(pascal, sch))
        .collect();
    // Forward-compat catch-all for non-MCP-exposed variants (multi-agent,
    // orchestrator-side, historical). Without it, ~25% of real corpus actions
    // would fail validation.
    one_of.push(json!({
        "type": "object",
        "properties": {
            "type": { "type": "string", "description": "Non-MCP-exposed Action variant." }
        },
        "required": ["type"],
        "additionalProperties": true,
    }));
    // anyOf (not oneOf) because the catch-all overlaps every strict branch.
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "X0NE Action",
        "description": "Tagged union over every typed Action the X0NE runtime accepts. Discriminator: `type` (PascalCase). The last `anyOf` branch is a permissive catch-all.",
        "anyOf": one_of,
    })
}

fn build_gbnf(pascal_names: &[String]) -> String {
    let alts = pascal_names
        .iter()
        .map(|n| format!("\"{}\"", n))
        .collect::<Vec<_>>()
        .join(" | ");
    let mut g = String::new();
    g.push_str("# X0NE Action GBNF — auto-generated. Do not edit by hand.\n");
    g.push_str("# Locks the `type` field of an emitted Action JSON to a known\n");
    g.push_str("# PascalCase variant. Use with llama.cpp's --grammar-file.\n\n");
    g.push_str("root   ::= ws \"{\" ws action-pair (ws \",\" ws field-pair)* ws \"}\" ws\n");
    g.push_str("action-pair ::= \"\\\"type\\\"\" ws \":\" ws \"\\\"\" action-type \"\\\"\"\n");
    g.push_str(&format!("action-type ::= {}\n", alts));
    g.push_str("field-pair ::= string ws \":\" ws value\n");
    g.push_str(
        "value      ::= string | number | object | array | \"true\" | \"false\" | \"null\"\n",
    );
    g.push_str("object     ::= \"{\" ws (field-pair (ws \",\" ws field-pair)*)? ws \"}\"\n");
    g.push_str("array      ::= \"[\" ws (value (ws \",\" ws value)*)? ws \"]\"\n");
    g.push_str("string     ::= \"\\\"\" char* \"\\\"\"\n");
    g.push_str("char       ::= [^\"\\\\] | \"\\\\\" ([\"\\\\/bfnrt] | \"u\" [0-9a-fA-F]{4})\n");
    g.push_str(
        "number     ::= \"-\"? ([0-9] | [1-9][0-9]+) (\".\" [0-9]+)? ([eE] [+-]? [0-9]+)?\n",
    );
    g.push_str("ws         ::= [ \\t\\n\\r]*\n");
    g
}

pub fn run() -> Result<()> {
    let dir = x0ne_home().join("dataset");
    fs::create_dir_all(&dir).context("creating dataset dir")?;
    let registry = ToolRegistry::new();
    let pairs: Vec<(String, Value)> = registry
        .tools()
        .iter()
        .map(|t| (pascalize(&t.name), t.input_schema.clone()))
        .collect();

    let mut pascal_names: Vec<String> = pairs.iter().map(|(n, _)| n.clone()).collect();
    pascal_names.sort();
    pascal_names.dedup();

    let schema = build_json_schema(&pairs);
    let schema_path = dir.join("action_schema.json");
    let mut sf = File::create(&schema_path)?;
    writeln!(sf, "{}", serde_json::to_string_pretty(&schema)?)?;

    let gbnf = build_gbnf(&pascal_names);
    let gbnf_path = dir.join("action.gbnf");
    let mut gf = File::create(&gbnf_path)?;
    write!(gf, "{}", gbnf)?;

    eprintln!("[schema] {} variants exported", pairs.len());
    eprintln!("[schema] wrote {}", schema_path.display());
    eprintln!("[schema] wrote {}", gbnf_path.display());
    eprintln!();
    eprintln!("Next:");
    eprintln!(
        "  • Constrained gen with llama.cpp:  llama-cli --grammar-file {} ...",
        gbnf_path.display()
    );
    eprintln!(
        "  • Inline in SFT system prompt:     paste {} into the system message",
        schema_path.display()
    );
    Ok(())
}
