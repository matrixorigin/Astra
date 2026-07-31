//! Compaction-tier budget pressure for `/chat` payload assembly (messages + schemas).

use serde_json::Value;

use crate::prompts;

const ABSOLUTE_TRIM_SCHEMA_TOKENS: usize = 128_000;
const ABSOLUTE_COMPACT_HISTORY_TOKENS: usize = 200_000;
const ABSOLUTE_AGGRESSIVE_PRUNE_TOKENS: usize = 320_000;

/// Same pressure value used when building `SelectionContext` for tool surface.
#[must_use]
#[cfg(test)]
fn budget_pressure_for_chat_turn(
    messages: &[Value],
    model: Option<&str>,
    always_load_schema_tokens: usize,
) -> f64 {
    let estimated = prompts::estimate_tokens(messages, always_load_schema_tokens, 0);
    let budget = prompts::budget_for_model(model);
    budget_pressure_for_estimate(estimated, &budget)
}

#[must_use]
pub fn budget_pressure_for_chat_turn_with_input_budget(
    messages: &[Value],
    always_load_schema_tokens: usize,
    effective_input_budget_tokens: u64,
) -> f64 {
    let estimated = prompts::estimate_tokens(messages, always_load_schema_tokens, 0);
    if effective_input_budget_tokens == 0 {
        return budget_pressure_for_estimate(estimated, &prompts::ContextBudget::default());
    }
    let budget = prompts::ContextBudget {
        model_limit: effective_input_budget_tokens.min(usize::MAX as u64) as usize,
        output_reserve_ratio: 0.0,
        ..Default::default()
    };
    budget_pressure_for_estimate(estimated, &budget)
}

#[must_use]
pub fn budget_pressure_for_chat_turn_with_context_window(
    messages: &[Value],
    always_load_schema_tokens: usize,
    context_window_tokens: u32,
) -> f64 {
    let estimated = prompts::estimate_tokens(messages, always_load_schema_tokens, 0);
    let budget = prompts::budget_for_model_with_override(None, Some(context_window_tokens));
    budget_pressure_for_estimate(estimated, &budget)
}

fn budget_pressure_for_estimate(estimated: usize, budget: &prompts::ContextBudget) -> f64 {
    let tier = budget.compaction_tier(estimated);
    tier.budget_pressure()
        .max(absolute_latency_pressure(estimated))
}

fn absolute_latency_pressure(estimated_tokens: usize) -> f64 {
    if estimated_tokens >= ABSOLUTE_AGGRESSIVE_PRUNE_TOKENS {
        0.9
    } else if estimated_tokens >= ABSOLUTE_COMPACT_HISTORY_TOKENS {
        0.6
    } else if estimated_tokens >= ABSOLUTE_TRIM_SCHEMA_TOKENS {
        0.3
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn budget_pressure_is_finite_for_minimal_messages() {
        let p = budget_pressure_for_chat_turn(&[json!({"role":"user","content":"hi"})], None, 0);
        assert!(p.is_finite());
    }

    /// Regression: without schema tokens, CJK-heavy conversations were
    /// underestimated by 50-60%, causing compaction to never trigger.
    /// Session 540c37d1: budget_pressure=0.887 but post_mc_pressure was
    /// ~0.61 because schema tokens were omitted from the estimate.
    #[test]
    fn cjk_with_schema_tokens_produces_higher_pressure() {
        // Simulate 40 messages of CJK conversation (user + assistant turns)
        let mut messages = Vec::new();
        for i in 0..20 {
            messages.push(json!({"role":"user","content": format!("第{i}个问题：请帮我分析一下这个问题应该怎么解决？")}));
            messages.push(json!({"role":"assistant","content": format!("好的，我来帮你分析第{i}个问题。根据代码审查的结果，问题出在以下几个方面...")}));
        }

        let pressure_without_schema =
            budget_pressure_for_chat_turn_with_input_budget(&messages, 0, 80_000);
        let pressure_with_schema =
            budget_pressure_for_chat_turn_with_input_budget(&messages, 50_000, 80_000);

        assert!(
            pressure_with_schema > pressure_without_schema,
            "schema tokens must increase pressure: without={pressure_without_schema}, with={pressure_with_schema}"
        );
        // 50K schema tokens in an 80K effective input budget produce at least a tier jump
        // (Normal→TrimSchemas = +0.3). Without schema the 40 CJK messages
        // alone land in Normal (0.0), so the delta must be meaningful.
        assert!(
            pressure_with_schema - pressure_without_schema >= 0.15,
            "50K schema tokens must increase pressure by at least 0.15: delta={}",
            pressure_with_schema - pressure_without_schema
        );
        // With schema tokens, must at least reach TrimSchemas (0.3).
        assert!(
            pressure_with_schema >= 0.3,
            "40 CJK messages + 50K schema tokens must reach at least TrimSchemas: got {pressure_with_schema}"
        );
    }

    /// Realistic scenario from session 540c37d1: ~130 messages (50+ turns
    /// with tool results), CJK content, ~50K tool schema tokens.
    /// Total estimated tokens should exceed ~81K (Compact trigger for
    /// default 128K model) and cross TrimSchemas (pressure >= 0.3).
    #[test]
    fn realistic_cjk_session_reaches_compact_pressure() {
        let mut messages = Vec::new();
        // Simulate a long conversation: 50 user turns + 50 assistant turns + 30 tool results
        for i in 0..50 {
            messages.push(json!({"role":"user","content": format!(
                "请继续分析第{i}个文件的问题，包括代码结构、错误处理、性能优化和安全性审查"
            )}));
            messages.push(json!({"role":"assistant","content": format!(
                "已分析第{i}个文件，发现以下问题：\n1. 错误处理不完善——缺少Result类型传播\n2. 性能瓶颈在第42行的嵌套循环中——建议使用HashMap索引\n3. 需要重构以提高可维护性——函数过长超过80行\n4. 安全风险：用户输入未做SQL注入防护\n5. 并发问题：Arc<Mutex<T>>可能导致死锁"
            )}));
            if i < 30 {
                // Simulated tool call results (file reads / greps / symbols)
                messages.push(json!({"role":"tool","content": format!(
                    "{{\"result\": \"文件 file_{i}.rs 分析完成：共 350 行，发现 5 个主要问题和 12 个优化建议\", \"success\": true}}"
                )}));
            }
        }

        let pressure = budget_pressure_for_chat_turn_with_input_budget(&messages, 50_000, 128_000);

        // At minimum must trigger TrimSchemas (0.3).
        assert!(
            pressure >= 0.3,
            "CJK-heavy session ({count} msgs) with schema tokens must reach at least TrimSchemas: got {pressure}",
            count = messages.len(),
        );

        // The continuous pressure estimate (ratio of estimated tokens to
        // effective input limit) must exceed 0.6 — this is the real
        // regression guard for session 540c37d1 where CJK underestimation
        // kept pressure artificially low.
        let (continuous_pressure, _) =
            crate::turn::agentic_loop::lifecycle::estimate_context_pressure(
                &messages, 50_000, 128_000,
            );
        assert!(
            continuous_pressure >= 0.6,
            "continuous CJK pressure must reach CompactHistory (≥0.6), got {continuous_pressure}"
        );
    }

    /// Messages that grow from 10 to 60 should monotonically increase pressure.
    #[test]
    fn pressure_grows_with_message_count() {
        let mut pressures = Vec::new();
        let mut continuous_pressures = Vec::new();
        for count in [10, 20, 40, 60, 80] {
            let messages: Vec<_> = (0..count)
                .map(|i| {
                    json!({"role":"user","content": format!("请帮我分析和修复第{i}个代码问题")})
                })
                .collect();
            pressures.push(budget_pressure_for_chat_turn(&messages, None, 40_000));
            let (cp, _) = crate::turn::agentic_loop::lifecycle::estimate_context_pressure(
                &messages, 0, 40_000,
            );
            continuous_pressures.push(cp);
        }

        for i in 1..pressures.len() {
            assert!(
                pressures[i] >= pressures[i - 1],
                "pressure must not decrease with more messages: index {i}: {} < {}",
                pressures[i],
                pressures[i - 1]
            );
        }
        // 80 CJK messages must produce noticeably higher continuous
        // pressure than 10.  Tiered pressure can stay flat within a band
        // (both in Normal=0.0), but the continuous estimator must grow.
        assert!(
            continuous_pressures[4] - continuous_pressures[0] >= 0.01,
            "80 CJK messages must be at least +0.01 continuous pressure over 10: delta={}",
            continuous_pressures[4] - continuous_pressures[0]
        );
    }

    /// Empty messages + no schema = Normal tier (0.0)
    #[test]
    fn empty_messages_zero_schema_is_normal() {
        let p = budget_pressure_for_chat_turn(&[], None, 0);
        assert_eq!(p, 0.0);
    }

    #[test]
    fn large_absolute_prompt_escalates_pressure_even_on_large_context_model() {
        assert_eq!(
            budget_pressure_for_chat_turn_with_input_budget(&[], 80_000, 800_000),
            0.0
        );
        assert!(budget_pressure_for_chat_turn_with_input_budget(&[], 128_000, 800_000) >= 0.3);
        assert!(budget_pressure_for_chat_turn_with_input_budget(&[], 200_000, 800_000) >= 0.6);
        assert!(budget_pressure_for_chat_turn_with_input_budget(&[], 320_000, 800_000) >= 0.9);
    }

    #[test]
    fn zero_effective_input_budget_falls_back_to_default_context_budget() {
        assert!(
            budget_pressure_for_chat_turn_with_input_budget(&[], 110_000, 0) >= 0.3,
            "legacy zero sentinel should use the default 200K context budget, not only absolute latency thresholds"
        );
    }

    #[test]
    fn context_window_path_applies_the_resolved_policy_before_classifying_pressure() {
        let trim_pressure = budget_pressure_for_chat_turn_with_context_window(&[], 90_000, 200_000);
        let compact_pressure =
            budget_pressure_for_chat_turn_with_context_window(&[], 115_000, 200_000);

        assert_eq!(trim_pressure, 0.3);
        assert_eq!(
            compact_pressure, 0.6,
            "the exact output, summary, and protocol reserves must all affect pressure"
        );
    }

    /// Even with many messages, if they're all short and no schema tokens,
    /// pressure should stay reasonable.
    #[test]
    fn short_ascii_messages_dont_spike_pressure() {
        let messages: Vec<_> = (0..100)
            .map(|i| json!({"role":"user","content": format!("msg{i}")}))
            .collect();
        let p = budget_pressure_for_chat_turn(&messages, Some("gpt-4o"), 0);
        assert!(
            p < 0.6,
            "100 short ASCII messages with no schema should be below CompactHistory threshold: {p}"
        );
    }
}
