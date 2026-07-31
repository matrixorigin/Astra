//! Message grouping by API round.
//!
//! Groups conversation messages into logical "rounds" — one group per LLM
//! response (identified by the assistant message that ends each round).
//! This enables PTL (prompt-too-long) retry to drop complete rounds
//! atomically, preserving tool_use/tool_result pairing integrity.

use serde_json::Value;

fn include_grouped_clone_measurement(measurement: &mut Option<(u64, u64)>, value: &Value) {
    let Some((bytes, rows)) = measurement.as_mut() else {
        return;
    };
    match astra_core::history_work::serialized_bytes(value) {
        Ok(value_bytes) => {
            *bytes = bytes.saturating_add(value_bytes);
            *rows = rows.saturating_add(1);
        }
        Err(error) => {
            astra_core::history_work::record_serialization_failure(
                astra_core::history_work::HistoryWorkSite::CloudHistoryGroupingClone,
                &error,
            );
            *measurement = None;
        }
    }
}

/// A single API round: the user message(s) that triggered it, the assistant
/// response, and any tool messages that followed.
#[derive(Debug, Clone)]
pub struct ApiRound {
    /// User messages that started this round (may include system injections).
    pub user_messages: Vec<Value>,
    /// The assistant response message.
    pub assistant_message: Option<Value>,
    /// Tool result messages following the assistant response.
    pub tool_messages: Vec<Value>,
}

impl ApiRound {
    /// Flattened ordered messages for this round.
    pub fn messages(&self) -> Vec<Value> {
        let mut out = self.user_messages.clone();
        if let Some(asst) = &self.assistant_message {
            out.push(asst.clone());
        }
        out.extend(self.tool_messages.iter().cloned());
        out
    }

    /// Total character count for this round (for budget estimation).
    pub fn char_count(&self) -> usize {
        self.messages()
            .iter()
            .map(|m| {
                m.get("content")
                    .and_then(Value::as_str)
                    .map(|s| s.chars().count())
                    .unwrap_or(0)
            })
            .sum()
    }
}

/// Group a flat message list into API rounds.
///
/// Each round starts with user message(s) and ends when the next user
/// message (or end of list) is encountered. System messages are collected
/// as a leading preamble and returned separately.
///
/// Returns `(system_messages, rounds)`.
pub fn group_by_api_round(messages: &[Value]) -> (Vec<Value>, Vec<ApiRound>) {
    let mut system_messages = Vec::new();
    let mut rounds: Vec<ApiRound> = Vec::new();
    let mut current_round: Option<ApiRound> = None;
    let mut clone_measurement =
        astra_core::history_work::instrumentation_enabled().then_some((0_u64, 0_u64));

    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");

        match role {
            "system" if current_round.is_none() => {
                let cloned = msg.clone();
                include_grouped_clone_measurement(&mut clone_measurement, &cloned);
                system_messages.push(cloned);
                // system messages mid-conversation are treated as user-side injections
            }
            "system" => {
                // system messages mid-conversation are treated as user-side injections
            }
            "user" => {
                // A new user message starts a new round (flush the current one)
                if let Some(round) = current_round.take() {
                    rounds.push(round);
                }
                current_round
                    .get_or_insert_with(|| ApiRound {
                        user_messages: Vec::new(),
                        assistant_message: None,
                        tool_messages: Vec::new(),
                    })
                    .user_messages
                    .push({
                        let cloned = msg.clone();
                        include_grouped_clone_measurement(&mut clone_measurement, &cloned);
                        cloned
                    });
            }
            "assistant" => {
                let sanitized =
                    crate::chat_history_openai::sanitize_empty_assistant_tool_calls_cloned(msg);
                include_grouped_clone_measurement(&mut clone_measurement, &sanitized);
                if let Some(round) = current_round.as_mut() {
                    round.assistant_message = Some(sanitized);
                } else {
                    // assistant without a preceding user (shouldn't happen, but handle it)
                    current_round = Some(ApiRound {
                        user_messages: Vec::new(),
                        assistant_message: Some(sanitized),
                        tool_messages: Vec::new(),
                    });
                }
            }
            "tool" => {
                if let Some(round) = current_round.as_mut() {
                    let cloned = msg.clone();
                    include_grouped_clone_measurement(&mut clone_measurement, &cloned);
                    round.tool_messages.push(cloned);
                }
                // tool without a round context is ignored
            }
            _ => {}
        }
    }

    // Flush the last in-progress round
    if let Some(round) = current_round {
        rounds.push(round);
    }
    if let Some((bytes, rows)) = clone_measurement {
        astra_core::history_work::record_operation(
            astra_core::history_work::HistoryWorkSite::CloudHistoryGroupingClone,
            bytes,
            rows,
            0,
        );
    }

    (system_messages, rounds)
}

/// Flatten grouped rounds back into a message list, optionally including
/// system messages at the front.
pub fn flatten_rounds(system_messages: &[Value], rounds: &[ApiRound]) -> Vec<Value> {
    let mut out = system_messages.to_vec();
    for round in rounds {
        out.extend(round.messages());
    }
    out
}

/// Drop the N oldest complete rounds from a grouped message list.
///
/// Returns the updated rounds slice (preserves the most recent rounds).
/// Leaves at least `min_keep` rounds even if `drop_n` would exceed the total.
pub fn drop_oldest_rounds(rounds: &[ApiRound], drop_n: usize, min_keep: usize) -> &[ApiRound] {
    let max_drop = rounds.len().saturating_sub(min_keep);
    let actual_drop = drop_n.min(max_drop);
    &rounds[actual_drop..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(c: &str) -> Value {
        json!({"role": "user", "content": c})
    }
    fn assistant(c: &str) -> Value {
        json!({"role": "assistant", "content": c})
    }
    fn tool(c: &str) -> Value {
        json!({"role": "tool", "content": c})
    }
    fn system(c: &str) -> Value {
        json!({"role": "system", "content": c})
    }

    #[test]
    fn empty_messages_yields_empty_rounds() {
        let (sys, rounds) = group_by_api_round(&[]);
        assert!(sys.is_empty());
        assert!(rounds.is_empty());
    }

    #[test]
    fn system_messages_extracted_as_preamble() {
        let msgs = vec![system("you are helpful"), user("hello"), assistant("hi")];
        let (sys, rounds) = group_by_api_round(&msgs);
        assert_eq!(sys.len(), 1);
        assert_eq!(rounds.len(), 1);
    }

    #[test]
    fn single_round_user_assistant_tool() {
        let msgs = vec![user("q"), assistant("a"), tool("result")];
        let (_, rounds) = group_by_api_round(&msgs);
        assert_eq!(rounds.len(), 1);
        let r = &rounds[0];
        assert_eq!(r.user_messages.len(), 1);
        assert!(r.assistant_message.is_some());
        assert_eq!(r.tool_messages.len(), 1);
    }

    #[test]
    fn multi_round_grouping() {
        let msgs = vec![
            user("q1"),
            assistant("a1"),
            tool("r1"),
            user("q2"),
            assistant("a2"),
        ];
        let (_, rounds) = group_by_api_round(&msgs);
        assert_eq!(rounds.len(), 2);
        assert_eq!(rounds[0].tool_messages.len(), 1);
        assert!(rounds[1].tool_messages.is_empty());
    }

    #[test]
    fn flatten_round_trip() {
        let msgs = vec![
            system("sys"),
            user("q1"),
            assistant("a1"),
            tool("r1"),
            user("q2"),
            assistant("a2"),
        ];
        let (sys, rounds) = group_by_api_round(&msgs);
        let flat = flatten_rounds(&sys, &rounds);
        assert_eq!(flat.len(), msgs.len());
        for (original, restored) in msgs.iter().zip(flat.iter()) {
            assert_eq!(
                original.get("content").unwrap().as_str(),
                restored.get("content").unwrap().as_str()
            );
        }
    }

    #[test]
    fn grouping_omits_empty_assistant_tool_calls() {
        let msgs = vec![
            user("q"),
            json!({"role": "assistant", "content": "a", "tool_calls": []}),
        ];
        let (_, rounds) = group_by_api_round(&msgs);
        let assistant = rounds[0].assistant_message.as_ref().unwrap();
        assert!(assistant.get("tool_calls").is_none(), "{assistant:?}");
    }

    #[test]
    fn grouping_owns_independent_nested_unicode_history() {
        let mut messages = vec![
            system("系统🙂"),
            json!({
                "role": "user",
                "content": [{"type": "text", "text": "问题🚀"}],
                "metadata": {"nested": ["甲", {"answer": "乙"}]}
            }),
            json!({
                "role": "assistant",
                "content": "réponse",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"key\":\"值\"}"}
                }]
            }),
        ];

        let (system_messages, rounds) = group_by_api_round(&messages);
        messages[0]["content"] = json!("mutated");
        messages[1]["metadata"]["nested"][1]["answer"] = json!("mutated");
        messages[2]["tool_calls"][0]["function"]["name"] = json!("mutated");

        assert_eq!(system_messages[0]["content"], "系统🙂");
        assert_eq!(
            rounds[0].user_messages[0]["metadata"]["nested"][1]["answer"],
            "乙"
        );
        assert_eq!(
            rounds[0].assistant_message.as_ref().unwrap()["tool_calls"][0]["function"]["name"],
            "lookup"
        );
    }

    #[test]
    fn drop_oldest_rounds_respects_min_keep() {
        let msgs = vec![
            user("q1"),
            assistant("a1"),
            user("q2"),
            assistant("a2"),
            user("q3"),
            assistant("a3"),
        ];
        let (_, rounds) = group_by_api_round(&msgs);
        assert_eq!(rounds.len(), 3);

        // Drop 2, keep at least 2 → only drop 1
        let kept = drop_oldest_rounds(&rounds, 2, 2);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn drop_oldest_rounds_normal_case() {
        let msgs = vec![
            user("q1"),
            assistant("a1"),
            user("q2"),
            assistant("a2"),
            user("q3"),
            assistant("a3"),
        ];
        let (_, rounds) = group_by_api_round(&msgs);
        let kept = drop_oldest_rounds(&rounds, 1, 1);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].user_messages[0]["content"].as_str().unwrap(), "q2");
    }

    #[test]
    fn round_char_count() {
        let msgs = vec![
            user("hello"),   // 5 chars
            assistant("hi"), // 2 chars
            tool("result"),  // 6 chars
        ];
        let (_, rounds) = group_by_api_round(&msgs);
        assert_eq!(rounds[0].char_count(), 13);
    }
}
