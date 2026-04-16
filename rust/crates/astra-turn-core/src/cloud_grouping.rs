//! Message grouping by API round.
//!
//! Groups conversation messages into logical "rounds" — one group per LLM
//! response (identified by the assistant message that ends each round).
//! This enables PTL (prompt-too-long) retry to drop complete rounds
//! atomically, preserving tool_use/tool_result pairing integrity.

use serde_json::Value;

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

    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");

        match role {
            "system" if current_round.is_none() => {
                system_messages.push(msg.clone());
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
                    .push(msg.clone());
            }
            "assistant" => {
                if let Some(round) = current_round.as_mut() {
                    round.assistant_message = Some(msg.clone());
                } else {
                    // assistant without a preceding user (shouldn't happen, but handle it)
                    current_round = Some(ApiRound {
                        user_messages: Vec::new(),
                        assistant_message: Some(msg.clone()),
                        tool_messages: Vec::new(),
                    });
                }
            }
            "tool" => {
                if let Some(round) = current_round.as_mut() {
                    round.tool_messages.push(msg.clone());
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
