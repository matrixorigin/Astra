//! Memory relevance filtering via a cheap selector model.
//!
//! Filters retrieved memories/lessons to only those clearly relevant
//! to the current task, reducing prompt noise and token waste.
//! Uses the cheapest `selector`-tagged model from the registry
//! (resolved via `resolve_memory_offerings` from the model DB).

use std::collections::HashSet;
use std::time::Duration;

use astra_text_utils::text_tokenize::tokenize;
use astra_turn_types::{
    InferencePurpose, JUDGMENT_SCHEMA_VERSION, JudgmentNoulDecision, JudgmentRequest,
    JudgmentResponse, JudgmentResponseProvenance, judgment_messages, normalize_judgment_response,
};

use super::inference::{MemoryInferencePort, MemoryInferenceRequest, MemoryInferenceResponse};

/// Prompt for the selector model to judge memory relevance.
pub const RELEVANCE_FILTER_PROMPT: &str = "Judge each memory independently by its contribution to the current task. A memory need only supply one requested fact or applicable constraint, not answer the whole task. Keep all such contributions. Respect the requested subject, scope and current instructions; shared words alone are insufficient. Without task context, answer no. Mark uncertainty rather than guess; uncertain candidates are retained after clear matches so optional judgment cannot silently erase potentially useful context.";

/// Business policy for explicitly rejected previously injected memories.
pub const MEMORY_FEEDBACK_FILTER_PROMPT: &str = "Identify candidates the latest user message explicitly rejects as irrelevant, stale, wrong, conflicting, or no longer applicable. A task change alone or a current-task exception to a general lesson is not rejection. Mark uncertainty rather than guess; uncertain candidates are excluded.";

// Business semantics and decision policies stay in the memory owner.
#[derive(Clone, Copy)]
enum MemoryJudgmentKind {
    Relevance,
    ExplicitDismissal,
}
const RELEVANCE_THRESHOLD: f64 = 0.5;
const DISMISSAL_THRESHOLD: f64 = 0.5;

/// Build the user-turn content for relevance filtering.
#[must_use]
pub fn build_relevance_query(user_message: &str, memories: &[String]) -> String {
    serde_json::to_string(&build_memory_judgment(
        MemoryJudgmentKind::Relevance,
        user_message,
        memories,
    ))
    .expect("typed memory judgment")
}

/// Build the user-turn content for memory feedback filtering.
#[must_use]
pub fn build_memory_feedback_query(user_message: &str, memories: &[String]) -> String {
    serde_json::to_string(&build_memory_judgment(
        MemoryJudgmentKind::ExplicitDismissal,
        user_message,
        memories,
    ))
    .expect("typed memory judgment")
}

/// Strict decisions use numeric candidate order for both inference backends.
#[cfg(test)]
fn parse_relevance_response(
    response: &str,
    memory_count: usize,
    provenance: JudgmentResponseProvenance,
) -> Result<Vec<usize>, astra_turn_types::JudgmentCodecError> {
    let request = build_memory_judgment(
        MemoryJudgmentKind::Relevance,
        "task",
        &vec![String::new(); memory_count],
    );
    let response = tests::discrete_fixture(&request, response);
    let normalized =
        normalize_judgment_response(&request, &response, "test-selector", Some(provenance))?;
    Ok(selector_indices(
        &normalized.response,
        memory_count,
        RELEVANCE_THRESHOLD,
        true,
        normalized.provenance,
    ))
}

fn selector_indices(
    response: &JudgmentResponse,
    memory_count: usize,
    threshold: f64,
    retain_uncertain: bool,
    provenance: JudgmentResponseProvenance,
) -> Vec<usize> {
    let mut selected: Vec<usize> = (0..memory_count)
        .filter(|i| {
            let answer = &response.answers[&i.to_string()];
            match provenance {
                JudgmentResponseProvenance::ProviderProbability => answer
                    .native_noul_probability()
                    .is_some_and(|value| value > threshold),
                JudgmentResponseProvenance::DiscreteDecision => {
                    match answer.discrete_noul_decision() {
                        Some(JudgmentNoulDecision::Yes) => true,
                        Some(JudgmentNoulDecision::No | JudgmentNoulDecision::Unknown) | None => {
                            false
                        }
                    }
                }
            }
        })
        .collect();
    if retain_uncertain {
        selected.extend((0..memory_count).filter(|i| {
            let answer = &response.answers[&i.to_string()];
            match provenance {
                JudgmentResponseProvenance::ProviderProbability => answer
                    .native_noul_probability()
                    .is_some_and(|value| value == threshold),
                JudgmentResponseProvenance::DiscreteDecision => {
                    answer.discrete_noul_decision() == Some(JudgmentNoulDecision::Unknown)
                }
            }
        }));
    }
    selected
}

/// Filter memories by the indices returned from the selector model.
/// Returns only the memories at the given indices, preserving order.
#[must_use]
pub fn filter_by_indices<T: Clone>(items: &[T], indices: &[usize]) -> Vec<T> {
    indices
        .iter()
        .filter_map(|&i| items.get(i).cloned())
        .collect()
}

/// Local relevance gate used when the selector model is unavailable and as a
/// deterministic fallback in tests. It only keeps items that share meaningful
/// task terms with the current user message.
#[must_use]
pub fn lexical_filter_memories(user_message: &str, items: &[String]) -> Vec<String> {
    let indices = lexical_relevant_indices(user_message, items);
    filter_by_indices(items, &indices)
}

#[must_use]
pub fn lexical_relevant_indices(user_message: &str, items: &[String]) -> Vec<usize> {
    let query_terms = meaningful_terms(user_message);
    if query_terms.is_empty() {
        return Vec::new();
    }

    let mut scored = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let item_terms = meaningful_terms(item);
        let score = overlap_score(&query_terms, &item_terms);
        if score > 0 {
            scored.push((idx, score));
        }
    }
    scored.sort_by(|(left_idx, left_score), (right_idx, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| left_idx.cmp(right_idx))
    });
    scored.into_iter().map(|(idx, _)| idx).collect()
}

/// Failure fallback for an optional selector. Local evidence can rank bounded
/// candidates, but lack of lexical overlap is not proof of irrelevance. Keep
/// the remaining retrieval-ranked candidates after the evidenced matches so a
/// missing/invalid judge cannot weaken the no-enhancement baseline.
fn lexical_fallback_indices(user_message: &str, items: &[String]) -> Vec<usize> {
    let mut ranked = lexical_relevant_indices(user_message, items);
    let matched = ranked.iter().copied().collect::<HashSet<_>>();
    ranked.extend((0..items.len()).filter(|index| !matched.contains(index)));
    ranked
}

fn meaningful_terms(text: &str) -> HashSet<String> {
    tokenize(text)
        .into_iter()
        .filter(|term| is_meaningful_term(term))
        .collect()
}

fn overlap_score(query_terms: &HashSet<String>, item_terms: &HashSet<String>) -> usize {
    query_terms
        .intersection(item_terms)
        .filter(|term| is_strong_overlap_term(term))
        .map(|term| if term.is_ascii() { 2 } else { 1 })
        .sum()
}

fn is_strong_overlap_term(term: &str) -> bool {
    if term.is_ascii() {
        return term.chars().count() >= 3;
    }
    term.chars().count() >= 2
}

fn is_meaningful_term(term: &str) -> bool {
    if term.trim().is_empty() {
        return false;
    }
    if matches!(
        term,
        "the"
            | "and"
            | "for"
            | "with"
            | "that"
            | "this"
            | "from"
            | "into"
            | "when"
            | "rule"
            | "rules"
            | "general"
            | "always"
            | "never"
            | "should"
            | "would"
            | "could"
            | "don't"
            | "dont"
            | "doesn't"
            | "doesnt"
            | "do"
            | "not"
            | "use"
            | "using"
            | "used"
            | "user"
            | "task"
            | "please"
            | "help"
            | "need"
            | "want"
            | "about"
            | "because"
            | "instead"
            | "prefer"
            | "run"
    ) {
        return false;
    }
    if matches!(
        term,
        "的" | "了"
            | "是"
            | "在"
            | "和"
            | "与"
            | "或"
            | "这"
            | "那"
            | "用"
            | "要"
            | "不"
            | "做"
            | "说"
            | "把"
            | "给"
            | "对"
            | "错"
    ) {
        return false;
    }
    true
}

/// Filter a list of text items through the selector model.
/// Returns only items deemed relevant to `user_message`.
///
/// On transport/model errors, falls back to deterministic lexical ranking
/// while retaining the bounded candidate set.
/// If the selector explicitly returns no relevant indices, returns an empty
/// list. The relevance question must recognize partial task contributions as
/// useful; a missed necessary fact can prevent a correct downstream answer.
pub async fn filter_memories(
    client: &dyn MemoryInferencePort,
    invocation_scope: &astra_turn_types::InferenceInvocationScope,
    user_message: &str,
    items: &[String],
) -> Vec<String> {
    let decision = select_memories(
        Some(client),
        Some(invocation_scope),
        user_message,
        items,
        false,
    )
    .await;
    filter_by_indices(items, &decision.selected_indices())
}

/// One decision result is used both to apply the selection and to explain it.
/// A failed dismissal never guesses which memories the user rejected.
/// Model selections follow numeric candidate order for both inference backends.
pub async fn select_memories(
    client: Option<&dyn MemoryInferencePort>,
    invocation_scope: Option<&astra_turn_types::InferenceInvocationScope>,
    user_message: &str,
    items: &[String],
    dismissal: bool,
) -> astra_turn_types::MemorySelectionReport {
    use astra_turn_types::{
        MemoryCandidateDecision, MemorySelectionMethod as Method,
        MemorySelectionOperation as Operation, MemorySelectionReason as Reason,
        MemorySelectionReport,
    };
    let started = std::time::Instant::now();
    let (session_id, turn) = match invocation_scope {
        Some(astra_turn_types::InferenceInvocationScope::Session {
            session_id, turn, ..
        }) => (session_id.clone(), *turn),
        _ => (String::new(), 0),
    };
    let mut report = MemorySelectionReport {
        session_id,
        turn,
        operation: if dismissal {
            Operation::Dismissal
        } else {
            Operation::Relevance
        },
        method: Method::None,
        selection_order: Vec::new(),
        reason: Reason::NoCandidates,
        model: None,
        candidates: items
            .iter()
            .enumerate()
            .map(|(i, _)| MemoryCandidateDecision {
                index: i as u32,
                selected: false,
                probability_bps: None,
            })
            .collect(),
        elapsed_ms: 0,
        candidate_coverage: None,
        prompt_projection: None,
    };
    if items.is_empty() {
        return report;
    }
    report.reason = Reason::NoSelector;
    if let (Some(client), Some(scope)) = (client, invocation_scope) {
        report.model = Some(client.model_name().to_string());
        let kind = if dismissal {
            MemoryJudgmentKind::ExplicitDismissal
        } else {
            MemoryJudgmentKind::Relevance
        };
        let threshold = if dismissal {
            DISMISSAL_THRESHOLD
        } else {
            RELEVANCE_THRESHOLD
        };
        let judgment = build_memory_judgment(kind, user_message, items);
        match run_selector_prompt(
            client,
            scope,
            InferencePurpose::MemoryRetrievalRerank,
            &judgment,
        )
        .await
        {
            None => report.reason = Reason::CallUnavailable,
            Some(response) => {
                match normalize_judgment_response(
                    &judgment,
                    &response.text,
                    &response.model_used,
                    response.judgment_provenance,
                ) {
                    Err(_) => report.reason = Reason::InvalidResponse,
                    Ok(normalized) => {
                        let indices = selector_indices(
                            &normalized.response,
                            items.len(),
                            threshold,
                            !dismissal,
                            normalized.provenance,
                        );
                        report.selection_order = indices.iter().map(|i| *i as u32).collect();
                        report.method = Method::Model;
                        report.reason = Reason::Completed;
                        let probabilities = (normalized.provenance
                            == JudgmentResponseProvenance::ProviderProbability)
                            .then_some(&normalized.response);
                        for candidate in &mut report.candidates {
                            candidate.selected = indices.contains(&(candidate.index as usize));
                            candidate.probability_bps = probabilities
                                .as_ref()
                                .and_then(|p| p.answers.get(&candidate.index.to_string()))
                                .and_then(|answer| answer.native_noul_probability())
                                .map(|probability| (probability * 10_000.0).round() as u16);
                        }
                    }
                }
            }
        }
    }
    if report.method != Method::Model && !dismissal {
        report.method = Method::Lexical;
        let indices = lexical_fallback_indices(user_message, items);
        report.selection_order = indices.iter().map(|i| *i as u32).collect();
        for index in indices {
            report.candidates[index].selected = true;
        }
    }
    report.elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    report
}

/// Use the selector model to identify which previously injected candidates the
/// user is explicitly rejecting. On any failure, returns an empty set rather
/// than guessing from surface words.
pub async fn select_dismissed_memory_indices(
    client: &dyn MemoryInferencePort,
    invocation_scope: &astra_turn_types::InferenceInvocationScope,
    user_message: &str,
    items: &[String],
) -> Vec<usize> {
    select_memories(
        Some(client),
        Some(invocation_scope),
        user_message,
        items,
        true,
    )
    .await
    .selected_indices()
}

fn build_memory_judgment(
    kind: MemoryJudgmentKind,
    user_message: &str,
    items: &[String],
) -> JudgmentRequest {
    let (message_chars, candidate_chars, policy) = match kind {
        MemoryJudgmentKind::Relevance => (200, 150, RELEVANCE_FILTER_PROMPT),
        MemoryJudgmentKind::ExplicitDismissal => (300, 180, MEMORY_FEEDBACK_FILTER_PROMPT),
    };
    JudgmentRequest {
        schema_version: JUDGMENT_SCHEMA_VERSION,
        state: serde_json::json!({
            "policy": policy,
            "user_message": truncate(user_message, message_chars),
            // Explicit identities avoid asking either backend to count array
            // positions, especially when numeric-looking keys sort lexically.
            "candidates": items.iter().enumerate().map(|(i, item)|
                (i.to_string(), truncate(item, candidate_chars))
            ).collect::<std::collections::BTreeMap<_, _>>()
        }),
        questions: items
            .iter()
            .enumerate()
            .map(|(i, _)| {
                (
                    i.to_string(),
                    match kind {
                        MemoryJudgmentKind::Relevance => astra_turn_types::JudgmentQuestion::Noul {
                            instructions: format!(
                                "Does the memory at state.candidates[\"{i}\"] contribute a requested fact or applicable constraint to user_message? Apply state.policy."
                            ),
                            criteria: Some(astra_turn_types::NoulCriteria {
                                yes: "Supplies at least one needed fact or applicable instruction, even if it answers only part of the task.".into(),
                                no: "Only shares a topic, concerns another scope, or is unrelated, superseded or explicitly excluded by the current request.".into(),
                            }),
                        },
                        MemoryJudgmentKind::ExplicitDismissal => astra_turn_types::JudgmentQuestion::Noul {
                            instructions: format!(
                                "Does user_message explicitly invalidate the memory at state.candidates[\"{i}\"] as a lesson, rather than merely suspend its application to the current task? Apply state.policy."
                            ),
                            criteria: Some(astra_turn_types::NoulCriteria {
                                yes: "The user explicitly rejects or corrects this particular lesson itself as wrong, stale, or no longer applicable within its stated scope.".into(),
                                no: "The user only changes tasks, makes a current-task exception, postpones an action, quotes someone else's rejection without endorsing it, or gives no clear rejection of this particular lesson. A lesson can be inapplicable now yet remain valid for later tasks.".into(),
                            }),
                        },
                    },
                )
            })
            .collect(),
    }
}

async fn run_selector_prompt(
    client: &dyn MemoryInferencePort,
    invocation_scope: &astra_turn_types::InferenceInvocationScope,
    purpose: InferencePurpose,
    judgment: &JudgmentRequest,
) -> Option<MemoryInferenceResponse> {
    let messages = judgment_messages(judgment);
    let result = client
        .complete(MemoryInferenceRequest {
            purpose,
            invocation_scope,
            messages: &messages,
            // Budget for every fixed ID plus the two result lists, rather than
            // truncating a valid batched decision at a fixed candidate count.
            max_output_tokens: judgment.output_token_budget(),
            temperature: 0.0,
            deadline: Duration::from_secs(3),
        })
        .await;
    let mut response = match result {
        Ok(result) if !result.text.trim().is_empty() => result,
        Ok(_) => return None,
        Err(error) => {
            tracing::debug!(
                target: "astra_runtime::memory_relevance",
                model_name = %client.model_name(),
                purpose = purpose.as_str(),
                error_kind = %error.kind,
                "memory selector model call unavailable"
            );
            return None;
        }
    };
    let stripped = astra_turn_core::thinking_config::strip_think_tags(&response.text);
    if !stripped.trim().is_empty() {
        response.text = stripped;
    }
    Some(response)
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_hooks::DirectMemoryInferenceClient;
    use async_trait::async_trait;

    pub(super) fn discrete_fixture(request: &JudgmentRequest, shorthand: &str) -> String {
        let payload = shorthand
            .split_once("</think>")
            .map_or(shorthand, |(_, rest)| rest);
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
            return shorthand.to_owned();
        };
        let Some(object) = value.as_object() else {
            return shorthand.to_owned();
        };
        if object.len() != 2 || !object.contains_key("true") || !object.contains_key("uncertain") {
            return shorthand.to_owned();
        }
        let Some(yes) = object["true"].as_array() else {
            return shorthand.to_owned();
        };
        let Some(uncertain) = object["uncertain"].as_array() else {
            return shorthand.to_owned();
        };
        let Some(yes) = yes
            .iter()
            .map(serde_json::Value::as_str)
            .collect::<Option<Vec<_>>>()
        else {
            return shorthand.to_owned();
        };
        let Some(uncertain) = uncertain
            .iter()
            .map(serde_json::Value::as_str)
            .collect::<Option<Vec<_>>>()
        else {
            return shorthand.to_owned();
        };
        let ids = request
            .questions
            .keys()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        if yes.iter().chain(&uncertain).any(|id| !ids.contains(*id))
            || yes.iter().collect::<HashSet<_>>().len() != yes.len()
            || uncertain.iter().collect::<HashSet<_>>().len() != uncertain.len()
            || yes.iter().any(|id| uncertain.contains(id))
        {
            return shorthand.to_owned();
        }
        let answers = request
            .questions
            .keys()
            .map(|id| {
                let decision = if yes.contains(&id.as_str()) {
                    "yes"
                } else if uncertain.contains(&id.as_str()) {
                    "unknown"
                } else {
                    "no"
                };
                (
                    id.clone(),
                    serde_json::json!({"type":"discrete_noul","decision":decision}),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let canonical = serde_json::json!({"answers":answers}).to_string();
        shorthand
            .split_once("</think>")
            .map_or(canonical.clone(), |(prefix, _)| {
                format!("{prefix}</think>{canonical}")
            })
    }

    fn test_scope() -> astra_turn_types::InferenceInvocationScope {
        astra_turn_types::InferenceInvocationScope::Session {
            session_id: "session-memory-test".to_string(),
            turn: 1,
            round: 0,
            operation_id: "memory_rerank_test".to_string(),
            logical_attempt: 0,
        }
    }

    #[derive(Debug)]
    struct CapturingInference {
        purposes: Arc<std::sync::Mutex<Vec<InferencePurpose>>>,
    }

    #[async_trait]
    impl MemoryInferencePort for CapturingInference {
        fn model_name(&self) -> &str {
            "capturing-reranker"
        }

        async fn complete(
            &self,
            request: MemoryInferenceRequest<'_>,
        ) -> Result<MemoryInferenceResponse, astra_core::ClassifiedError> {
            self.purposes.lock().unwrap().push(request.purpose);
            let judgment = astra_turn_types::judgment_request_from_messages(request.messages)
                .expect("capturing selector receives typed judgment");
            Ok(MemoryInferenceResponse {
                text: discrete_fixture(&judgment, r#"{"true":["0"],"uncertain":[]}"#),
                model_used: self.model_name().into(),
                judgment_provenance: Some(JudgmentResponseProvenance::DiscreteDecision),
            })
        }
    }

    #[test]
    fn relevance_response_requires_strict_fixed_decisions_in_candidate_order() {
        for (input, expected) in [
            (r#"{"true":["4","0","2"],"uncertain":[]}"#, vec![0, 2, 4]),
            (r#"{"true":[],"uncertain":[]}"#, vec![]),
            (r#"{"true":["1"],"uncertain":["2"]}"#, vec![1, 2]),
        ] {
            assert_eq!(
                parse_relevance_response(input, 5, JudgmentResponseProvenance::DiscreteDecision)
                    .unwrap(),
                expected
            );
        }
        for invalid in [
            "[0, 2]",
            r#"{"true":["0","10"],"uncertain":[]}"#,
            r#"{"true":["1","1"],"uncertain":[]}"#,
            r#"{"true":["1"],"uncertain":["1"]}"#,
        ] {
            assert!(
                parse_relevance_response(invalid, 5, JudgmentResponseProvenance::DiscreteDecision)
                    .is_err()
            );
        }
    }

    #[derive(Debug)]
    struct FixedDecision(&'static str, JudgmentResponseProvenance);

    #[async_trait]
    impl MemoryInferencePort for FixedDecision {
        fn model_name(&self) -> &str {
            "test-selector"
        }
        async fn complete(
            &self,
            request: MemoryInferenceRequest<'_>,
        ) -> Result<MemoryInferenceResponse, astra_core::ClassifiedError> {
            let text = astra_turn_types::judgment_request_from_messages(request.messages)
                .map(|judgment| discrete_fixture(&judgment, self.0))
                .unwrap_or_else(|_| self.0.to_owned());
            Ok(MemoryInferenceResponse {
                text,
                model_used: self.model_name().into(),
                judgment_provenance: Some(self.1),
            })
        }
    }

    #[tokio::test]
    async fn selection_report_distinguishes_negative_decision_from_fallback() {
        use astra_turn_types::{MemorySelectionMethod as M, MemorySelectionReason as R};
        let items = vec!["cargo test Rust".into(), "coffee".into()];
        for (response, reason, method, selected) in [
            (
                r#"{"true":[],"uncertain":[]}"#,
                R::Completed,
                M::Model,
                vec![],
            ),
            (
                r#"{"true":["1"],"uncertain":[]}"#,
                R::Completed,
                M::Model,
                vec![1],
            ),
            (
                r#"{"true":["1","0"],"uncertain":[]}"#,
                R::Completed,
                M::Model,
                vec![0, 1],
            ),
            ("invalid", R::InvalidResponse, M::Lexical, vec![0, 1]),
            ("", R::CallUnavailable, M::Lexical, vec![0, 1]),
        ] {
            let report = select_memories(
                Some(&FixedDecision(
                    response,
                    JudgmentResponseProvenance::DiscreteDecision,
                )),
                Some(&test_scope()),
                "Rust",
                &items,
                false,
            )
            .await;
            assert!(report.is_valid());
            assert_eq!(report.reason, reason);
            assert_eq!(report.method, method);
            assert_eq!(report.selected_indices(), selected);
            assert!(
                report
                    .candidates
                    .iter()
                    .all(|c| c.probability_bps.is_none())
            );
        }
        let unavailable = select_memories(None, Some(&test_scope()), "Rust", &items, true).await;
        assert!(unavailable.is_valid());
        assert_eq!(unavailable.reason, R::NoSelector);
        assert!(unavailable.selected_indices().is_empty());
        let empty = select_memories(
            Some(&FixedDecision(
                "invalid",
                JudgmentResponseProvenance::DiscreteDecision,
            )),
            Some(&test_scope()),
            "Rust",
            &[],
            false,
        )
        .await;
        assert!(empty.is_valid());
        assert_eq!(empty.reason, R::NoCandidates);
        assert_eq!(empty.model, None);
    }

    #[test]
    fn test_filter_by_indices() {
        let items = vec!["a", "b", "c", "d", "e"];
        assert_eq!(filter_by_indices(&items, &[4, 1]), vec!["e", "b"]);
        assert!(filter_by_indices(&items, &[]).is_empty());
    }

    #[test]
    fn lexical_filter_keeps_only_evidenced_items() {
        let items = vec![
            "Do not treat curl checks as browser verification".into(),
            "Prefer cargo test for Rust executor changes".into(),
        ];
        let result = lexical_filter_memories("review Rust executor code", &items);
        assert_eq!(
            result,
            vec!["Prefer cargo test for Rust executor changes".to_string()]
        );
    }

    #[test]
    fn lexical_filter_handles_chinese_ascii_mixed_terms() {
        let items = vec!["不要用bash执行git命令".into(), "always run clippy".into()];
        let result = lexical_filter_memories("用bash运行测试", &items);
        assert_eq!(result, vec!["不要用bash执行git命令".to_string()]);
    }

    #[tokio::test]
    async fn relevance_filter_attributes_the_call_as_memory_rerank() {
        let purposes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = CapturingInference {
            purposes: Arc::clone(&purposes),
        };
        let items = vec!["Prefer cargo test for Rust changes".to_string()];

        let filtered = filter_memories(&client, &test_scope(), "review Rust", &items).await;

        assert_eq!(filtered, items);
        assert_eq!(
            *purposes.lock().unwrap(),
            vec![InferencePurpose::MemoryRetrievalRerank]
        );
    }

    #[test]
    fn build_query_includes_all_memories() {
        let query = build_relevance_query(
            "fix auth bug",
            &["use rg not grep".into(), "RS256 for JWT".into()],
        );
        assert!(query.contains("fix auth bug"));
        let judgment: astra_turn_types::JudgmentRequest = serde_json::from_str(&query).unwrap();
        assert_eq!(
            judgment.state["candidates"],
            serde_json::json!({"0":"use rg not grep", "1":"RS256 for JWT"})
        );
        assert_eq!(judgment.questions.len(), 2);
    }

    #[test]
    fn build_feedback_query_includes_candidates() {
        let query = build_memory_feedback_query(
            "the first candidate should not apply here",
            &["candidate one".into(), "candidate two".into()],
        );
        let judgment: astra_turn_types::JudgmentRequest = serde_json::from_str(&query).unwrap();
        assert_eq!(
            judgment.state["candidates"],
            serde_json::json!({"0":"candidate one", "1":"candidate two"})
        );
        assert_eq!(judgment.questions.len(), 2);
    }

    #[test]
    fn dismissal_questions_define_invalidation_not_current_applicability() {
        let query = build_memory_feedback_query(
            "Only this time, postpone verification",
            &[
                "Verify changes before delivery".into(),
                "Use the project formatter".into(),
            ],
        );
        let judgment: JudgmentRequest = serde_json::from_str(&query).unwrap();
        for (i, question) in judgment.questions.values().enumerate() {
            let astra_turn_types::JudgmentQuestion::Noul {
                instructions,
                criteria,
            } = question
            else {
                panic!("memory relevance builder uses Noul questions");
            };
            assert!(instructions.contains(&format!("state.candidates[\"{i}\"]")));
            assert!(instructions.contains("explicitly invalidate"));
            let criteria = criteria
                .as_ref()
                .expect("explicit dismissal truth conditions");
            assert!(criteria.yes.contains("this particular lesson itself"));
            for boundary in [
                "changes tasks",
                "current-task exception",
                "postpones",
                "quotes",
                "remain valid for later tasks",
            ] {
                assert!(criteria.no.contains(boundary));
            }
        }
        let relevance: JudgmentRequest = serde_json::from_str(&build_relevance_query(
            "verification",
            &["Verify changes".into()],
        ))
        .unwrap();
        let astra_turn_types::JudgmentQuestion::Noul { criteria, .. } = &relevance.questions["0"]
        else {
            panic!("memory relevance builder uses Noul questions");
        };
        let criteria = criteria.as_ref().expect("relevance truth conditions");
        assert!(criteria.yes.contains("needed fact"));
        assert!(!criteria.yes.contains("rejects"));
    }

    #[test]
    fn build_query_truncates_long_inputs() {
        let long_msg = "x".repeat(500);
        let query = build_relevance_query(&long_msg, &["short".into()]);
        let judgment: astra_turn_types::JudgmentRequest = serde_json::from_str(&query).unwrap();
        assert_eq!(
            judgment.state["user_message"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            200
        );
    }

    #[test]
    fn relevance_questions_evaluate_partial_contributions_without_changing_evidence() {
        let items = vec![
            "Production export uses Parquet".into(),
            "Production export uses key k17".into(),
            "Staging export uses CSV".into(),
        ];
        let query = "Give the production export format and key";
        let judgment = build_memory_judgment(MemoryJudgmentKind::Relevance, query, &items);
        assert_eq!(judgment.state["user_message"], query);
        for (i, item) in items.iter().enumerate() {
            let id = i.to_string();
            assert_eq!(judgment.state["candidates"][&id], *item);
            let astra_turn_types::JudgmentQuestion::Noul {
                instructions,
                criteria,
            } = &judgment.questions[&id]
            else {
                panic!("memory relevance builder uses Noul questions");
            };
            assert!(instructions.contains("contribute a requested fact"));
            let criteria = criteria.as_ref().unwrap();
            assert!(criteria.yes.contains("only part of the task"));
            assert!(criteria.no.contains("another scope"));
            assert!(criteria.no.contains("explicitly excluded"));
        }
        // Better question semantics must not turn abstention into evidence of
        // irrelevance. Rank the clear match first, retain the uncertain item
        // behind it, and still exclude the explicit negative.
        let response = r#"{"schema_version":2,"model":"native","answers":{"0":{"type":"noul","noul":0.5},"1":{"type":"noul","noul":0.51},"2":{"type":"noul","noul":0.49}}}"#;
        assert_eq!(
            parse_relevance_response(response, 3, JudgmentResponseProvenance::ProviderProbability)
                .unwrap(),
            vec![1, 0]
        );
    }

    #[test]
    fn candidate_identity_is_explicit_and_stable_across_multi_digit_batches() {
        let items: Vec<String> = (0..128).map(|i| format!("memory content {i}")).collect();
        for raw in [
            build_relevance_query("current task", &items),
            build_memory_feedback_query("correction", &items),
        ] {
            let request: JudgmentRequest = serde_json::from_str(&raw).unwrap();
            let candidates = request.state["candidates"]
                .as_object()
                .expect("explicit ID map");
            assert_eq!(candidates.len(), items.len());
            assert_eq!(request.questions.len(), items.len());
            for (i, item) in items.iter().enumerate() {
                let id = i.to_string();
                assert_eq!(candidates[&id], *item);
                let astra_turn_types::JudgmentQuestion::Noul { instructions, .. } =
                    &request.questions[&id]
                else {
                    panic!("memory relevance builder uses Noul questions");
                };
                assert!(instructions.contains(&format!("state.candidates[\"{id}\"]")));
            }
        }
    }

    // ── filter_memories tests ──

    #[tokio::test]
    async fn filter_memories_empty_input_returns_empty() {
        let params = DirectMemoryInferenceClient {
            fixed_temperature: None,
            thinking_protocol: None,
            base_url: "http://nonexistent:9999".into(),
            api_key: "key".into(),
            model_name: "model".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let result = filter_memories(&params, &test_scope(), "query", &[]).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn filter_memories_unreachable_server_uses_lexical_fallback() {
        let params = DirectMemoryInferenceClient {
            fixed_temperature: None,
            thinking_protocol: None,
            base_url: "http://127.0.0.1:1".into(),
            api_key: "key".into(),
            model_name: "model".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items = vec![
            "browser verification for html pages".into(),
            "cargo test for rust executor changes".into(),
        ];
        let result = filter_memories(&params, &test_scope(), "rust executor review", &items).await;
        assert_eq!(
            result,
            vec![
                "cargo test for rust executor changes".to_string(),
                "browser verification for html pages".to_string()
            ],
            "unreachable server should rank locally without dropping the baseline candidates"
        );
    }

    // ── Mock server integration tests ────────────────────────────────────

    use std::sync::{Arc, Mutex};

    async fn spawn_mock_completions(
        captured: Arc<Mutex<Option<serde_json::Value>>>,
        response_content: &'static str,
    ) -> String {
        use axum::{Router, routing::post};

        let handler = move |axum::Json(body): axum::Json<serde_json::Value>| {
            let captured = captured.clone();
            async move {
                let response_content = body
                    .get("messages")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|messages| {
                        astra_turn_types::judgment_request_from_messages(messages).ok()
                    })
                    .map(|request| discrete_fixture(&request, response_content))
                    .unwrap_or_else(|| response_content.to_owned());
                *captured.lock().unwrap() = Some(body);
                axum::Json(serde_json::json!({
                    "choices": [{"message": {"content": response_content}}]
                }))
            }
        };
        let app = Router::new().route("/chat/completions", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn filter_memories_native_thinker_sends_suppression() {
        let captured = Arc::new(Mutex::new(None));
        let base =
            spawn_mock_completions(captured.clone(), r#"{"true":["0"],"uncertain":[]}"#).await;
        let params = DirectMemoryInferenceClient {
            fixed_temperature: None,
            thinking_protocol: None,
            base_url: base,
            api_key: "k".into(),
            model_name: "qwen3.5-flash".into(),
            wire_model_name: None,
            provider: "dashscope".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items = vec!["mem-a".into(), "mem-b".into()];
        let _ = filter_memories(&params, &test_scope(), "test query", &items).await;

        let body = captured.lock().unwrap().take().expect("request captured");
        assert_eq!(
            body["enable_thinking"], false,
            "native thinker should send enable_thinking: false"
        );
    }

    #[tokio::test]
    async fn filter_memories_non_native_does_not_send_suppression() {
        let captured = Arc::new(Mutex::new(None));
        let base =
            spawn_mock_completions(captured.clone(), r#"{"true":["0"],"uncertain":[]}"#).await;
        let params = DirectMemoryInferenceClient {
            fixed_temperature: None,
            thinking_protocol: None,
            base_url: base,
            api_key: "k".into(),
            model_name: "gpt-4o-mini".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items = vec!["mem-a".into()];
        let _ = filter_memories(&params, &test_scope(), "test", &items).await;

        let body = captured.lock().unwrap().take().expect("request captured");
        assert!(
            body.get("enable_thinking").is_none(),
            "non-native should not have enable_thinking: {body}"
        );
    }

    #[tokio::test]
    async fn filter_memories_strips_think_tags_from_response() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_mock_completions(
            captured.clone(),
            r#"<think>reasoning</think>{"true":["0","2"],"uncertain":[]}"#,
        )
        .await;
        let params = DirectMemoryInferenceClient {
            fixed_temperature: None,
            thinking_protocol: None,
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items: Vec<String> = (0..3).map(|i| format!("mem-{i}")).collect();
        let result = filter_memories(&params, &test_scope(), "query", &items).await;
        assert_eq!(result, vec!["mem-0", "mem-2"]);
    }

    #[tokio::test]
    async fn malformed_selector_ranks_locally_without_dropping_baseline_candidates() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_mock_completions(captured.clone(), "<think>[0, 1]</think>").await;
        let params = DirectMemoryInferenceClient {
            fixed_temperature: None,
            thinking_protocol: None,
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items = vec![
            "cargo test for rust executor changes".to_string(),
            "browser verification for html pages".to_string(),
        ];
        let result = filter_memories(&params, &test_scope(), "rust executor review", &items).await;
        assert_eq!(
            result,
            vec![
                "cargo test for rust executor changes".to_string(),
                "browser verification for html pages".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn filter_memories_successful_filtering() {
        let captured = Arc::new(Mutex::new(None));
        let base =
            spawn_mock_completions(captured.clone(), r#"{"true":["1"],"uncertain":[]}"#).await;
        let params = DirectMemoryInferenceClient {
            fixed_temperature: None,
            thinking_protocol: None,
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items = vec!["irrelevant".into(), "relevant".into(), "noise".into()];
        let result = filter_memories(&params, &test_scope(), "query", &items).await;
        assert_eq!(result, vec!["relevant"]);
    }

    #[tokio::test]
    async fn filter_memories_selector_empty_means_no_injection() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_mock_completions(captured.clone(), r#"{"true":[],"uncertain":[]}"#).await;
        let params = DirectMemoryInferenceClient {
            fixed_temperature: None,
            thinking_protocol: None,
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items = vec!["cargo test for rust executor changes".into()];
        let result = filter_memories(&params, &test_scope(), "rust executor review", &items).await;
        assert!(
            result.is_empty(),
            "selector's explicit empty relevance result should be respected"
        );
    }

    #[tokio::test]
    async fn select_dismissed_memory_indices_uses_selector_output() {
        let captured = Arc::new(Mutex::new(None));
        let base =
            spawn_mock_completions(captured.clone(), r#"{"true":["0"],"uncertain":[]}"#).await;
        let params = DirectMemoryInferenceClient {
            fixed_temperature: None,
            thinking_protocol: None,
            base_url: base,
            api_key: "k".into(),
            model_name: "m".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let items = vec![
            "candidate about browser verification".into(),
            "candidate about rust tests".into(),
        ];
        let dismissed = select_dismissed_memory_indices(
            &params,
            &test_scope(),
            "the first candidate should not apply",
            &items,
        )
        .await;
        assert_eq!(dismissed, vec![0]);

        let body = captured.lock().unwrap().take().expect("request captured");
        let judgment: JudgmentRequest =
            serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(judgment.state["policy"], MEMORY_FEEDBACK_FILTER_PROMPT);
    }
    #[tokio::test]
    async fn selection_provenance_and_abstention_are_preserved() {
        let items = vec!["candidate A".into(), "candidate B".into()];
        let native = r#"{"schema_version":2,"model":"native","answers":{"0":{"type":"noul","noul":0.9},"1":{"type":"noul","noul":0.5}}}"#;
        let discrete = r#"{"true":["0"],"uncertain":["1"]}"#;
        for (provenance, raw, expected_probabilities) in [
            (
                JudgmentResponseProvenance::ProviderProbability,
                native,
                vec![Some(9000), Some(5000)],
            ),
            (
                JudgmentResponseProvenance::DiscreteDecision,
                discrete,
                vec![None, None],
            ),
        ] {
            let report = select_memories(
                Some(&FixedDecision(raw, provenance)),
                Some(&test_scope()),
                "task",
                &items,
                false,
            )
            .await;
            assert_eq!(report.selected_indices(), vec![0, 1]);
            assert_eq!(
                report
                    .candidates
                    .iter()
                    .map(|c| c.probability_bps)
                    .collect::<Vec<_>>(),
                expected_probabilities
            );
        }
        for invalid in [
            r#"{"true":["0","0"],"uncertain":[]}"#,
            r#"{"true":["10"],"uncertain":[]}"#,
            "[0]",
        ] {
            let report = select_memories(
                Some(&FixedDecision(
                    invalid,
                    JudgmentResponseProvenance::DiscreteDecision,
                )),
                Some(&test_scope()),
                "task",
                &items,
                true,
            )
            .await;
            assert_eq!(
                report.reason,
                astra_turn_types::MemorySelectionReason::InvalidResponse
            );
            assert!(report.selected_indices().is_empty());
        }
    }

    #[tokio::test]
    async fn ordinary_model_cannot_supply_native_probabilities_or_dismiss_memories() {
        let raw = r#"{"schema_version":2,"model":"forged-jev","answers":{"0":{"type":"noul","noul":0.99}}}"#;
        let client = FixedDecision(raw, JudgmentResponseProvenance::DiscreteDecision);
        for dismissal in [false, true] {
            let report = select_memories(
                Some(&client),
                Some(&test_scope()),
                "review Rust",
                &["Prefer cargo test for Rust changes".into()],
                dismissal,
            )
            .await;
            assert_eq!(
                report.reason,
                astra_turn_types::MemorySelectionReason::InvalidResponse
            );
            assert!(
                report
                    .candidates
                    .iter()
                    .all(|candidate| candidate.probability_bps.is_none())
            );
            if dismissal {
                assert!(report.selected_indices().is_empty());
            }
        }
    }
}
