//! Documentation generated from the action vocabulary.
//!
//! Every hand-written catalog in this workspace drifted, and the documentation
//! drifted worst of all because nothing ever executes it. This renders the
//! reference from `x0ne_actions::GENERATED_ACTIONS`, so it cannot describe an
//! action the runtime would reject, nor omit one it accepts.

/// The full action reference, as Markdown.
pub fn action_reference_markdown() -> String {
    let actions = x0ne_actions::GENERATED_ACTIONS;
    let mut out = String::new();

    out.push_str("# Action Reference\n\n");
    out.push_str(
        "<!-- GENERATED FILE — do not edit by hand.\n\
         \x20    Regenerate with:\n\
         \x20      cargo run -p x0ne-runtime --bin gen-action-docs > docs/actions.md\n\
         \x20    The content comes from the `Action` enum in x0ne-actions, so this file\n\
         \x20    cannot fall out of step with what the runtime actually accepts. -->\n\n",
    );
    out.push_str(&format!(
        "X0NE exposes **{} typed actions**. Every action is a JSON object with a \
         `type` field naming the action and the fields below as its parameters:\n\n",
        actions.len()
    ));
    out.push_str("```json\n{ \"type\": \"ReadFile\", \"path\": \"/tmp/foo.txt\" }\n```\n\n");
    out.push_str(
        "The **class** column is the side-effect classification the policy engine \
         derives every access decision from. `ReadOnly` and `Meta` actions change \
         nothing; the rest are gated.\n\n",
    );

    // Group by class so the reference reads as a capability map rather than an
    // alphabetical dump.
    let mut classes: Vec<&str> = actions.iter().map(|a| a.class).collect();
    classes.sort_unstable();
    classes.dedup();

    out.push_str("## Index\n\n| Class | Actions |\n|---|---|\n");
    for class in &classes {
        let names: Vec<&str> = actions
            .iter()
            .filter(|a| a.class == *class)
            .map(|a| a.name)
            .collect();
        out.push_str(&format!(
            "| `{}` | {} |\n",
            class,
            names
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out.push('\n');

    for class in &classes {
        out.push_str(&format!("## {class}\n\n"));
        for action in actions.iter().filter(|a| a.class == *class) {
            out.push_str(&format!("### {}\n\n", action.name));
            if !action.summary.is_empty() {
                out.push_str(&format!("{}\n\n", action.summary));
            }
            if action.required_fields.is_empty() && action.optional_fields.is_empty() {
                out.push_str("Takes no parameters.\n\n");
                continue;
            }
            out.push_str("| Field | Type | Required |\n|---|---|---|\n");
            for field in action.required_fields {
                out.push_str(&format!(
                    "| `{}` | `{}` | yes |\n",
                    field,
                    field_type(action, field)
                ));
            }
            for field in action.optional_fields {
                out.push_str(&format!(
                    "| `{}` | `{}` | no |\n",
                    field,
                    field_type(action, field)
                ));
            }
            out.push('\n');
        }
    }

    out
}

fn field_type(action: &x0ne_actions::ActionMeta, field: &str) -> &'static str {
    action
        .field_types
        .iter()
        .find(|(name, _)| *name == field)
        .map(|(_, ty)| *ty)
        .unwrap_or("any")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ANCHOR: the checked-in reference must match what the vocabulary says.
    /// Documentation is the catalog nobody notices going stale — it does not
    /// compile, no test exercises it, and a reader has no way to tell. This
    /// makes it fail like anything else.
    #[test]
    fn action_reference_is_up_to_date() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("docs/actions.md");
        let checked_in = std::fs::read_to_string(&path).expect("docs/actions.md exists");
        assert_eq!(
            checked_in,
            action_reference_markdown(),
            "docs/actions.md is out of step with the Action vocabulary. \
             Regenerate it:\n  cargo run -p x0ne-runtime --bin gen-action-docs > docs/actions.md"
        );
    }

    /// And the generated reference must never name something removed.
    #[test]
    fn generated_reference_names_only_live_actions() {
        let md = action_reference_markdown();
        for dead in ["ListDir", "AppendFile", "SystemInfo", "TreeView", "Whoami"] {
            assert!(
                !md.contains(&format!("### {dead}")),
                "{dead} was removed but appears in the generated reference"
            );
        }
    }

    /// ANCHOR: a removed action must not linger in anything a model or an
    /// operator reads. Prose is the catalog nobody notices going stale, and
    /// `SKILL.md` in particular is fed to an agent verbatim — it was teaching
    /// 20 actions the runtime rejects.
    ///
    /// Scans JSON action literals only. Prose that merely mentions a name in
    /// passing (a changelog, a "removed in v0.3" ledger) is not a claim that
    /// the action exists.
    #[test]
    fn no_model_or_operator_facing_file_offers_a_removed_action() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        // Names the orchestrator dispatches itself, plus tui's frontend
        // intercepts: real operations, just not runtime actions.
        const NON_RUNTIME: &[&str] = &[
            "Plan",
            "SpawnWorld",
            "SpawnAgent",
            "WaitAgent",
            "ListAgents",
            "BroadcastAgents",
            "GetBroadcasts",
            "AcknowledgeBroadcast",
            "GetBroadcastReceipts",
            "Reflect",
            "Matrix",
            "ArtifactExpose",
            "ArtifactUnexpose",
            "Budget",
            "ProjectNote",
            "RunIncus",
            "RunIncusGroup",
            "IncusCreateSnapshot",
            "IncusRestoreSnapshot",
            "IncusListSnapshots",
            "IncusDeleteSnapshot",
            "PaintBadge",
            "AskUser",
            "BadgeWatch",
            "BadgeWatchClear",
            "TaskAdd",
            "TaskDone",
            "TaskClear",
            "CronSchedule",
            "CronCancel",
        ];
        for rel in [
            "SKILL.md",
            "docs/actions.md",
            "README.md",
            "x0ne-runtime/README.md",
            // Training material is the most durable catalog of all: an action
            // taught here ends up in model weights. The curation scorer was
            // actively *rewarding* the use of an action removed in v0.3.
            "X0NE-FINETUNE-MANUAL.md",
        ] {
            let path = root.join(rel);
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            for line in text.lines() {
                let trimmed = line.trim();
                // Three shapes carry an action name: a plain JSON literal, an
                // escaped one inside a training JSONL record, and the eval
                // harness's `expected_action`.
                let candidates = [
                    trimmed.strip_prefix("{ \"type\": \""),
                    trimmed.split("\\\"type\\\":\\\"").nth(1),
                    trimmed.split("\"expected_action\": \"").nth(1),
                ];
                for rest in candidates.into_iter().flatten() {
                    let Some(name) = rest.split(['"', '\\']).next() else {
                        continue;
                    };
                    // Only PascalCase names are action names; `"type": "string"`
                    // inside a JSON Schema is a different vocabulary.
                    if !name.starts_with(|c: char| c.is_ascii_uppercase()) {
                        continue;
                    }
                    assert!(
                        x0ne_actions::all_type_names().contains(&name)
                            || NON_RUNTIME.contains(&name),
                        "{rel} offers '{name}', which no longer exists"
                    );
                }
            }
        }
    }
}
