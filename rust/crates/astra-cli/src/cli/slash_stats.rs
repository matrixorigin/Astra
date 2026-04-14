#![allow(unused_imports)]
use super::*;

// ═══════════════════════════════════════════════════════ Stats ════════════

pub(super) async fn handle_stats_command(arg: &str, state: &ReplState) {
    use astra_services::session_analytics;

    match arg {
        // Consolidated subcommands from former standalone commands
        "tools" => super::slash_tools::handle_tools_command(state),
        sub if sub.starts_with("health") => {
            let rest = sub.strip_prefix("health").unwrap_or("").trim();
            super::slash_health::handle_health_command(rest, state).await;
        }
        sub if sub.starts_with("learn") => {
            let rest = sub.strip_prefix("learn").unwrap_or("").trim();
            super::slash_learn::handle_learn_command(rest, state);
        }
        sub if sub.starts_with("cost") => {
            let rest = sub.strip_prefix("cost").unwrap_or("").trim();
            handle_cost_command(rest, state);
        }
        "history" => {
            // Show stats across recent sessions
            let sessions = match session_journal::list_sessions() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "{}",
                        format!("  ⚠ Could not read session history: {e}").yellow()
                    );
                    return;
                }
            };
            if sessions.is_empty() {
                eprintln!("{}", "  No sessions found.".dim());
                return;
            }
            let recent: Vec<_> = sessions.into_iter().take(10).collect();
            let mut all_stats = Vec::new();
            for sid in &recent {
                if let Ok(events) = session_journal::read_journal(sid) {
                    all_stats.push(session_analytics::compute_session_stats(sid, &events));
                }
            }
            if all_stats.is_empty() {
                eprintln!("{}", "  No session data.".dim());
                return;
            }
            eprintln!(
                "\n{}",
                "─── Recent Sessions ─────────────────────────────".bold()
            );
            for s in &all_stats {
                let short = &s.session_id[..8.min(s.session_id.len())];
                let model = s.model.as_deref().unwrap_or("?");
                eprintln!(
                    "  {} {:>3} turns  {:>6}+{:<6} tok  {:>3} tools  {} err  {}",
                    short.cyan(),
                    s.turn_count,
                    s.total_tokens_in,
                    s.total_tokens_out,
                    s.total_tool_calls,
                    s.error_count,
                    model.dim(),
                );
            }
            let agg = session_analytics::aggregate_stats(&all_stats);
            eprintln!(
                "\n  {} {} sessions, {} turns, {}+{} tokens, {:.1}% tool errors",
                "Summary:".bold(),
                agg.session_count,
                agg.total_turns,
                agg.total_tokens_in,
                agg.total_tokens_out,
                agg.overall_tool_error_rate * 100.0,
            );
            if agg.total_approval_required > 0
                || agg.total_approval_decisions > 0
                || agg.total_approval_timeouts > 0
            {
                eprintln!(
                    "  {:<14} {} required, {} decisions, {} timeouts",
                    "approvals:".dim(),
                    agg.total_approval_required,
                    agg.total_approval_decisions,
                    agg.total_approval_timeouts
                );
            }
            eprintln!();
        }
        _ => {
            // Show current session stats
            let sid = match &state.session_id {
                Some(s) => s.clone(),
                None => {
                    eprintln!("{}", "  No active session. Use /stats history.".dim());
                    return;
                }
            };
            let events = session_journal::read_journal(&sid).unwrap_or_default();
            let stats = session_analytics::compute_session_stats(&sid, &events);

            eprintln!(
                "\n{}",
                "─── Session Stats ───────────────────────────────".bold()
            );
            eprintln!(
                "  {:<14} {}",
                "session:".dim(),
                sid[..8.min(sid.len())].cyan()
            );
            if let Some(ref m) = stats.model {
                eprintln!("  {:<14} {}", "model:".dim(), m.as_str().cyan());
            }
            eprintln!("  {:<14} {}", "turns:".dim(), stats.turn_count);
            eprintln!(
                "  {:<14} {} in + {} out",
                "tokens:".dim(),
                stats.total_tokens_in,
                stats.total_tokens_out
            );
            eprintln!(
                "  {:<14} {:.1}s ({:.0}ms/turn)",
                "duration:".dim(),
                stats.total_duration_ms as f64 / 1000.0,
                stats.avg_duration_ms as f64
            );
            eprintln!(
                "  {:<14} {} ({} failed, {:.1}% error rate)",
                "tool calls:".dim(),
                stats.total_tool_calls,
                stats.failed_tool_calls,
                stats.tool_error_rate * 100.0
            );
            if !stats.unique_tools.is_empty() {
                eprintln!(
                    "  {:<14} {}",
                    "tools used:".dim(),
                    stats.unique_tools.join(", ")
                );
            }
            if stats.error_count > 0 || stats.stall_count > 0 {
                eprintln!(
                    "  {:<14} {} errors, {} stalls",
                    "issues:".dim(),
                    stats.error_count,
                    stats.stall_count
                );
            }
            if stats.checkpoint_count > 0 {
                eprintln!("  {:<14} {}", "checkpoints:".dim(), stats.checkpoint_count);
            }
            if stats.approval_required_count > 0
                || stats.approval_decision_count > 0
                || stats.approval_timeout_count > 0
            {
                eprintln!(
                    "  {:<14} {} required, {} decisions, {} timeouts",
                    "approvals:".dim(),
                    stats.approval_required_count,
                    stats.approval_decision_count,
                    stats.approval_timeout_count
                );
            }
            eprintln!(
                "\n  {}",
                "Subcommands: /stats history | tools | cost [detail|history] | health [detail] | learn [drift|explore]".dim()
            );
            eprintln!();
        }
    }
}

/// Per-turn cost record for granular cost breakdown.
#[derive(Clone, Debug)]
struct TurnCostEntry {
    turn: u32,
    prompt_tokens: u64,
    completion_tokens: u64,
    model: String,
}

/// Handle the `/cost` slash command — display per-session API cost estimates.
///
/// Subcommands:
///   /cost           — current session summary
///   /cost detail    — per-turn breakdown
///   /cost history   — across recent sessions
pub(super) fn handle_cost_command(arg: &str, state: &ReplState) {
    use astra_services::session_analytics;

    match arg {
        "detail" | "breakdown" => {
            // Per-turn breakdown from journal
            let sid = match &state.session_id {
                Some(s) => s.clone(),
                None => {
                    eprintln!("{}", "  No active session.".dim());
                    return;
                }
            };
            let events = session_journal::read_journal(&sid).unwrap_or_default();
            let pricing = &state.cached_pricing;

            eprintln!(
                "\n{}",
                "─── Per-Turn Cost Breakdown ─────────────────────".bold()
            );
            if let Some(ref m) = state.model {
                eprintln!("  {:<14} {}", "model:".dim(), m.as_str().cyan());
            }
            eprintln!(
                "  {:<14} ${:.4}/1k prompt, ${:.4}/1k completion",
                "rates:".dim(),
                pricing.prompt,
                pricing.completion
            );
            eprintln!();

            let mut total_in = 0u64;
            let mut total_out = 0u64;
            let mut total_cost = 0.0f64;
            let mut turn_num = 0u32;

            for ev in &events {
                if ev.event_type == session_journal::JournalEventType::Turn {
                    turn_num += 1;
                    let p_tok = ev.tokens_in.unwrap_or(0);
                    let c_tok = ev.tokens_out.unwrap_or(0);
                    let cr = ev.cache_read_tokens.unwrap_or(0);
                    let cw = ev.cache_creation_tokens.unwrap_or(0);
                    let cost = cost_for_tokens(p_tok, c_tok, cr, cw, pricing);
                    total_in += p_tok;
                    total_out += c_tok;
                    total_cost += cost;

                    let cache_info = if cr > 0 {
                        let pct = cr as f64 / (p_tok + cr).max(1) as f64 * 100.0;
                        format!("  cache:{pct:.0}%")
                    } else {
                        String::new()
                    };
                    eprintln!(
                        "  {} {:>6}+{:<6} tok  {}{}",
                        format!("Turn {:>3}", turn_num).dim(),
                        p_tok,
                        c_tok,
                        format_cost(cost),
                        cache_info.dim()
                    );
                }
            }

            eprintln!(
                "\n  {}",
                "─────────────────────────────────────────────────".dim()
            );
            eprintln!(
                "  {:<14} {}+{} tok  {}",
                "total:".bold(),
                total_in,
                total_out,
                format_cost(total_cost).bold(),
            );
            eprintln!();
        }

        "history" => {
            // Across recent sessions
            let sessions = match session_journal::list_sessions() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "{}",
                        format!("  ⚠ Could not read session history: {e}").yellow()
                    );
                    return;
                }
            };
            if sessions.is_empty() {
                eprintln!("{}", "  No sessions found.".dim());
                return;
            }

            let pricing = &state.cached_pricing;

            eprintln!(
                "\n{}",
                "─── Session Cost History ────────────────────────".bold()
            );
            eprintln!(
                "  {:<14} ${:.4}/1k prompt, ${:.4}/1k completion",
                "rates:".dim(),
                pricing.prompt,
                pricing.completion
            );
            eprintln!();

            let recent: Vec<_> = sessions.into_iter().take(10).collect();
            let mut grand_total = 0.0f64;

            for sid in &recent {
                if let Ok(events) = session_journal::read_journal(sid) {
                    let stats = session_analytics::compute_session_stats(sid, &events);
                    let cost = cost_for_tokens(
                        stats.total_tokens_in,
                        stats.total_tokens_out,
                        stats.total_cache_read,
                        stats.total_cache_creation,
                        pricing,
                    );
                    grand_total += cost;

                    let short = &sid[..8.min(sid.len())];
                    let model = stats.model.as_deref().unwrap_or("?");
                    eprintln!(
                        "  {} {:>3} turns  {:>6}+{:<6} tok  {}  {}",
                        short.cyan(),
                        stats.turn_count,
                        stats.total_tokens_in,
                        stats.total_tokens_out,
                        format_cost(cost),
                        model.dim(),
                    );
                }
            }

            eprintln!(
                "\n  {} across {} sessions",
                format_cost(grand_total).bold(),
                recent.len(),
            );
            eprintln!();
        }

        _ => {
            // Current session summary
            let pricing = &state.cached_pricing;
            let cache_read_rate = pricing.cache_read.unwrap_or(pricing.prompt * 0.1);
            let cache_write_rate = pricing.cache_write.unwrap_or(pricing.prompt * 1.25);
            let cost = cost_for_tokens(
                state.total_prompt_tokens,
                state.total_completion_tokens,
                state.total_cache_read_tokens,
                state.total_cache_creation_tokens,
                pricing,
            );

            eprintln!(
                "\n{}",
                "─── Session Cost ────────────────────────────────".bold()
            );
            if let Some(ref sid) = state.session_id {
                eprintln!(
                    "  {:<14} {}",
                    "session:".dim(),
                    sid[..8.min(sid.len())].cyan()
                );
            }
            if let Some(ref m) = state.model {
                eprintln!("  {:<14} {}", "model:".dim(), m.as_str().cyan());
            }
            eprintln!(
                "  {:<14} ${:.4}/1k prompt, ${:.4}/1k completion",
                "rates:".dim(),
                pricing.prompt,
                pricing.completion
            );
            eprintln!();
            eprintln!(
                "  {:<14} {} ({})",
                "prompt:".dim(),
                state.total_prompt_tokens,
                format_cost(state.total_prompt_tokens as f64 * pricing.prompt / 1000.0),
            );
            eprintln!(
                "  {:<14} {} ({})",
                "completion:".dim(),
                state.total_completion_tokens,
                format_cost(state.total_completion_tokens as f64 * pricing.completion / 1000.0),
            );
            if state.total_cache_read_tokens > 0 {
                eprintln!(
                    "  {:<14} {} ({})",
                    "cache read:".dim(),
                    state.total_cache_read_tokens,
                    format_cost(state.total_cache_read_tokens as f64 * cache_read_rate / 1000.0),
                );
            }
            if state.total_cache_creation_tokens > 0 {
                eprintln!(
                    "  {:<14} {} ({})",
                    "cache write:".dim(),
                    state.total_cache_creation_tokens,
                    format_cost(
                        state.total_cache_creation_tokens as f64 * cache_write_rate / 1000.0
                    ),
                );
            }
            eprintln!("  {:<14} {}", "total:".bold(), format_cost(cost).bold());
            if state.turn > 0 {
                eprintln!(
                    "  {:<14} {} per turn",
                    "avg:".dim(),
                    format_cost(cost / state.turn as f64)
                );
            }
            if state.total_cache_read_tokens > 0 {
                let total_input = state.total_prompt_tokens + state.total_cache_read_tokens;
                let cache_pct =
                    state.total_cache_read_tokens as f64 / total_input.max(1) as f64 * 100.0;
                let saved = state.total_cache_read_tokens as f64
                    * (pricing.prompt - cache_read_rate)
                    / 1000.0;
                eprintln!(
                    "  {:<14} {:.0}% cache hit, {} saved",
                    "savings:".dim(),
                    cache_pct,
                    format_cost(saved),
                );
            }
            eprintln!(
                "\n  {}",
                "Use /cost detail for per-turn breakdown, /cost history for past sessions.".dim()
            );
            eprintln!();
        }
    }
}

/// Calculate cost in dollars for given token counts.
pub(crate) fn cost_for_tokens(
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    pricing: &astra_services::models::PricingData,
) -> f64 {
    let cache_read_rate = pricing.cache_read.unwrap_or(pricing.prompt * 0.1);
    let cache_write_rate = pricing.cache_write.unwrap_or(pricing.prompt * 1.25);
    (prompt_tokens as f64 * pricing.prompt / 1000.0)
        + (completion_tokens as f64 * pricing.completion / 1000.0)
        + (cache_read_tokens as f64 * cache_read_rate / 1000.0)
        + (cache_creation_tokens as f64 * cache_write_rate / 1000.0)
}

/// Format a dollar cost for display.
pub(crate) fn format_cost(cost: f64) -> String {
    if cost < 0.01 {
        format!("${:.4}", cost)
    } else if cost < 1.0 {
        format!("${:.3}", cost)
    } else {
        format!("${:.2}", cost)
    }
}

/// Extract pricing data for a model from the API models list.
pub(crate) fn extract_pricing_for_model(
    models: &[serde_json::Value],
    model_name: &str,
) -> Option<astra_services::models::PricingData> {
    for m in models {
        let name = m
            .get("name")
            .or_else(|| m.get("model_name"))
            .and_then(|v| v.as_str())?;
        if name != model_name {
            continue;
        }
        if let Some(pricing) = m.get("pricing") {
            return serde_json::from_value(pricing.clone()).ok();
        }
        // Fallback: top-level pricing_prompt / pricing_completion fields
        let prompt = m
            .get("pricing_prompt")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let completion = m
            .get("pricing_completion")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if prompt > 0.0 || completion > 0.0 {
            return Some(astra_services::models::PricingData {
                prompt,
                completion,
                cache_read: None,
                cache_write: None,
            });
        }
        return None;
    }
    None
}

/// Built-in pricing table for known models ($/Ktok).
/// Used when the API model list doesn't include pricing data.
/// Pricing from https://platform.claude.com/docs/en/about-claude/pricing
/// and https://openai.com/api/pricing/
pub(crate) fn fallback_pricing(model_name: &str) -> astra_services::models::PricingData {
    use astra_services::models::PricingData;
    let name = model_name.to_lowercase();

    // Claude Opus 4/4.1: $15/$75 per Mtok
    if name.contains("opus-4") && !name.contains("4.5") && !name.contains("4.6") {
        return PricingData {
            prompt: 0.015,
            completion: 0.075,
            cache_read: Some(0.0015),
            cache_write: Some(0.01875),
        };
    }
    // Claude Opus 4.5/4.6: $5/$25 per Mtok
    if name.contains("opus") {
        return PricingData {
            prompt: 0.005,
            completion: 0.025,
            cache_read: Some(0.0005),
            cache_write: Some(0.00625),
        };
    }
    // Claude Sonnet (3.5/3.7/4/4.5/4.6): $3/$15 per Mtok
    if name.contains("sonnet") {
        return PricingData {
            prompt: 0.003,
            completion: 0.015,
            cache_read: Some(0.0003),
            cache_write: Some(0.00375),
        };
    }
    // Claude Haiku 4.5: $1/$5 per Mtok
    if name.contains("haiku") && (name.contains("4.5") || name.contains("4-5")) {
        return PricingData {
            prompt: 0.001,
            completion: 0.005,
            cache_read: Some(0.0001),
            cache_write: Some(0.00125),
        };
    }
    // Claude Haiku 3.5: $0.80/$4 per Mtok
    if name.contains("haiku") {
        return PricingData {
            prompt: 0.0008,
            completion: 0.004,
            cache_read: Some(0.00008),
            cache_write: Some(0.001),
        };
    }
    // GPT-4o / GPT-4.1: $2.5/$10 per Mtok
    if name.contains("gpt-4o") || name.contains("gpt-4.1") {
        return PricingData {
            prompt: 0.0025,
            completion: 0.01,
            cache_read: Some(0.000625),
            cache_write: None,
        };
    }
    // GPT-4o-mini / GPT-4.1-mini: $0.15/$0.60 per Mtok
    if name.contains("4o-mini")
        || name.contains("4.1-mini")
        || name.contains("5-mini")
        || name.contains("5.4-mini")
    {
        return PricingData {
            prompt: 0.00015,
            completion: 0.0006,
            cache_read: Some(0.0000375),
            cache_write: None,
        };
    }
    // DeepSeek V3/R1: $0.27/$1.10 per Mtok (cache read $0.07)
    if name.contains("deepseek") {
        return PricingData {
            prompt: 0.00027,
            completion: 0.0011,
            cache_read: Some(0.00007),
            cache_write: None,
        };
    }
    // Default: Sonnet pricing as safe fallback
    PricingData {
        prompt: 0.003,
        completion: 0.015,
        cache_read: Some(0.0003),
        cache_write: Some(0.00375),
    }
}
