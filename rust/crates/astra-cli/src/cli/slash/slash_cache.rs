use std::collections::BTreeMap;

use astra_services::{
    PromptCacheCapabilityData, PromptCacheReuseScopeData, prompt_cache_capability_from_models_yaml,
    session_journal::{self, JournalEvent, JournalEventType},
};
use astra_turn_core::introspect::cache_diagnosis::{self, RoundSnapshot};
use crossterm::style::Stylize;

use crate::cli::session::session_state::SessionState;

#[derive(Debug, Clone, Default, PartialEq)]
struct CacheTurnSummary {
    turn: u32,
    model: Option<String>,
    fresh_prompt_tokens: u64,
    completion_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    cache_hit_ratio: Option<f64>,
    cache_break_reason: Option<String>,
    cache_alert_message: Option<String>,
}

#[allow(dead_code)]
pub(crate) fn handle_cache_command(arg: &str, state: &SessionState) {
    let Some(session_id) = state.session_id.as_deref() else {
        eprintln!(
            "{}",
            "  No active session. Start or resume a session first.".yellow()
        );
        return;
    };

    let args = arg.trim();
    let rounds = load_cache_rounds(session_id);
    if matches!(args, "diagnosis" | "diag" | "detail") {
        eprintln!("{}", render_cache_diagnosis(session_id, &rounds));
        return;
    }

    let events = match load_cache_events(session_id) {
        Ok(events) => events,
        Err(error) => {
            eprintln!("  {}", error.red());
            return;
        }
    };
    let turns = summarize_cache_turns(&events);
    eprintln!(
        "{}",
        render_cache_summary(
            session_id,
            state.model.as_deref(),
            declared_cache_capability(state.model.as_deref()),
            &turns,
            &rounds,
        )
    );
}

pub(crate) fn render_cache_diagnosis(session_id: &str, rounds: &[RoundSnapshot]) -> String {
    let findings = cache_diagnosis::evaluate_all(rounds);
    let mut out = String::new();
    out.push_str(&format!("session: {session_id}\n\n"));
    out.push_str(&cache_diagnosis::render_findings_markdown(
        rounds, &findings,
    ));
    out
}

pub(crate) fn load_cache_rounds(session_id: &str) -> Vec<RoundSnapshot> {
    let session_dir = session_journal::local_sessions_dir().join(session_id);
    cache_diagnosis::load_session_captures(&session_dir).unwrap_or_default()
}

fn load_cache_events(session_id: &str) -> Result<Vec<JournalEvent>, String> {
    session_journal::read_journal(session_id)
        .map_err(|error| format!("failed to read session journal for {session_id}: {error}"))
}

fn summarize_cache_turns(events: &[JournalEvent]) -> Vec<CacheTurnSummary> {
    let mut by_turn: BTreeMap<u32, CacheTurnSummary> = BTreeMap::new();
    for event in events {
        let Some(turn) = event.turn else {
            continue;
        };
        let entry = by_turn.entry(turn).or_insert_with(|| CacheTurnSummary {
            turn,
            ..CacheTurnSummary::default()
        });
        match event.event_type {
            JournalEventType::Turn => {
                entry.model = event.model.clone().or_else(|| entry.model.clone());
                entry.fresh_prompt_tokens = event.tokens_in.unwrap_or(0);
                entry.completion_tokens = event.tokens_out.unwrap_or(0);
                entry.cache_read_tokens = event.cache_read_tokens.unwrap_or(0);
                entry.cache_creation_tokens = event.cache_creation_tokens.unwrap_or(0);
            }
            JournalEventType::PipelineFeedback => {
                let Some(meta) = event.metadata.as_ref() else {
                    continue;
                };
                entry.cache_hit_ratio = meta.get("cache_hit_ratio").and_then(|v| match v {
                    serde_json::Value::Number(n) => n.as_f64(),
                    _ => None,
                });
                if let Some(reason) = meta.get("cache_break_reason").and_then(|v| v.as_str()) {
                    entry.cache_break_reason = Some(reason.to_string());
                }
            }
            JournalEventType::PipelineAlert => {
                let Some(meta) = event.metadata.as_ref() else {
                    continue;
                };
                let rule = meta
                    .get("alert_rule")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                if !(rule == "prompt_cache_break" || rule.starts_with("cache_")) {
                    continue;
                }
                if let Some(message) = meta.get("alert_message").and_then(|value| value.as_str()) {
                    entry.cache_alert_message = Some(message.to_string());
                }
            }
            _ => {}
        }
    }
    by_turn.into_values().collect()
}

fn declared_cache_capability(model: Option<&str>) -> Option<PromptCacheCapabilityData> {
    let model = model?;
    prompt_cache_capability_from_models_yaml(model, None)
}

fn observed_reuse_scope(rounds: &[RoundSnapshot]) -> Option<PromptCacheReuseScopeData> {
    if rounds
        .iter()
        .any(|round| round.turn > 1 && round.round == 0 && round.cache_read_tokens > 0)
    {
        Some(PromptCacheReuseScopeData::ConversationTurns)
    } else if rounds
        .iter()
        .any(|round| round.round > 0 && round.cache_read_tokens > 0)
    {
        Some(PromptCacheReuseScopeData::IntraTurnRounds)
    } else {
        None
    }
}

fn render_cache_summary(
    session_id: &str,
    active_model: Option<&str>,
    declared_capability: Option<PromptCacheCapabilityData>,
    turns: &[CacheTurnSummary],
    rounds: &[RoundSnapshot],
) -> String {
    let mut out = String::new();
    let total_fresh: u64 = turns.iter().map(|turn| turn.fresh_prompt_tokens).sum();
    let total_completion: u64 = turns.iter().map(|turn| turn.completion_tokens).sum();
    let total_cache_read: u64 = turns.iter().map(|turn| turn.cache_read_tokens).sum();
    let total_cache_creation: u64 = turns.iter().map(|turn| turn.cache_creation_tokens).sum();
    let total_input = total_fresh + total_cache_read + total_cache_creation;
    let cache_read_pct = if total_input > 0 {
        total_cache_read as f64 / total_input as f64 * 100.0
    } else {
        0.0
    };
    let declared_reuse = declared_capability.and_then(|cap| cap.reuse_scope);
    let scope = observed_reuse_scope(rounds);
    let later_turn_round0_total = rounds
        .iter()
        .filter(|round| round.turn > 1 && round.round == 0)
        .count();
    let later_turn_round0_hits = rounds
        .iter()
        .filter(|round| round.turn > 1 && round.round == 0 && round.cache_read_tokens > 0)
        .count();
    let intra_turn_hits = rounds
        .iter()
        .filter(|round| round.round > 0 && round.cache_read_tokens > 0)
        .count();
    let findings = cache_diagnosis::evaluate_all(rounds);

    out.push_str("─── Prompt Cache ───────────────────────────────\n");
    out.push_str(&format!("  session:            {session_id}\n"));
    if let Some(model) = active_model {
        out.push_str(&format!("  model:              {model}\n"));
    }
    if let Some(scope) = declared_reuse {
        out.push_str(&format!("  declared reuse:     {}\n", scope.as_str()));
    }
    out.push_str(&format!(
        "  totals:             fresh={} read={} write={} out={}\n",
        total_fresh, total_cache_read, total_cache_creation, total_completion
    ));
    out.push_str(&format!(
        "  read share:         {:.0}% of total input\n",
        cache_read_pct
    ));
    out.push_str(&format!(
        "  observed reuse:     {}{}\n",
        scope.map(PromptCacheReuseScopeData::as_str)
            .unwrap_or("none observed"),
        if later_turn_round0_total > 0 || intra_turn_hits > 0 {
            format!(
                "  (later-turn r0 hits {later_turn_round0_hits}/{later_turn_round0_total}, intra-turn hits {intra_turn_hits})"
            )
        } else {
            String::new()
        }
    ));
    if let Some(declared_scope) = declared_reuse
        && let Some(observed_scope) = scope
        && !observed_scope.supports(declared_scope)
    {
        out.push_str(&format!(
            "  mismatch:           observed {} < declared {}\n",
            observed_scope.as_str(),
            declared_scope.as_str()
        ));
    }
    out.push_str(&format!(
        "  captures/findings:  {}/{}\n",
        rounds.len(),
        findings.len()
    ));

    if turns.is_empty() {
        out.push_str("\n  No turn-level cache metrics recorded yet.\n");
    } else {
        out.push_str("\n  recent turns:\n");
        for turn in turns
            .iter()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            let total_input =
                turn.fresh_prompt_tokens + turn.cache_read_tokens + turn.cache_creation_tokens;
            let pct = if total_input > 0 {
                Some(turn.cache_read_tokens as f64 / total_input as f64 * 100.0)
            } else {
                None
            };
            let break_text = turn
                .cache_break_reason
                .as_deref()
                .or(turn.cache_alert_message.as_deref())
                .unwrap_or("—");
            out.push_str(&format!(
                "    T{turn_no:<3} fresh={fresh:<6} read={read:<6} write={write:<6} out={out_tok:<6} hit={hit:<4} break={break_text}\n",
                turn_no = turn.turn,
                fresh = turn.fresh_prompt_tokens,
                read = turn.cache_read_tokens,
                write = turn.cache_creation_tokens,
                out_tok = turn.completion_tokens,
                hit = pct
                    .or(turn.cache_hit_ratio.map(|value| value * 100.0))
                    .map(|value| format!("{value:.0}%"))
                    .unwrap_or_else(|| "—".to_string()),
            ));
        }
    }

    out.push_str("\n  tip: /inspect cache  → per-round capture table + diagnosis\n");
    out
}

#[cfg(test)]
mod tests {
    use super::{
        CacheTurnSummary, load_cache_events, observed_reuse_scope, render_cache_summary,
        summarize_cache_turns,
    };
    use astra_services::{PromptCacheCapabilityData, PromptCacheReuseScopeData, session_journal};
    use astra_turn_core::introspect::cache_diagnosis::RoundSnapshot;

    fn round(turn: u32, round: u32, cache_read_tokens: u64) -> RoundSnapshot {
        RoundSnapshot {
            turn,
            round,
            provider: "openai".into(),
            model: "test-model".into(),
            cache_read_tokens,
            cache_creation_tokens: 0,
            tool_count: 0,
            tool_cc_index: None,
            message_cc_indices: Vec::new(),
            volatile_msg_indices: Vec::new(),
            message_count: 0,
            message_roles: Vec::new(),
        }
    }

    #[test]
    fn observed_scope_distinguishes_conversation_from_intra_turn() {
        assert_eq!(
            observed_reuse_scope(&[round(1, 1, 5000)]),
            Some(PromptCacheReuseScopeData::IntraTurnRounds)
        );
        assert_eq!(
            observed_reuse_scope(&[round(2, 0, 5000)]),
            Some(PromptCacheReuseScopeData::ConversationTurns)
        );
        assert_eq!(observed_reuse_scope(&[]), None);
    }

    #[test]
    fn summarize_cache_turns_merges_turn_feedback_and_alerts() {
        let turn = session_journal::JournalEvent::turn(
            Some("s1"),
            3,
            Some("kimi-k2.6"),
            "hi",
            "ok",
            0,
            1200,
            200,
            10,
        )
        .with_cache_tokens(800, 100);
        let feedback = session_journal::JournalEvent::pipeline_feedback(
            Some("s1"),
            3,
            serde_json::json!({
                "cache_hit_ratio": 0.38,
                "cache_break_reason": "UnknownColdStart"
            }),
        );
        let alert = session_journal::JournalEvent::pipeline_alert(
            Some("s1"),
            3,
            serde_json::json!({
                "alert_rule": "prompt_cache_break",
                "alert_message": "Prompt cache break on turn 3: UnknownColdStart."
            }),
        );

        let turns = summarize_cache_turns(&[turn, feedback, alert]);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].turn, 3);
        assert_eq!(turns[0].fresh_prompt_tokens, 1200);
        assert_eq!(turns[0].cache_read_tokens, 800);
        assert_eq!(turns[0].cache_creation_tokens, 100);
        assert_eq!(turns[0].cache_hit_ratio, Some(0.38));
        assert_eq!(
            turns[0].cache_break_reason.as_deref(),
            Some("UnknownColdStart")
        );
        assert!(
            turns[0]
                .cache_alert_message
                .as_deref()
                .unwrap_or("")
                .contains("Prompt cache break")
        );
    }

    #[test]
    fn render_cache_summary_surfaces_scope_and_tip() {
        let turns = vec![CacheTurnSummary {
            turn: 2,
            fresh_prompt_tokens: 1000,
            completion_tokens: 100,
            cache_read_tokens: 3000,
            cache_creation_tokens: 200,
            ..CacheTurnSummary::default()
        }];
        let text = render_cache_summary(
            "sess-1",
            Some("kimi-k2.6"),
            Some(PromptCacheCapabilityData {
                protocol: astra_services::PromptCacheProtocolData::OpenAiAutoPrefix,
                volatile_placement: astra_services::PromptCacheVolatilePlacementData::TailSuffix,
                reuse_scope: Some(PromptCacheReuseScopeData::ConversationTurns),
            }),
            &turns,
            &[round(2, 0, 3000), round(2, 1, 4000)],
        );
        assert!(text.contains("declared reuse:     conversation_turns"));
        assert!(text.contains("observed reuse:     conversation_turns"));
        assert!(text.contains("/inspect cache"));
        assert!(text.contains("fresh=1000"));
        assert!(text.contains("read=3000"));
    }

    #[test]
    fn render_cache_summary_surfaces_declared_vs_observed_mismatch() {
        let text = render_cache_summary(
            "sess-2",
            Some("kimi-k2.6"),
            Some(PromptCacheCapabilityData {
                protocol: astra_services::PromptCacheProtocolData::OpenAiAutoPrefix,
                volatile_placement: astra_services::PromptCacheVolatilePlacementData::TailSuffix,
                reuse_scope: Some(PromptCacheReuseScopeData::ConversationTurns),
            }),
            &[],
            &[round(1, 1, 5000)],
        );
        assert!(text.contains("declared reuse:     conversation_turns"));
        assert!(text.contains("observed reuse:     intra_turn_rounds"));
        assert!(text.contains(
            "mismatch:           observed intra_turn_rounds < declared conversation_turns"
        ));
    }

    #[test]
    #[serial_test::serial]
    fn load_cache_events_surfaces_unreadable_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("cache-unreadable-{}", uuid::Uuid::new_v4());
        std::fs::create_dir_all(session_journal::journal_file_path(&session_id)).unwrap();

        let error = load_cache_events(&session_id)
            .expect_err("directory journal path should surface an error");

        assert!(error.contains("failed to read session journal"), "{error}");
    }
}
