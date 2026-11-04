//! Native OpenAI function calling for the action protocol. Returned
//! `tool_calls` are rewritten into canonical `<action>` text, so the parser,
//! policy and confirmation gates stay the single path.

use serde_json::{json, Map, Value};
use std::sync::OnceLock;

/// Actions offered as native tools. The rest of the vocabulary stays
/// reachable through the text protocol described in the system prompt.
pub const NATIVE_ACTIONS: &[&str] = &[
    "ReadFile",
    "WriteFile",
    "EditFile",
    "RunCommand",
    "SearchCode",
];

/// One tool call as the server returned it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    /// JSON object, as a string (OpenAI wire format).
    pub arguments: String,
}

/// The `tools` array sent with each agent request.
pub fn action_tools() -> &'static [Value] {
    static TOOLS: OnceLock<Vec<Value>> = OnceLock::new();
    TOOLS.get_or_init(|| NATIVE_ACTIONS.iter().filter_map(|n| tool_for(n)).collect())
}

/// One OpenAI function, derived from the build-time action catalog.
fn tool_for(name: &str) -> Option<Value> {
    let meta = x0ne_actions::action_meta(name)?;
    let mut properties = Map::new();
    for (field, ty) in meta.field_types {
        properties.insert((*field).to_string(), field_schema(name, field, ty));
    }
    let description = match name {
        // The catalog summary says "shell command"; there is no shell.
        "RunCommand" => "Run ONE program without a shell. `command` is the program name, \
                         `args` its arguments, one per element. No pipes, `&&`, redirections \
                         or globs."
            .to_string(),
        _ => meta.summary.to_string(),
    };
    Some(json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": properties,
                "required": meta.required_fields,
            }
        }
    }))
}

fn field_schema(action: &str, field: &str, ty: &str) -> Value {
    match (action, field, ty) {
        ("EditFile", "edits", _) => json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "old": {"type": "string"},
                    "new": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                },
                "required": ["old", "new"]
            }
        }),
        (_, _, "array") => json!({"type": "array", "items": {"type": "string"}}),
        (_, _, "any") => json!({}),
        (_, _, t) => json!({"type": t}),
    }
}

/// Native calls as canonical `<action>…</action>` text. The function name
/// sets `type` (overrides the arguments); several calls become one
/// `Pipeline`; unparseable arguments yield text the parser rejects.
pub fn tool_calls_to_action_text(calls: &[ToolCall]) -> Option<String> {
    let mut actions = Vec::with_capacity(calls.len());
    for call in calls {
        match serde_json::from_str::<Value>(&call.arguments) {
            Ok(Value::Object(mut obj)) => {
                obj.insert("type".into(), Value::String(call.name.clone()));
                actions.push(Value::Object(obj));
            }
            _ => {
                // `{"type":…,"<raw as a JSON string>"}` never parses: the
                // raw text cannot close the object or add a field.
                return Some(format!(
                    "<action>{{\"type\":{},{}}}</action>",
                    Value::String(call.name.clone()),
                    Value::String(call.arguments.clone())
                ));
            }
        }
    }
    let action = match actions.len() {
        0 => return None,
        1 => actions.pop()?,
        _ => json!({"type": "Pipeline", "steps": actions, "stop_on_error": true}),
    };
    Some(format!("<action>{action}</action>"))
}

/// `choices[0].message.tool_calls` of a non-streamed answer.
pub fn tool_calls_of_message(message: &Value) -> Vec<ToolCall> {
    let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
        return Vec::new();
    };
    calls
        .iter()
        .filter_map(|c| {
            let f = c.get("function")?;
            let name = f.get("name")?.as_str()?.to_string();
            let arguments = match f.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "{}".into(),
            };
            Some(ToolCall { name, arguments })
        })
        .collect()
}

/// More parallel calls than any model emits in one turn; a larger `index`
/// is ignored rather than allocated.
const MAX_STREAMED_CALLS: usize = 32;

/// Accumulates `delta.tool_calls` fragments of an SSE stream, by index.
#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    calls: Vec<ToolCall>,
}

impl ToolCallAccumulator {
    pub fn push_delta(&mut self, delta: &Value) {
        let Some(parts) = delta.get("tool_calls").and_then(Value::as_array) else {
            return;
        };
        for part in parts {
            let index = part.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if index >= MAX_STREAMED_CALLS {
                continue;
            }
            if self.calls.len() <= index {
                self.calls.resize_with(index + 1, ToolCall::default);
            }
            let slot = &mut self.calls[index];
            if let Some(f) = part.get("function") {
                if let Some(n) = f.get("name").and_then(Value::as_str) {
                    slot.name.push_str(n);
                }
                match f.get("arguments") {
                    Some(Value::String(a)) => slot.arguments.push_str(a),
                    Some(Value::Null) | None => {}
                    Some(other) => slot.arguments.push_str(&other.to_string()),
                }
            }
        }
    }

    pub fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .filter(|c| !c.name.is_empty())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{parse_llm_response, LlmOutput};

    fn action_of(text: &str) -> Value {
        match parse_llm_response(text) {
            LlmOutput::Action { action, .. } => action,
            other => panic!("expected an action from {text:?}, got {other:?}"),
        }
    }

    #[test]
    fn the_core_actions_are_offered_as_openai_functions() {
        let tools = action_tools();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, NATIVE_ACTIONS);
        for t in tools {
            assert_eq!(t["type"], "function");
            assert_eq!(t["function"]["parameters"]["type"], "object");
        }
        let write = &tools[1]["function"]["parameters"];
        assert_eq!(write["required"], json!(["path", "content"]));
        assert_eq!(write["properties"]["content"]["type"], "string");
        let run = &tools[3]["function"];
        assert_eq!(
            run["parameters"]["properties"]["args"]["items"]["type"],
            "string"
        );
        assert!(
            !run["description"]
                .as_str()
                .unwrap()
                .contains("shell command"),
            "RunCommand runs no shell; the tool must not say it does"
        );
    }

    /// The measured failure, through the native path. The server
    /// decoded the arguments once; the file gets real newlines.
    #[test]
    fn a_native_write_file_call_becomes_a_canonical_action() {
        let calls = [ToolCall {
            name: "WriteFile".into(),
            arguments: r#"{"path":"/work/fib.py","content":"a, b = 0, 1\nfor _ in range(10):\n    print(a)\n"}"#.into(),
        }];
        let text = tool_calls_to_action_text(&calls).expect("one call, one action");
        let action = action_of(&text);
        assert_eq!(action["type"], "WriteFile");
        assert_eq!(
            action["content"],
            "a, b = 0, 1\nfor _ in range(10):\n    print(a)\n"
        );
    }

    /// SECURITY: the function name decides the type. A `type` smuggled in
    /// the arguments must not turn a ReadFile call into something else.
    #[test]
    fn the_function_name_wins_over_a_type_in_the_arguments() {
        let calls = [ToolCall {
            name: "ReadFile".into(),
            arguments: r#"{"type":"MoveToTrash","path":"/etc/hosts"}"#.into(),
        }];
        let action = action_of(&tool_calls_to_action_text(&calls).unwrap());
        assert_eq!(action["type"], "ReadFile");
    }

    #[test]
    fn several_calls_become_one_pipeline() {
        let calls = [
            ToolCall {
                name: "WriteFile".into(),
                arguments: r#"{"path":"/w/a","content":"x"}"#.into(),
            },
            ToolCall {
                name: "RunCommand".into(),
                arguments: r#"{"command":"python3","args":["/w/a"]}"#.into(),
            },
        ];
        let action = action_of(&tool_calls_to_action_text(&calls).unwrap());
        assert_eq!(action["type"], "Pipeline");
        assert_eq!(action["steps"][1]["type"], "RunCommand");
        assert_eq!(action["stop_on_error"], true);
    }

    /// Unparseable arguments surface as a parse error the model can fix,
    /// never as a half-built action.
    #[test]
    fn invalid_arguments_are_a_parse_error() {
        let calls = [ToolCall {
            name: "WriteFile".into(),
            arguments: "{\"path\":".into(),
        }];
        let text = tool_calls_to_action_text(&calls).unwrap();
        assert!(
            matches!(parse_llm_response(&text), LlmOutput::ParseError { .. }),
            "{text}"
        );
        let calls = [ToolCall {
            name: "WriteFile".into(),
            arguments: "[1,2]".into(),
        }];
        let text = tool_calls_to_action_text(&calls).unwrap();
        assert!(
            matches!(parse_llm_response(&text), LlmOutput::ParseError { .. }),
            "{text}"
        );
    }

    #[test]
    fn no_call_means_no_action_text() {
        assert_eq!(tool_calls_to_action_text(&[]), None);
    }

    #[test]
    fn message_tool_calls_are_read() {
        let msg = json!({"role":"assistant","content":null,"tool_calls":[
            {"id":"c1","type":"function","function":{"name":"RunCommand","arguments":"{\"command\":\"ls\"}"}}
        ]});
        assert_eq!(
            tool_calls_of_message(&msg),
            [ToolCall {
                name: "RunCommand".into(),
                arguments: "{\"command\":\"ls\"}".into()
            }]
        );
        // Some servers send the arguments as an object rather than a string.
        let msg =
            json!({"tool_calls":[{"function":{"name":"ReadFile","arguments":{"path":"/a"}}}]});
        assert_eq!(tool_calls_of_message(&msg)[0].arguments, r#"{"path":"/a"}"#);
    }

    #[test]
    fn streamed_fragments_are_reassembled_by_index() {
        let mut acc = ToolCallAccumulator::default();
        for d in [
            json!({"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"Write","arguments":""}}]}),
            json!({"tool_calls":[{"index":0,"function":{"name":"File","arguments":"{\"path\":\"/w/a\","}}]}),
            json!({"tool_calls":[{"index":1,"function":{"name":"RunCommand","arguments":"{\"command\":\"ls\"}"}}]}),
            json!({"tool_calls":[{"index":0,"function":{"arguments":"\"content\":\"x\"}"}}]}),
            json!({"content":"ignored"}),
        ] {
            acc.push_delta(&d);
        }
        assert_eq!(
            acc.finish(),
            [
                ToolCall {
                    name: "WriteFile".into(),
                    arguments: r#"{"path":"/w/a","content":"x"}"#.into()
                },
                ToolCall {
                    name: "RunCommand".into(),
                    arguments: r#"{"command":"ls"}"#.into()
                },
            ]
        );
    }

    /// A hostile stream cannot make the accumulator allocate without bound.
    #[test]
    fn an_absurd_index_is_ignored() {
        let mut acc = ToolCallAccumulator::default();
        acc.push_delta(&json!({"tool_calls":[{"index":4000000000u64,"function":{"name":"X","arguments":"{}"}}]}));
        assert!(acc.finish().is_empty());
    }
}
