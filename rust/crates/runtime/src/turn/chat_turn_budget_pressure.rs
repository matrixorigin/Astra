//! Compaction-tier budget pressure for `/chat` payload assembly (selector + schemas).

use serde_json::Value;

use crate::prompts;

/// Same pressure value used when building `SelectionContext` for tool selection.
#[must_use]
pub fn budget_pressure_for_chat_turn(
    messages: &[Value],
    model: Option<&str>,
    pinned_schema_tokens: usize,
) -> f64 {
    let estimated = prompts::estimate_tokens(messages, pinned_schema_tokens, 0);
    let budget = prompts::budget_for_model(model);
    let tier = budget.compaction_tier(estimated);
    tier.budget_pressure()
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

        let pressure_without_schema = budget_pressure_for_chat_turn(&messages, None, 0);
        let pressure_with_schema = budget_pressure_for_chat_turn(&messages, None, 50_000);

        assert!(
            pressure_with_schema > pressure_without_schema,
            "schema tokens must increase pressure: without={pressure_without_schema}, with={pressure_with_schema}"
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

        let pressure = budget_pressure_for_chat_turn(&messages, None, 50_000);

        assert!(
            pressure >= 0.3,
            "CJK-heavy session ({count} msgs) with schema tokens should reach at least TrimSchemas: got {pressure}",
            count = messages.len(),
        );
    }

    /// Messages that grow from 10 to 60 should monotonically increase pressure.
    #[test]
    fn pressure_grows_with_message_count() {
        let mut pressures = Vec::new();
        for count in [10, 20, 40, 60, 80] {
            let messages: Vec<_> = (0..count)
                .map(|i| {
                    json!({"role":"user","content": format!("请帮我分析和修复第{i}个代码问题")})
                })
                .collect();
            pressures.push(budget_pressure_for_chat_turn(&messages, None, 40_000));
        }

        for i in 1..pressures.len() {
            assert!(
                pressures[i] >= pressures[i - 1],
                "pressure must not decrease with more messages: index {i}: {} < {}",
                pressures[i],
                pressures[i - 1]
            );
        }
    }

    /// Empty messages + no schema = Normal tier (0.0)
    #[test]
    fn empty_messages_zero_schema_is_normal() {
        let p = budget_pressure_for_chat_turn(&[], None, 0);
        assert_eq!(p, 0.0);
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
