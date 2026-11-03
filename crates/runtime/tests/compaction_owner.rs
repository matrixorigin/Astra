//! Production compaction invariants shared by pre-turn, retry and request paths.
use astra_runtime::turn::{CompactionEngine, TokenBudget};
use astra_turn_types::{
    RuntimeAuthorityLifetime, mark_append_only_required_context,
    render_append_only_runtime_authority_frame,
};
use serde_json::json;

fn budget() -> TokenBudget {
    TokenBudget {
        max_prompt_tokens: 1,
        last_measured_tokens: 100_000,
        current_round_index: Some(99),
        now_secs: 10_000,
    }
}

#[test]
fn ordered_compaction_preserves_active_controls_and_retry_dependencies() {
    let mut messages = vec![
        json!({"role":"system", "content":"stable contract"}),
        json!({"role":"user", "content":"initial goal"}),
        json!({"role":"assistant", "content":"old answer".repeat(200)}),
        json!({"role":"user", "content":"current human goal"}),
    ];
    let mut authority = json!({"role":"user", "content":
        render_append_only_runtime_authority_frame(
            "active_work_attempt_start", RuntimeAuthorityLifetime::CurrentUserTurn,
            "The current Work attempt owns this ordered execution.",
        ).unwrap()
    });
    mark_append_only_required_context(
        &mut authority,
        "active_work_attempt_start",
        RuntimeAuthorityLifetime::CurrentUserTurn,
    );
    messages.push(authority.clone());
    for index in 0..8 {
        messages.push(
            json!({"role":"assistant", "content":null, "_round_index":99,
            "tool_calls":[{"id":format!("active-{index}"), "type":"function",
                "function":{"name":"read_file", "arguments":"{}"}}]}),
        );
        messages.push(
            json!({"role":"tool", "tool_call_id":format!("active-{index}"),
            "content":format!("current evidence {index}"), "_round_index":99, "_timestamp":1}),
        );
    }
    let partial = json!({"role":"assistant", "content":"unfinished response"});
    let mut retry = json!({"role":"user", "content":
        render_append_only_runtime_authority_frame(
            "output_cap_continuation", RuntimeAuthorityLifetime::NextAssistantDecision,
            "Continue the unfinished response.",
        ).unwrap()
    });
    mark_append_only_required_context(
        &mut retry,
        "output_cap_continuation",
        RuntimeAuthorityLifetime::NextAssistantDecision,
    );
    messages.extend([partial.clone(), retry.clone()]);
    let outcome =
        CompactionEngine::aggressive_pipeline().compress_if_needed(&mut messages, &budget());
    assert!(outcome.total_tokens_freed > 0);
    assert!(!outcome.budget_satisfied);
    assert!(
        !messages
            .iter()
            .any(|message| message["content"] == "old answer".repeat(200))
    );
    let anchor = messages
        .iter()
        .position(|message| message["content"] == "current human goal")
        .expect("current human anchor must survive");
    let work = messages
        .iter()
        .position(|message| message == &authority)
        .unwrap();
    let partial_index = messages
        .iter()
        .position(|message| message == &partial)
        .unwrap();
    assert!(anchor < work && work < partial_index);
    assert_eq!(&messages[partial_index..], &[partial, retry]);
    assert!(
        messages
            .iter()
            .filter(|message| message["tool_call_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("active-")))
            .count()
            < 8,
        "older tool rounds must be compactable"
    );
    astra_turn_types::validate_canonical_tool_pairing(&messages).unwrap();
}

#[test]
fn pressure_without_a_useful_rewrite_preserves_tool_call_extension_fields() {
    let mut messages = vec![
        json!({"role":"system", "content":"stable contract"}),
        json!({"role":"user", "content":"inspect"}),
        json!({"role":"assistant", "content":null,
            "tool_calls":[{"id":"c1", "type":"function", "provider_extension":{"opaque":"keep"},
                "function":{"name":"read_file", "arguments":"{}", "provider_hint":"keep"}}]}),
        json!({"role":"tool", "tool_call_id":"c1", "content":"small result"}),
    ];
    let original = messages.clone();
    let outcome =
        CompactionEngine::default_pipeline_for(64_000).compress_if_needed(&mut messages, &budget());
    assert_eq!(outcome.total_tokens_freed, 0);
    assert!(!outcome.budget_satisfied);
    assert_eq!(
        messages, original,
        "mechanical no-op must preserve exact canonical input"
    );
}

#[test]
fn old_multimodal_tool_result_keeps_non_text_blocks_and_array_shape() {
    let image = json!({"type":"image_url", "image_url":{"url":"data:image/png;base64,AA=="}});
    let mut messages = vec![
        json!({"role":"system", "content":"stable contract"}),
        json!({"role":"user", "content":"inspect"}),
        json!({"role":"assistant", "content":null,
            "tool_calls":[{"id":"c1", "type":"function", "function":{"name":"read_file", "arguments":"{}"}}]}),
        json!({"role":"tool", "tool_call_id":"c1", "_round_index":0, "_timestamp":1,
            "content":[{"type":"text", "text":"old text ".repeat(2000)}, image.clone()]}),
    ];
    let outcome =
        CompactionEngine::default_pipeline_for(64_000).compress_if_needed(&mut messages, &budget());
    assert!(outcome.total_tokens_freed > 0);
    let result = messages
        .iter()
        .find(|message| message["tool_call_id"] == "c1")
        .unwrap();
    let blocks = result["content"]
        .as_array()
        .expect("tool content must remain structured blocks");
    let non_text: Vec<_> = blocks
        .iter()
        .filter(|block| block["type"] != "text")
        .cloned()
        .collect();
    assert_eq!(
        non_text,
        vec![image],
        "non-text evidence must remain unchanged and ordered"
    );
    assert!(
        blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .map(str::len)
            .sum::<usize>()
            < 18_000
    );
}

#[test]
fn tool_result_user_frames_stay_in_their_real_human_round() {
    let messages = vec![
        json!({"role":"user", "content":"first human goal"}),
        json!({"role":"assistant", "content":[{"type":"tool_use", "id":"a", "name":"read_file", "input":{}}]}),
        json!({"role":"user", "content":[{"type":"tool_result", "tool_use_id":"a", "content":"evidence"}]}),
        json!({"role":"assistant", "content":"answer"}),
        json!({"role":"user", "content":"next human goal"}),
        json!({"role":"assistant", "content":"next answer"}),
    ];
    let (system, rounds) = astra_turn_core::cloud::grouping::group_by_api_round(&messages);
    assert_eq!(rounds.len(), 2);
    assert_eq!(rounds[0].user_messages().count(), 1);
    assert_eq!(rounds[0].messages(), &messages[..4]);
    assert_eq!(
        astra_turn_core::cloud::grouping::flatten_rounds(&system, &rounds),
        messages
    );
}
