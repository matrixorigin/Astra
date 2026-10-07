// ── Constants ────────────────────────────────────────────────────────────────

pub const CROSS_SESSION_MEMORY_RETRIEVE_TOP_K: u64 = 5;

// ── Cross-session memory admission ──────────────────────────────────────────

/// Structured reason why cross-session memory retrieval did not run.
///
/// This deliberately models routing state, not prose. Infrastructure should not
/// decide memory retrieval from natural-language substrings in the user message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossSessionMemorySkipReason {
    /// No planner/tool/runtime component supplied an explicit semantic query.
    NoStructuredIntent,
    /// A structured query was supplied but normalized to empty.
    EmptyStructuredQuery,
    /// The current conversation already carries memory context, so automatic
    /// cross-session recall would add duplicate cache-volatile context.
    ConversationAlreadyHasMemoryContext,
}

/// Admission decision for cross-session memory retrieval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossSessionMemoryDecision<'a> {
    Retrieve {
        query: &'a str,
        top_k: u64,
    },
    Skip {
        query: &'a str,
        reason: CrossSessionMemorySkipReason,
    },
}

impl<'a> CrossSessionMemoryDecision<'a> {
    pub fn query(self) -> &'a str {
        match self {
            Self::Retrieve { query, .. } | Self::Skip { query, .. } => query,
        }
    }
}

/// Decide whether to perform cross-session memory retrieval for a turn.
///
/// First-principles contract:
/// - user text is evidence, not infrastructure control flow;
/// - cross-session retrieval requires structured intent;
/// - prompt-cache stability wins when the current conversation already carries
///   memory context.
pub fn decide_cross_session_memory_retrieval<'a>(
    message: &'a str,
    semantic_query_override: Option<&'a str>,
    has_conversation_memory_context: bool,
) -> CrossSessionMemoryDecision<'a> {
    let Some(query) = semantic_query_override.map(str::trim) else {
        return CrossSessionMemoryDecision::Skip {
            query: message,
            reason: CrossSessionMemorySkipReason::NoStructuredIntent,
        };
    };

    if query.is_empty() {
        return CrossSessionMemoryDecision::Skip {
            query,
            reason: CrossSessionMemorySkipReason::EmptyStructuredQuery,
        };
    }

    if has_conversation_memory_context {
        return CrossSessionMemoryDecision::Skip {
            query,
            reason: CrossSessionMemorySkipReason::ConversationAlreadyHasMemoryContext,
        };
    }

    CrossSessionMemoryDecision::Retrieve {
        query,
        top_k: CROSS_SESSION_MEMORY_RETRIEVE_TOP_K,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_session_memory_requires_structured_intent_not_user_text_shape() {
        for input in [
            "hi",
            "继续",
            "ok",
            "analyze session 62f1e532-f4c3-4953-b1dc-c427acd63b83",
            "继续之前那个分支的修复",
        ] {
            assert_eq!(
                decide_cross_session_memory_retrieval(input, None, false),
                CrossSessionMemoryDecision::Skip {
                    query: input,
                    reason: CrossSessionMemorySkipReason::NoStructuredIntent,
                }
            );
        }
    }

    #[test]
    fn cross_session_memory_uses_normalized_structured_query() {
        assert_eq!(
            decide_cross_session_memory_retrieval(
                "fallback user text",
                Some("  previous branch  "),
                false
            ),
            CrossSessionMemoryDecision::Retrieve {
                query: "previous branch",
                top_k: CROSS_SESSION_MEMORY_RETRIEVE_TOP_K,
            }
        );
    }

    #[test]
    fn cross_session_memory_rejects_empty_structured_query() {
        assert_eq!(
            decide_cross_session_memory_retrieval("fallback", Some("  "), false),
            CrossSessionMemoryDecision::Skip {
                query: "",
                reason: CrossSessionMemorySkipReason::EmptyStructuredQuery,
            }
        );
    }

    #[test]
    fn cross_session_memory_skips_when_conversation_already_has_memory_context() {
        assert_eq!(
            decide_cross_session_memory_retrieval("resume", Some("aa1f419b"), true),
            CrossSessionMemoryDecision::Skip {
                query: "aa1f419b",
                reason: CrossSessionMemorySkipReason::ConversationAlreadyHasMemoryContext,
            }
        );
    }
}
