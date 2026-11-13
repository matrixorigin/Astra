use serde_json::{Map, Value};

/// Returns whether a message structurally invokes the memory tool.
///
/// Observer input is a knowledge-extraction surface, not a replay of the
/// agent's control transcript.  Memory operations are already persisted by
/// the memory tool itself; feeding their tool call/result/confirmation back
/// into the observer creates active semantic echoes.  Keep this predicate
/// deliberately structural: never classify ordinary assistant prose by
/// matching words such as "stored" or "deleted".
pub fn is_memory_tool_message(message: &Value) -> bool {
    let role = message.get("role").and_then(Value::as_str);

    if role == Some("tool") && message.get("name").and_then(Value::as_str) == Some("memory") {
        return true;
    }

    if role == Some("assistant")
        && let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array)
        && tool_calls.iter().any(|tool_call| {
            tool_call
                .get("function")
                .and_then(Value::as_object)
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                == Some("memory")
        })
    {
        return true;
    }

    role == Some("assistant")
        && message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && block.get("name").and_then(Value::as_str) == Some("memory")
                })
            })
}

/// Remove complete user-turn segments containing a structural memory
/// tool-use from the knowledge-extraction history.
///
/// A segment is bounded by real user messages; Anthropic `tool_result` user
/// messages do not open a new segment.  Dropping the whole segment prevents
/// the operation request, its tool result, and the assistant confirmation
/// from being re-learned as independent memories.  Other turns remain
/// untouched, including ordinary assistant text and non-memory knowledge.
pub fn filter_memory_operation_turns(messages: &[Value]) -> Vec<Value> {
    filter_memory_operation_turns_after(messages, false).0
}

/// Carry only whether the preceding unfinished human-delimited segment was a
/// memory operation. This preserves filtering across durable observation slices
/// without retaining or rereading the preceding transcript.
pub fn filter_memory_operation_turns_after(
    messages: &[Value],
    preceding_memory_operation: bool,
) -> (Vec<Value>, bool) {
    if messages.is_empty() {
        return (Vec::new(), preceding_memory_operation);
    }

    let mut filtered = Vec::with_capacity(messages.len());
    let mut segment_start = 0usize;
    let mut memory_operation = preceding_memory_operation;
    let append_observer_safe_segment = |filtered: &mut Vec<Value>, segment: &[Value]| {
        filtered.extend(
            segment
                .iter()
                .filter(|message| !astra_turn_types::is_runtime_owned_message(message))
                .map(astra_core::model_wire::responses::without_continuation),
        );
    };

    for (index, message) in messages.iter().enumerate() {
        if astra_turn_types::is_human_user_message(message) {
            let segment = &messages[segment_start..index];
            if !memory_operation && !segment.iter().any(is_memory_tool_message) {
                append_observer_safe_segment(&mut filtered, segment);
            }
            segment_start = index;
            memory_operation = false;
        }
    }

    let segment = &messages[segment_start..];
    memory_operation |= segment.iter().any(is_memory_tool_message);
    if !memory_operation {
        append_observer_safe_segment(&mut filtered, segment);
    }

    (filtered, memory_operation)
}

pub fn should_run_observer(full_text: &str, has_tool_calls: bool) -> bool {
    !full_text.is_empty() && !has_tool_calls
}

/// Bounded prior context is reference material, never an independently due
/// observation. Preserve the checkpoint's filter state for new content, and
/// exclude a prior human segment if its memory-tool call arrives in the new
/// suffix before the next human boundary.
pub fn filter_memory_operation_parts(
    context: &[Value],
    messages: &[Value],
    preceding_memory_operation: bool,
) -> (Vec<Value>, Vec<Value>, bool) {
    let (new, flag) = filter_memory_operation_turns_after(messages, preceding_memory_operation);
    if new.is_empty() {
        return (Vec::new(), new, flag);
    }
    let continuation_is_memory_operation = messages
        .iter()
        .take_while(|message| !astra_turn_types::is_human_user_message(message))
        .any(is_memory_tool_message);
    let context_end = if continuation_is_memory_operation {
        context
            .iter()
            .rposition(astra_turn_types::is_human_user_message)
            .unwrap_or(0)
    } else {
        context.len()
    };
    let context = filter_memory_operation_turns_after(&context[..context_end], true).0;
    (context, new, flag)
}

pub fn build_observer_messages(
    user_content: Option<&str>,
    full_text: &str,
) -> Vec<Map<String, Value>> {
    let mut messages = Vec::new();
    if let Some(user_content) = user_content.filter(|content| !content.is_empty()) {
        messages.push(Map::from_iter([
            ("role".to_string(), Value::from("user")),
            ("content".to_string(), Value::from(user_content)),
        ]));
    }
    messages.push(Map::from_iter([
        ("role".to_string(), Value::from("assistant")),
        ("content".to_string(), Value::from(full_text)),
    ]));
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_context_resolves_antecedent_without_relearning_memory_segment() {
        use serde_json::json;
        let context = vec![
            json!({"role":"user","content":"My cat is Pixel."}),
            json!({"role":"assistant","content":"Noted."}),
            json!({"role":"user","content":"Remember my address."}),
            json!({"role":"assistant","content":"Checking."}),
        ];
        let messages = vec![
            json!({"role":"assistant","tool_calls":[{"function":{"name":"memory"}}]}),
            json!({"role":"tool","name":"memory","content":"saved"}),
            json!({"role":"assistant","content":"Saved your address."}),
            json!({"role":"user","content":"He had surgery."}),
        ];
        let (reference, new, flag) = filter_memory_operation_parts(&context, &messages, false);
        assert_eq!(reference, context[..2]);
        assert_eq!(new, messages[3..]);
        assert!(!flag);
        let (reference, new, flag) = filter_memory_operation_parts(&context, &messages[..3], false);
        assert!(reference.is_empty());
        assert!(new.is_empty());
        assert!(flag);
        let (reference, new, flag) = filter_memory_operation_parts(&[], &context[3..], false);
        assert!(reference.is_empty());
        assert_eq!(new, context[3..]);
        assert!(!flag);
        let (reference, new, flag) = filter_memory_operation_parts(&context, &context[3..], true);
        assert!(reference.is_empty());
        assert!(new.is_empty());
        assert!(flag);
        // Reference text must never become newly due, including when no memory
        // operation is present. Durable cursor offsets address only `new`.
        let (reference, new, flag) =
            filter_memory_operation_parts(&context[..2], &messages[3..], false);
        assert_eq!(reference, context[..2]);
        assert_eq!(new, messages[3..]);
        assert!(!flag);
        let (reference, new, flag) = filter_memory_operation_parts(&context[..2], &[], false);
        assert!(reference.is_empty());
        assert!(new.is_empty());
        assert!(!flag);
    }

    #[test]
    fn should_run_text_no_calls() {
        assert!(should_run_observer("hello", false));
    }

    #[test]
    fn should_run_empty_text() {
        assert!(!should_run_observer("", false));
    }

    #[test]
    fn should_run_has_tool_calls() {
        assert!(!should_run_observer("hello", true));
    }

    #[test]
    fn observer_messages_with_user() {
        let msgs = build_observer_messages(Some("question"), "answer");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"].as_str().unwrap(), "user");
        assert_eq!(msgs[1]["role"].as_str().unwrap(), "assistant");
    }

    #[test]
    fn observer_messages_no_user() {
        let msgs = build_observer_messages(None, "answer");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"].as_str().unwrap(), "assistant");
    }

    #[test]
    fn observer_messages_empty_user_content() {
        let msgs = build_observer_messages(Some(""), "answer");
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn observer_excludes_private_provider_state_without_changing_history() {
        let messages = vec![
            serde_json::json!({"role":"user", "content":"inspect"}),
            serde_json::json!({"role":"assistant", "content":"done", "_astra_responses":{
                "items":[{"type":"reasoning", "content":[{"type":"reasoning_text", "text":"private"}]}]
            }}),
        ];
        let filtered = filter_memory_operation_turns(&messages);
        assert_eq!(
            filtered,
            vec![
                messages[0].clone(),
                serde_json::json!({"role":"assistant", "content":"done"})
            ]
        );
        assert!(messages[1].get("_astra_responses").is_some());
    }

    #[test]
    fn memory_operation_turn_is_excluded_without_matching_confirmation_prose() {
        let messages = vec![
            serde_json::json!({"role":"user","content":"Remember the launch date."}),
            serde_json::json!({
                "role":"assistant",
                "tool_calls":[{"function":{"name":"memory","arguments":"{\"action\":\"remember\"}"}}]
            }),
            serde_json::json!({"role":"tool","name":"memory","content":"{\"memory_id\":\"m1\"}"}),
            serde_json::json!({"role":"assistant","content":"Stored. Memory m1 is active."}),
            serde_json::json!({"role":"user","content":"Rust uses ownership."}),
            serde_json::json!({"role":"assistant","content":"Ownership prevents data races."}),
        ];

        let filtered = filter_memory_operation_turns(&messages);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0]["content"], "Rust uses ownership.");
        assert_eq!(filtered[1]["content"], "Ownership prevents data races.");
    }

    #[test]
    fn memory_operation_marker_survives_sliced_confirmations() {
        let messages = vec![
            serde_json::json!({"role":"user","content":"Remember this."}),
            serde_json::json!({"role":"assistant","tool_calls":[{"function":{"name":"memory","arguments":"{}"}}]}),
            serde_json::json!({"role":"tool","name":"memory","content":"stored"}),
            serde_json::json!({"role":"assistant","content":"Stored confirmation."}),
            serde_json::json!({"role":"user","content":"An independent fact."}),
            serde_json::json!({"role":"assistant","content":"Acknowledged."}),
        ];
        for cut in 2..=messages.len() {
            let (mut first, state) = filter_memory_operation_turns_after(&messages[..cut], false);
            assert_eq!(
                filter_memory_operation_turns_after(&[], state),
                (Vec::new(), state)
            );
            let (second, _) = filter_memory_operation_turns_after(&messages[cut..], state);
            first.extend(second);
            assert_eq!(first, filter_memory_operation_turns(&messages));
        }
        assert!(
            filter_memory_operation_turns_after(&[messages[3].clone()], true)
                .0
                .is_empty()
        );
        assert_eq!(
            filter_memory_operation_turns_after(&messages[4..], true).0,
            messages[4..]
        );
    }

    #[test]
    fn recall_and_anthropic_memory_tool_use_are_excluded_fail_closed() {
        let messages = vec![
            serde_json::json!({"role":"user","content":"Recall the project date."}),
            serde_json::json!({
                "role":"assistant",
                "content":[{"type":"tool_use","name":"memory","input":{"action":"recall"}}]
            }),
            serde_json::json!({
                "role":"user",
                "content":[{"type":"tool_result","tool_use_id":"m1","content":"..."}]
            }),
            serde_json::json!({"role":"assistant","content":"The project date is June 15th."}),
            serde_json::json!({"role":"user","content":"The repository is Rust."}),
        ];

        let filtered = filter_memory_operation_turns(&messages);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["content"], "The repository is Rust.");
    }

    #[test]
    fn ordinary_confirmation_prose_without_memory_tool_use_is_preserved() {
        let messages = vec![
            serde_json::json!({"role":"user","content":"Tell me how this works."}),
            serde_json::json!({"role":"assistant","content":"Stored as an example in the explanation."}),
        ];

        let filtered = filter_memory_operation_turns(&messages);
        assert_eq!(filtered, messages);
    }

    #[test]
    fn runtime_owned_user_role_messages_never_reach_the_observer() {
        let mut authority = serde_json::json!({
            "role": "user",
            "content": "runtime settlement"
        });
        astra_turn_types::mark_append_only_required_context(
            &mut authority,
            "final_answer_settlement",
            astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
        );
        let messages = vec![
            serde_json::json!({"role":"user","content":"real task"}),
            authority,
            serde_json::json!({"role":"assistant","content":"done"}),
        ];

        let filtered = filter_memory_operation_turns(&messages);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0]["content"], "real task");
        assert_eq!(filtered[1]["content"], "done");
    }

    #[test]
    fn memory_name_without_a_tool_message_shape_is_not_filtered() {
        let messages = vec![
            serde_json::json!({"role":"user","content":"Explain memory safety."}),
            serde_json::json!({
                "role":"assistant",
                "name":"memory",
                "content":"Memory safety prevents data races."
            }),
        ];

        assert_eq!(filter_memory_operation_turns(&messages), messages);
    }
}
