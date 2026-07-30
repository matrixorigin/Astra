use crate::cli::session::session_state::SessionState;
use astra_services::session_journal;
use crossterm::style::Stylize;

// ═══════════════════════════════════════════════════════ Stats ════════════

/// Retention: fallback handler for `/stats` — called from slash_router.rs.
/// In TUI mode this is shadowed by the native stats views.
/// Kept for headless / non-interactive execution paths.
pub(crate) async fn handle_stats_command(arg: &str, state: &SessionState) {
    use astra_services::session_analytics;

    match arg {
        // Consolidated subcommands from former standalone commands
        "tools" => super::slash_tools::handle_tools_command(state),
        sub if sub.starts_with("health") => {
            let rest = sub.strip_prefix("health").unwrap_or("").trim();
            super::slash_health::handle_health_command(rest, state).await;
        }
        sub if sub.starts_with("cost") => {
            let rest = sub.strip_prefix("cost").unwrap_or("").trim();
            handle_cost_command(rest, state);
        }
        "history" => {
            // Show stats across recent sessions
            let scan =
                match crate::cli::session::session_stats_scan::collect_recent_session_stats(10) {
                    Ok(scan) => scan,
                    Err(error) => {
                        eprintln!("  {}", error.red());
                        return;
                    }
                };
            if scan.stats.is_empty() && scan.unreadable.is_empty() {
                eprintln!("{}", "  No sessions found.".dim());
                return;
            }
            if scan.stats.is_empty() {
                eprintln!("{}", "  No readable session data.".dim());
                if !scan.unreadable.is_empty() {
                    eprintln!(
                        "  {}",
                        format!("Skipped {} unreadable journal(s).", scan.unreadable.len())
                            .yellow()
                    );
                }
                return;
            }
            eprintln!(
                "\n{}",
                "─── Recent Sessions ─────────────────────────────".bold()
            );
            for s in &scan.stats {
                let short = &s.session_id[..8.min(s.session_id.len())];
                let model = s.model.as_deref().unwrap_or("?");
                eprintln!(
                    "  {} {:>3} turns  {:>6}+{:<6} tok  {:>3} tools  {} err  {}",
                    short.magenta(),
                    s.turn_count,
                    s.total_tokens_in,
                    s.total_tokens_out,
                    s.total_tool_calls,
                    s.error_count,
                    model.dim(),
                );
            }
            let agg = session_analytics::aggregate_stats(&scan.stats);
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
            if agg.total_execution_boundaries_opened > 0
                || agg.total_execution_boundaries_committed > 0
                || agg.total_execution_boundaries_aborted > 0
            {
                eprintln!(
                    "  {:<14} {} opened, {} committed, {} aborted",
                    "boundaries:".dim(),
                    agg.total_execution_boundaries_opened,
                    agg.total_execution_boundaries_committed,
                    agg.total_execution_boundaries_aborted
                );
            }
            if !scan.unreadable.is_empty() {
                eprintln!(
                    "  {}",
                    format!("Skipped {} unreadable journal(s).", scan.unreadable.len()).yellow()
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
            let events =
                match crate::cli::session::session_stats_scan::read_session_journal_for_stats(&sid)
                {
                    Ok(events) => events,
                    Err(error) => {
                        eprintln!("  {}", error.red());
                        return;
                    }
                };
            let stats = session_analytics::compute_session_stats(&sid, &events);

            eprintln!(
                "\n{}",
                "─── Session Stats ───────────────────────────────".bold()
            );
            eprintln!(
                "  {:<14} {}",
                "session:".dim(),
                sid[..8.min(sid.len())].magenta()
            );
            if let Some(ref m) = stats.model {
                eprintln!("  {:<14} {}", "model:".dim(), m.as_str().magenta());
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
            if stats.execution_boundary_opened_count > 0
                || stats.execution_boundary_committed_count > 0
                || stats.execution_boundary_aborted_count > 0
            {
                eprintln!(
                    "  {:<14} {} opened, {} committed, {} aborted",
                    "boundaries:".dim(),
                    stats.execution_boundary_opened_count,
                    stats.execution_boundary_committed_count,
                    stats.execution_boundary_aborted_count
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
pub(crate) fn handle_cost_command(arg: &str, state: &SessionState) {
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
            let events =
                match crate::cli::session::session_stats_scan::read_session_journal_for_stats(&sid)
                {
                    Ok(events) => events,
                    Err(error) => {
                        eprintln!("  {}", error.red());
                        return;
                    }
                };
            let pricing = &state.cached_pricing;

            eprintln!(
                "\n{}",
                "─── Per-Turn Cost Breakdown ─────────────────────".bold()
            );
            if let Some(ref m) = state.model {
                eprintln!("  {:<14} {}", "model:".dim(), m.as_str().magenta());
            }
            eprintln!(
                "  {:<14} ${:.3}/1M prompt, ${:.3}/1M completion",
                "rates:".dim(),
                pricing.prompt * 1_000_000.0,
                pricing.completion * 1_000_000.0
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
            let scan =
                match crate::cli::session::session_stats_scan::collect_recent_session_stats(10) {
                    Ok(scan) => scan,
                    Err(error) => {
                        eprintln!("  {}", error.red());
                        return;
                    }
                };
            if scan.stats.is_empty() && scan.unreadable.is_empty() {
                eprintln!("{}", "  No sessions found.".dim());
                return;
            }
            if scan.stats.is_empty() {
                eprintln!("{}", "  No readable session data.".dim());
                if !scan.unreadable.is_empty() {
                    eprintln!(
                        "  {}",
                        format!("Skipped {} unreadable journal(s).", scan.unreadable.len())
                            .yellow()
                    );
                }
                return;
            }

            let pricing = &state.cached_pricing;

            eprintln!(
                "\n{}",
                "─── Session Cost History ────────────────────────".bold()
            );
            eprintln!(
                "  {:<14} ${:.3}/1M prompt, ${:.3}/1M completion",
                "rates:".dim(),
                pricing.prompt * 1_000_000.0,
                pricing.completion * 1_000_000.0
            );
            eprintln!();

            let mut grand_total = 0.0f64;

            for stats in &scan.stats {
                let cost = cost_for_tokens(
                    stats.total_tokens_in,
                    stats.total_tokens_out,
                    stats.total_cache_read,
                    stats.total_cache_creation,
                    pricing,
                );
                grand_total += cost;

                let short = &stats.session_id[..8.min(stats.session_id.len())];
                let model = stats.model.as_deref().unwrap_or("?");
                eprintln!(
                    "  {} {:>3} turns  {:>6}+{:<6} tok  {}  {}",
                    short.magenta(),
                    stats.turn_count,
                    stats.total_tokens_in,
                    stats.total_tokens_out,
                    format_cost(cost),
                    model.dim(),
                );
            }

            eprintln!(
                "\n  {} across {} sessions",
                format_cost(grand_total).bold(),
                scan.stats.len(),
            );
            if !scan.unreadable.is_empty() {
                eprintln!(
                    "  {}",
                    format!("Skipped {} unreadable journal(s).", scan.unreadable.len()).yellow()
                );
            }
            eprintln!();
        }

        _ => {
            // Current session summary
            let pricing = &state.cached_pricing;
            let cache_read_rate = pricing.cache_read.unwrap_or(pricing.prompt);
            let cache_write_rate = pricing.cache_write.unwrap_or(pricing.prompt);
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
                    sid[..8.min(sid.len())].magenta()
                );
            }
            if let Some(ref m) = state.model {
                eprintln!("  {:<14} {}", "model:".dim(), m.as_str().magenta());
            }
            eprintln!(
                "  {:<14} ${:.3}/1M prompt, ${:.3}/1M completion",
                "rates:".dim(),
                pricing.prompt * 1_000_000.0,
                pricing.completion * 1_000_000.0
            );
            eprintln!();
            eprintln!(
                "  {:<14} {} ({})",
                "prompt:".dim(),
                state.total_prompt_tokens,
                format_cost(state.total_prompt_tokens as f64 * pricing.prompt),
            );
            eprintln!(
                "  {:<14} {} ({})",
                "completion:".dim(),
                state.total_completion_tokens,
                format_cost(state.total_completion_tokens as f64 * pricing.completion),
            );
            if state.total_cache_read_tokens > 0 {
                eprintln!(
                    "  {:<14} {} ({})",
                    "cache read:".dim(),
                    state.total_cache_read_tokens,
                    format_cost(state.total_cache_read_tokens as f64 * cache_read_rate),
                );
            }
            if state.total_cache_creation_tokens > 0 {
                eprintln!(
                    "  {:<14} {} ({})",
                    "cache write:".dim(),
                    state.total_cache_creation_tokens,
                    format_cost(state.total_cache_creation_tokens as f64 * cache_write_rate),
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
                // Denominator = full billable input (fresh + cache-read + cache-creation)
                // so cache-creation-heavy sessions don't report misleadingly high hit
                // rates.
                let total_input = astra_turn_types::NormalizedPromptCacheUsage::new(
                    state.total_prompt_tokens,
                    state.total_cache_read_tokens,
                    state.total_cache_creation_tokens,
                )
                .total_input_tokens();
                let cache_pct =
                    state.total_cache_read_tokens as f64 / total_input.max(1) as f64 * 100.0;
                let saved =
                    state.total_cache_read_tokens as f64 * (pricing.prompt - cache_read_rate);
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
    debug_assert!(
        pricing.is_valid(),
        "CLI pricing must be validated at ingress"
    );
    pricing
        .estimated_cost_usd(
            prompt_tokens,
            completion_tokens,
            cache_read_tokens,
            cache_creation_tokens,
        )
        .unwrap_or(0.0)
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
///
/// Recognizes three shapes:
/// - `pricing: {...}` — full PricingData JSON (all fields optional except prompt/completion)
/// - `pricing_cache_read` / `pricing_cache_write` at top level — explicit cache rates
/// - `pricing_prompt` / `pricing_completion` only — base rates with no invented
///   cache discount; samples containing cache tokens remain unpriced.
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
            return serde_json::from_value(pricing.clone())
                .ok()
                .filter(astra_services::models::PricingData::is_valid);
        }
        // Top-level pricing_prompt / pricing_completion / pricing_cache_*
        let prompt = m
            .get("pricing_prompt")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let completion = m
            .get("pricing_completion")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if prompt > 0.0 || completion > 0.0 {
            let pricing = astra_services::models::PricingData {
                prompt,
                completion,
                cache_read: m.get("pricing_cache_read").and_then(|v| v.as_f64()),
                cache_write: m.get("pricing_cache_write").and_then(|v| v.as_f64()),
            };
            return pricing.is_valid().then_some(pricing);
        }
        return None;
    }
    None
}

/// Built-in pricing table for known models (USD per token).
/// Used when the API model list doesn't include pricing data.
/// Pricing from https://platform.claude.com/docs/en/about-claude/pricing
/// and https://openai.com/api/pricing/
pub(crate) fn fallback_pricing(model_name: &str) -> astra_services::models::PricingData {
    use astra_services::models::PricingData;
    let name = model_name.to_lowercase();

    // Claude Opus 4/4.1: $15/$75 per Mtok
    if name.contains("opus-4") && !name.contains("4.5") && !name.contains("4.6") {
        return PricingData {
            prompt: 0.000_015,
            completion: 0.000_075,
            cache_read: Some(0.000_001_5),
            cache_write: Some(0.000_018_75),
        };
    }
    // Claude Opus 4.5/4.6: $5/$25 per Mtok
    if name.contains("opus") {
        return PricingData {
            prompt: 0.000_005,
            completion: 0.000_025,
            cache_read: Some(0.000_000_5),
            cache_write: Some(0.000_006_25),
        };
    }
    // Claude Sonnet (3.5/3.7/4/4.5/4.6): $3/$15 per Mtok
    if name.contains("sonnet") {
        return PricingData {
            prompt: 0.000_003,
            completion: 0.000_015,
            cache_read: Some(0.000_000_3),
            cache_write: Some(0.000_003_75),
        };
    }
    // Claude Haiku 4.5: $1/$5 per Mtok
    if name.contains("haiku") && (name.contains("4.5") || name.contains("4-5")) {
        return PricingData {
            prompt: 0.000_001,
            completion: 0.000_005,
            cache_read: Some(0.000_000_1),
            cache_write: Some(0.000_001_25),
        };
    }
    // Claude Haiku 3.5: $0.80/$4 per Mtok
    if name.contains("haiku") {
        return PricingData {
            prompt: 0.000_000_8,
            completion: 0.000_004,
            cache_read: Some(0.000_000_08),
            cache_write: Some(0.000_001),
        };
    }
    // GPT-4o / GPT-4.1: $2.5/$10 per Mtok
    if name.contains("gpt-4o") || name.contains("gpt-4.1") {
        return PricingData {
            prompt: 0.000_002_5,
            completion: 0.000_01,
            cache_read: Some(0.000_000_625),
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
            prompt: 0.000_000_15,
            completion: 0.000_000_6,
            cache_read: Some(0.000_000_037_5),
            cache_write: None,
        };
    }
    // DeepSeek V3/R1: $0.27/$1.10 per Mtok (cache read $0.07)
    if name.contains("deepseek") {
        return PricingData {
            prompt: 0.000_000_27,
            completion: 0.000_001_1,
            cache_read: Some(0.000_000_07),
            cache_write: None,
        };
    }
    // Qwen (DashScope): cache reads ≈ 40% of input, no cache_write premium.
    // Per-Mtok varies widely by Qwen tier (qwen-plus, qwen-max, ...); leave
    // prompt/completion for the yaml to populate and supply only the cache
    // ratio so `extract_pricing_for_model` can blend it in.
    if name.contains("qwen") {
        return PricingData {
            prompt: 0.000_000_8,
            completion: 0.000_002,
            cache_read: Some(0.000_000_32),
            cache_write: None,
        };
    }
    // MiniMax: cache reads discounted, no cache_write premium.
    if name.contains("minimax") {
        return PricingData {
            prompt: 0.000_000_8,
            completion: 0.000_008,
            cache_read: Some(0.000_000_2),
            cache_write: None,
        };
    }
    // GLM / Zhipu: cache reads ~25% of input, no cache_write premium.
    if name.contains("glm") {
        return PricingData {
            prompt: 0.000_000_5,
            completion: 0.000_001_5,
            cache_read: Some(0.000_000_125),
            cache_write: None,
        };
    }
    // Kimi (Moonshot): cache reads ~25%, no cache_write premium.
    if name.contains("kimi") || name.contains("moonshot") {
        return PricingData {
            prompt: 0.000_003,
            completion: 0.000_015,
            cache_read: Some(0.000_000_75),
            cache_write: None,
        };
    }
    // Default: Sonnet pricing as safe fallback
    PricingData {
        prompt: 0.000_003,
        completion: 0.000_015,
        cache_read: Some(0.000_000_3),
        cache_write: Some(0.000_003_75),
    }
}
