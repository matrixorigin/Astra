//! Recent-argument hint extraction for prompt injection (gap #5).
//!
//! Given a small sliding window of recently-seen tool calls (by name + JSON
//! arguments), produce a compact block of paths and shell commands the agent
//! just touched. Downstream callers inject this block into the system / tool
//! descriptions so the model reuses known-good arguments rather than
//! re-discovering them.
//!
//! This module is **pure** and has no I/O. It mirrors the philosophy of
//! [`crate::tool_argument_hints`] — one canonical source of truth for which
//! argument keys represent paths (`path`) and commands (`command`) — and
//! delegates extraction to those helpers so behavior stays consistent with
//! permission prompts and journal previews.

use serde_json::Value;

use crate::tool_argument_hints::{
    command_hint_from_args, normalize_llm_function_arguments, path_hint_from_args,
};

/// Maximum number of distinct paths to surface.
pub const MAX_RECENT_PATHS: usize = 5;

/// Maximum number of distinct shell commands to surface.
pub const MAX_RECENT_COMMANDS: usize = 3;

/// Extracted hints from recent tool calls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecentArgHints {
    /// Distinct paths in MRU order (most recent first). Cap
    /// [`MAX_RECENT_PATHS`].
    pub paths: Vec<String>,
    /// Distinct shell commands in MRU order. Cap [`MAX_RECENT_COMMANDS`].
    pub commands: Vec<String>,
}

impl RecentArgHints {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.commands.is_empty()
    }

    /// Render a prompt-friendly text block. Returns `None` when no hints
    /// were collected so callers can cleanly skip injection.
    ///
    /// Output format (stable for downstream diffing):
    /// ```text
    /// Recent working context (reuse these argument values when applicable):
    ///   paths:
    ///     - src/lib.rs
    ///     - Cargo.toml
    ///   commands:
    ///     - cargo test
    /// ```
    pub fn render_prompt_block(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut out = String::new();
        out.push_str(
            "Recent working context (reuse these argument values when applicable):\n",
        );
        if !self.paths.is_empty() {
            out.push_str("  paths:\n");
            for p in &self.paths {
                out.push_str("    - ");
                out.push_str(p);
                out.push('\n');
            }
        }
        if !self.commands.is_empty() {
            out.push_str("  commands:\n");
            for c in &self.commands {
                out.push_str("    - ");
                out.push_str(c);
                out.push('\n');
            }
        }
        Some(out)
    }
}

/// Build hints from an iterator of `(tool_name, arguments)` entries ordered
/// **most-recent first**. The `arguments` value may be either a JSON object
/// or a stringified JSON object — the same normalization rule used by the
/// permission prompts applies here.
///
/// Duplicate paths / commands are removed (first occurrence wins, preserving
/// MRU order). Lists are truncated to the constants above.
pub fn build_recent_arg_hints<'a, I>(recent_calls: I) -> RecentArgHints
where
    I: IntoIterator<Item = (&'a str, &'a Value)>,
{
    let mut hints = RecentArgHints::default();
    for (_tool_name, raw_args) in recent_calls {
        let args = normalize_llm_function_arguments(raw_args);
        if hints.paths.len() < MAX_RECENT_PATHS {
            if let Some(p) = path_hint_from_args(&args) {
                if !hints.paths.iter().any(|existing| existing == &p) {
                    hints.paths.push(p);
                }
            }
        }
        if hints.commands.len() < MAX_RECENT_COMMANDS {
            if let Some(c) = command_hint_from_args(&args) {
                let owned = c.to_string();
                if !hints.commands.iter().any(|existing| existing == &owned) {
                    hints.commands.push(owned);
                }
            }
        }
        if hints.paths.len() >= MAX_RECENT_PATHS
            && hints.commands.len() >= MAX_RECENT_COMMANDS
        {
            break;
        }
    }
    hints
}

/// Walk a chat-history slice (most recent message first preferred) and extract
/// `(tool_name, arguments)` pairs from every assistant `tool_calls` entry.
///
/// Each assistant message may carry multiple tool calls; each tool call has
/// `function.name` and `function.arguments` (a JSON-encoded string per the
/// OpenAI schema). Arguments that are already JSON values are accepted too.
///
/// The returned list is in **MRU order** (most recent tool call first) so it
/// can be handed straight to [`build_recent_arg_hints`].
#[must_use]
pub fn extract_recent_tool_calls_from_messages(messages: &[Value]) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for msg in messages.iter().rev() {
        let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for tc in calls {
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .or_else(|| tc.get("name").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            let args_raw = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .or_else(|| tc.get("arguments"))
                .cloned()
                .unwrap_or(Value::Null);
            let args = match args_raw {
                Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::Null),
                other => other,
            };
            out.push((name, args));
        }
    }
    out
}

/// Convenience wrapper: extract tool calls from messages, build hints,
/// render the prompt block in one shot. Returns `None` when nothing
/// usable was found.
#[must_use]
pub fn prompt_block_from_messages(messages: &[Value]) -> Option<String> {
    let calls = extract_recent_tool_calls_from_messages(messages);
    let borrowed: Vec<(&str, &Value)> = calls
        .iter()
        .map(|(n, v)| (n.as_str(), v))
        .collect();
    let hints = build_recent_arg_hints(borrowed);
    hints.render_prompt_block()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_input_yields_no_hints() {
        let hints = build_recent_arg_hints(std::iter::empty());
        assert!(hints.is_empty());
        assert!(hints.render_prompt_block().is_none());
    }

    #[test]
    fn single_path_and_command_are_collected() {
        let a1 = json!({"path": "src/lib.rs"});
        let a2 = json!({"command": "cargo test"});
        let calls = vec![("read_file", &a1), ("bash", &a2)];
        let hints = build_recent_arg_hints(calls);
        assert_eq!(hints.paths, vec!["src/lib.rs".to_string()]);
        assert_eq!(hints.commands, vec!["cargo test".to_string()]);
    }

    #[test]
    fn duplicate_paths_deduped_preserving_mru_order() {
        let a1 = json!({"path": "src/lib.rs"});
        let a2 = json!({"path": "src/main.rs"});
        let a3 = json!({"path": "src/lib.rs"}); // duplicate, older
        let calls = vec![("read_file", &a1), ("read_file", &a2), ("read_file", &a3)];
        let hints = build_recent_arg_hints(calls);
        assert_eq!(
            hints.paths,
            vec!["src/lib.rs".to_string(), "src/main.rs".to_string()]
        );
    }

    #[test]
    fn stringified_arguments_are_normalized() {
        let stringy = json!(r#"{"path": "from_string.rs"}"#);
        let hints = build_recent_arg_hints([("read_file", &stringy)]);
        assert_eq!(hints.paths, vec!["from_string.rs".to_string()]);
    }

    #[test]
    fn path_cap_enforced() {
        let values: Vec<Value> = (0..(MAX_RECENT_PATHS + 3))
            .map(|i| json!({"path": format!("f{i}.rs")}))
            .collect();
        let calls: Vec<(&str, &Value)> =
            values.iter().map(|v| ("read_file", v)).collect();
        let hints = build_recent_arg_hints(calls);
        assert_eq!(hints.paths.len(), MAX_RECENT_PATHS);
        assert_eq!(hints.paths[0], "f0.rs");
    }

    #[test]
    fn command_cap_enforced() {
        let values: Vec<Value> = (0..(MAX_RECENT_COMMANDS + 2))
            .map(|i| json!({"command": format!("cmd{i}")}))
            .collect();
        let calls: Vec<(&str, &Value)> = values.iter().map(|v| ("bash", v)).collect();
        let hints = build_recent_arg_hints(calls);
        assert_eq!(hints.commands.len(), MAX_RECENT_COMMANDS);
        assert_eq!(hints.commands[0], "cmd0");
    }

    #[test]
    fn legacy_argument_keys_ignored_by_design() {
        // Consistent with tool_argument_hints: only canonical `path` /
        // `command` keys are recognized.
        let a = json!({"file_path": "ignored.rs", "cmd": "ignored-cmd"});
        let hints = build_recent_arg_hints([("read_file", &a)]);
        assert!(hints.is_empty());
    }

    #[test]
    fn render_block_contains_all_sections() {
        let a1 = json!({"path": "a.rs"});
        let a2 = json!({"command": "ls"});
        let calls = vec![("read_file", &a1), ("bash", &a2)];
        let hints = build_recent_arg_hints(calls);
        assert!(text_contains_paths_and_commands(&hints));
    }

    fn text_contains_paths_and_commands(hints: &RecentArgHints) -> bool {
        let text = hints.render_prompt_block().expect("non-empty");
        text.contains("paths:")
            && text.contains("- a.rs")
            && text.contains("commands:")
            && text.contains("- ls")
    }

    #[test]
    fn render_block_skips_empty_sections() {
        let a = json!({"path": "only.rs"});
        let hints = build_recent_arg_hints([("read_file", &a)]);
        let text = hints.render_prompt_block().expect("non-empty");
        assert!(text.contains("paths:"));
        assert!(!text.contains("commands:"));
    }

    #[test]
    fn early_break_when_both_caps_reached() {
        // Construct enough entries to fill both caps, then add extras; extras
        // must not influence the final lists.
        let mut values = Vec::new();
        for i in 0..MAX_RECENT_PATHS {
            values.push(json!({"path": format!("p{i}.rs")}));
        }
        for i in 0..MAX_RECENT_COMMANDS {
            values.push(json!({"command": format!("c{i}")}));
        }
        values.push(json!({"path": "EXTRA.rs", "command": "EXTRA"}));
        let calls: Vec<(&str, &Value)> = values.iter().map(|v| ("tool", v)).collect();
        let hints = build_recent_arg_hints(calls);
        assert!(hints.paths.iter().all(|p| p != "EXTRA.rs"));
        assert!(hints.commands.iter().all(|c| c != "EXTRA"));
    }

    #[test]
    fn extract_from_messages_parses_string_arguments() {
        // OpenAI schema: arguments is a JSON-encoded string.
        let messages = vec![json!({
            "role": "assistant",
            "tool_calls": [{
                "function": {
                    "name": "read_file",
                    "arguments": "{\"path\":\"x.rs\"}"
                }
            }]
        })];
        let calls = extract_recent_tool_calls_from_messages(&messages);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(calls[0].1, json!({"path": "x.rs"}));
    }

    #[test]
    fn extract_from_messages_parses_object_arguments() {
        let messages = vec![json!({
            "role": "assistant",
            "tool_calls": [{
                "function": {
                    "name": "bash",
                    "arguments": {"command": "ls"}
                }
            }]
        })];
        let calls = extract_recent_tool_calls_from_messages(&messages);
        assert_eq!(calls[0].1, json!({"command": "ls"}));
    }

    #[test]
    fn extract_from_messages_returns_mru_order() {
        let messages = vec![
            json!({"role": "assistant", "tool_calls": [{"function": {"name": "read_file", "arguments": "{\"path\":\"older.rs\"}"}}]}),
            json!({"role": "user", "content": "tell me more"}),
            json!({"role": "assistant", "tool_calls": [{"function": {"name": "read_file", "arguments": "{\"path\":\"newer.rs\"}"}}]}),
        ];
        let calls = extract_recent_tool_calls_from_messages(&messages);
        // MRU first.
        assert_eq!(calls[0].1, json!({"path": "newer.rs"}));
        assert_eq!(calls[1].1, json!({"path": "older.rs"}));
    }

    #[test]
    fn prompt_block_from_messages_wires_end_to_end() {
        let messages = vec![json!({
            "role": "assistant",
            "tool_calls": [
                {"function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}},
                {"function": {"name": "bash", "arguments": "{\"command\":\"cargo test\"}"}}
            ]
        })];
        let text = prompt_block_from_messages(&messages).expect("should produce block");
        assert!(text.contains("- a.rs"));
        assert!(text.contains("- cargo test"));
    }

    #[test]
    fn prompt_block_from_messages_empty_returns_none() {
        assert!(prompt_block_from_messages(&[]).is_none());
        let no_calls = vec![json!({"role": "user", "content": "hi"})];
        assert!(prompt_block_from_messages(&no_calls).is_none());
    }
}
