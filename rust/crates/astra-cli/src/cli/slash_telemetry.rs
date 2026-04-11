use super::*;

/// Handle `/telemetry` command — display observability session metrics.
///
/// Subcommands:
/// - (no arg)     Show summary: turns, timings, drift, decisions
/// - `turns`      List per-turn timing breakdowns
/// - `drift`      Check focus drift analysis
/// - `decisions`  List tool selection decisions with confidence
pub(super) fn handle_telemetry_command(arg: &str, state: &ReplState) {
    let (sub_cmd, sub_arg) = match arg.find(char::is_whitespace) {
        Some(pos) => (arg[..pos].trim(), arg[pos..].trim()),
        None => (arg.trim(), ""),
    };

    // Check if observability is active
    let (hub, session) = match (&state.observability_hub, &state.observability_session) {
        (Some(h), Some(s)) => (h, s),
        (Some(_), None) => {
            eprintln!(
                "{}",
                "  No active observability session. Start a conversation first.".yellow()
            );
            return;
        }
        (None, _) => {
            eprintln!("{}", "  Observability hub not initialized.".yellow());
            return;
        }
    };

    match sub_cmd {
        "" => show_summary(hub, session, state),
        "turns" => show_turn_timings(session),
        "drift" => show_drift_analysis(session, state),
        "decisions" => show_decisions(session),
        "profile" => show_user_profile(hub, state),
        "context" => show_context_trace(session, sub_arg),
        "context-detail" => show_context_detail(session, sub_arg),
        "session" => show_session_analysis(session),
        "tools" => show_tool_trace(session, sub_arg),
        "compression" => show_compression_trace(session, sub_arg),
        "budget" => show_budget_evolution(session),
        "help" | "-h" | "--help" => show_help(),
        _ => {
            eprintln!(
                "{}",
                format!("  Unknown subcommand: {sub_cmd}. Try /telemetry help").yellow()
            );
        }
    }
}

fn show_summary(
    hub: &std::sync::Arc<astra_runtime::observability_integration::ObservabilityHub>,
    session: &std::sync::Arc<
        std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>,
    >,
    state: &ReplState,
) {
    use astra_runtime::observability_integration::FuzzyMatchOutcome;

    let session_guard = session.read().unwrap_or_else(|e| e.into_inner());

    eprintln!(
        "\n{}",
        "─── Observability Session ───────────────────────"
            .bold()
            .cyan()
    );

    // Session info
    eprintln!(
        "  {:<18} {}",
        "session_id:".dim(),
        state.session_id.as_deref().unwrap_or("none").cyan()
    );
    eprintln!(
        "  {:<18} {}",
        "user_id:".dim(),
        session_guard.user_id.clone().cyan()
    );

    // Timing
    let duration = session_guard.duration();
    let duration_str = if duration.as_secs() >= 60 {
        format!(
            "{}m{:.0}s",
            duration.as_secs() / 60,
            duration.as_secs() % 60
        )
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    };
    eprintln!("  {:<18} {}", "duration:".dim(), duration_str.cyan());

    // Turn count
    eprintln!(
        "  {:<18} {}",
        "turns_tracked:".dim(),
        session_guard.turn_number.to_string().cyan()
    );

    // Turn timings summary
    if !session_guard.turn_timings.is_empty() {
        let total_ms: u64 = session_guard.turn_timings.iter().map(|t| t.total_ms).sum();
        let avg_ms = total_ms / session_guard.turn_timings.len() as u64;
        let llm_ms: u64 = session_guard.turn_timings.iter().map(|t| t.ttft_ms).sum();
        let tool_ms: u64 = session_guard
            .turn_timings
            .iter()
            .map(|t| t.tool_execution_ms)
            .sum();
        eprintln!(
            "  {:<18} {}ms avg ({} total)",
            "turn_time:".dim(),
            avg_ms.to_string().cyan(),
            format!("{}ms", total_ms).dim()
        );
        eprintln!(
            "  {:<18} {}ms LLM · {}ms tools",
            "breakdown:".dim(),
            llm_ms.to_string().cyan(),
            tool_ms.to_string().cyan()
        );
    }

    eprintln!();
    eprintln!("  {}", "— Tracking Stats —".dim());

    // Context traces
    eprintln!(
        "  {:<18} {}",
        "context_traces:".dim(),
        session_guard.context_traces.len().to_string().cyan()
    );

    // Decisions
    eprintln!(
        "  {:<18} {}",
        "decisions:".dim(),
        session_guard.decision_explanations.len().to_string().cyan()
    );

    // Recent queries for drift
    eprintln!(
        "  {:<18} {}",
        "queries_tracked:".dim(),
        session_guard.recent_queries.len().to_string().cyan()
    );

    if !session_guard.fuzzy_match_events.is_empty() {
        let matched = session_guard
            .fuzzy_match_events
            .iter()
            .filter(|event| event.outcome == FuzzyMatchOutcome::Matched)
            .count();
        let ambiguous = session_guard
            .fuzzy_match_events
            .iter()
            .filter(|event| event.outcome == FuzzyMatchOutcome::Ambiguous)
            .count();
        let not_found = session_guard
            .fuzzy_match_events
            .iter()
            .filter(|event| event.outcome == FuzzyMatchOutcome::NotFound)
            .count();
        let mut by_strategy = std::collections::BTreeMap::<String, usize>::new();
        for event in session_guard
            .fuzzy_match_events
            .iter()
            .filter(|event| event.outcome == FuzzyMatchOutcome::Matched)
        {
            *by_strategy.entry(event.strategy.clone()).or_default() += 1;
        }

        eprintln!(
            "  {:<18} {}",
            "fuzzy_events:".dim(),
            session_guard.fuzzy_match_events.len().to_string().cyan()
        );
        eprintln!(
            "  {:<18} {} matched · {} ambiguous · {} misses",
            "fuzzy_stats:".dim(),
            matched.to_string().cyan(),
            ambiguous.to_string().yellow(),
            not_found.to_string().red()
        );
        if !by_strategy.is_empty() {
            let strategies = by_strategy
                .into_iter()
                .map(|(strategy, count)| format!("{strategy}:{count}"))
                .collect::<Vec<_>>()
                .join(", ");
            eprintln!("  {:<18} {}", "fuzzy_by_strategy:".dim(), strategies.cyan());
        }
    }

    // Active experiment
    if let Some(ref variant) = session_guard.active_variant {
        eprintln!(
            "  {:<18} {} ({})",
            "experiment:".dim(),
            variant.clone().cyan(),
            session_guard
                .active_experiment_id
                .as_deref()
                .unwrap_or("?")
                .dim()
        );
    }

    // Scenario
    if let Some(scenario) = session_guard.current_scenario() {
        eprintln!("  {:<18} {:?}", "detected_scenario:".dim(), scenario);
    }

    // User profile stats from hub
    let profile = hub.profiles().get_profile(&session_guard.user_id);
    if profile.stats.total_queries > 0 || profile.stats.total_tool_calls > 0 {
        eprintln!();
        eprintln!("  {}", "— User Profile —".dim());
        eprintln!(
            "  {:<18} {}",
            "total_queries:".dim(),
            profile.stats.total_queries.to_string().cyan()
        );
        eprintln!(
            "  {:<18} {}",
            "total_tool_calls:".dim(),
            profile.stats.total_tool_calls.to_string().cyan()
        );
        eprintln!(
            "  {:<18} {}",
            "avg_session_secs:".dim(),
            format!("{:.1}", profile.stats.avg_session_duration_secs).cyan()
        );
    }

    eprintln!();
    eprintln!(
        "  {}",
        "Use /telemetry turns|drift|decisions|profile for details".dim()
    );
    eprintln!();
}

fn show_turn_timings(
    session: &std::sync::Arc<
        std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>,
    >,
) {
    let session_guard = session.read().unwrap_or_else(|e| e.into_inner());

    if session_guard.turn_timings.is_empty() {
        eprintln!("{}", "  No turn timing data yet.".yellow());
        return;
    }

    eprintln!(
        "\n{}",
        "─── Turn Timings ────────────────────────────────"
            .bold()
            .cyan()
    );
    eprintln!(
        "  {:<6} {:>10} {:>10} {:>10} {:>10}",
        "Turn".dim(),
        "Total".dim(),
        "Context".dim(),
        "LLM".dim(),
        "Tools".dim()
    );
    eprintln!("  {}", "─".repeat(50).dim());

    for timing in &session_guard.turn_timings {
        let total_str = format!("{}ms", timing.total_ms);
        let ctx_str = if timing.context_assembly_ms > 0 {
            format!("{}ms", timing.context_assembly_ms)
        } else {
            "—".to_string()
        };
        let llm_str = format!("{}ms", timing.ttft_ms);
        let tool_str = if timing.tool_execution_ms > 0 {
            format!("{}ms", timing.tool_execution_ms)
        } else {
            "—".to_string()
        };

        eprintln!(
            "  {:<6} {:>10} {:>10} {:>10} {:>10}",
            format!("T{}", timing.turn).cyan(),
            total_str,
            ctx_str,
            llm_str,
            tool_str
        );
    }
    eprintln!();
}

fn show_drift_analysis(
    session: &std::sync::Arc<
        std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>,
    >,
    state: &ReplState,
) {
    let session_guard = session.read().unwrap_or_else(|e| e.into_inner());

    eprintln!(
        "\n{}",
        "─── Focus Drift Analysis ────────────────────────"
            .bold()
            .cyan()
    );

    if session_guard.recent_queries.is_empty() {
        eprintln!("{}", "  No queries tracked yet.".yellow());
        return;
    }

    // Show recent queries
    eprintln!("  {}", "Recent queries:".dim());
    let show_count = session_guard.recent_queries.len().min(5);
    for (i, q) in session_guard
        .recent_queries
        .iter()
        .rev()
        .take(show_count)
        .enumerate()
    {
        let preview: String = q.chars().take(60).collect();
        let suffix = if q.len() > 60 { "…" } else { "" };
        eprintln!(
            "    {} {}{}",
            format!("[{}]", show_count - i).dim(),
            preview,
            suffix
        );
    }
    eprintln!();

    // Analyze drift from session goal
    if state.session_goal.is_some() {
        let analysis = session_guard.check_drift();
        let severity_color = if analysis.drift_severity > 0.7 {
            "red"
        } else if analysis.drift_severity > 0.4 {
            "yellow"
        } else {
            "green"
        };
        let severity_str = format!("{:.1}%", analysis.drift_severity * 100.0);
        eprintln!(
            "  {:<18} {}",
            "drift_severity:".dim(),
            match severity_color {
                "red" => severity_str.red().to_string(),
                "yellow" => severity_str.yellow().to_string(),
                _ => severity_str.green().to_string(),
            }
        );
        if let Some(turn) = analysis.drift_turn {
            eprintln!("  {:<18} {}", "drift_turn:".dim(), turn);
        }
        eprintln!(
            "  {:<18} {:?}",
            "likely_cause:".dim(),
            analysis.likely_cause
        );
        if !analysis.affected_context.is_empty() {
            eprintln!("  {:<18}", "affected_context:".dim());
            for ctx in analysis.affected_context.iter().take(3) {
                eprintln!("    • {}", ctx);
            }
        }
        eprintln!(
            "  {:<18} {}",
            "recovery:".dim(),
            analysis.recovery_suggestion
        );
    } else {
        eprintln!(
            "  {}",
            "No session goal set — drift analysis requires a baseline.".dim()
        );
    }
    eprintln!();
}

fn show_decisions(
    session: &std::sync::Arc<
        std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>,
    >,
) {
    let session_guard = session.read().unwrap_or_else(|e| e.into_inner());

    if session_guard.decision_explanations.is_empty() {
        eprintln!("{}", "  No decision data yet.".yellow());
        return;
    }

    eprintln!(
        "\n{}",
        "─── Decision History ────────────────────────────"
            .bold()
            .cyan()
    );

    for (i, decision) in session_guard.decision_explanations.iter().enumerate() {
        let conf_str = format!("{:.0}%", decision.confidence * 100.0);
        let conf_color = if decision.confidence > 0.7 {
            conf_str.green()
        } else if decision.confidence > 0.4 {
            conf_str.yellow()
        } else {
            conf_str.red()
        };

        // Format decision type nicely
        use astra_runtime::turn::decision_explainer::DecisionType;
        let type_label = match &decision.decision_type {
            DecisionType::ToolSelection {
                selected_tools,
                total_available,
            } => {
                format!(
                    "ToolSelection ({}/{})",
                    selected_tools.len(),
                    total_available
                )
            }
            DecisionType::HistoryCompression {
                turns_compressed,
                turns_retained,
                compression_ratio,
            } => {
                format!(
                    "HistoryCompression (-{}/+{}, {:.0}%)",
                    turns_compressed.len(),
                    turns_retained.len(),
                    compression_ratio * 100.0
                )
            }
            DecisionType::MemoryRetrieval {
                memories_selected,
                total_candidates,
            } => {
                format!(
                    "MemoryRetrieval ({}/{})",
                    memories_selected.len(),
                    total_candidates
                )
            }
            DecisionType::StrategyChoice { strategy, .. } => {
                format!("StrategyChoice: {}", strategy)
            }
            DecisionType::ModelRouting { selected_model, .. } => {
                format!("ModelRouting → {}", selected_model)
            }
        };

        eprintln!(
            "  {} {} — {} confidence",
            format!("[{}]", i + 1).dim(),
            type_label.cyan(),
            conf_color
        );

        // Inputs summary
        if !decision.inputs.is_empty() {
            let inputs_count = decision.inputs.len();
            eprintln!("      {}: {} input(s)", "inputs".dim(), inputs_count);
        }

        // Reasoning preview
        let reasoning_preview: String = decision.reasoning.chars().take(80).collect();
        let suffix = if decision.reasoning.len() > 80 {
            "…"
        } else {
            ""
        };
        eprintln!("      {}: {}{}", "reason".dim(), reasoning_preview, suffix);
    }
    eprintln!();
}

fn show_user_profile(
    hub: &std::sync::Arc<astra_runtime::observability_integration::ObservabilityHub>,
    state: &ReplState,
) {
    let user_id = state.ingestion_user_id.as_deref().unwrap_or("anonymous");
    let profile = hub.profiles().get_profile(user_id);

    eprintln!(
        "\n{}",
        "─── User Profile ────────────────────────────────"
            .bold()
            .cyan()
    );
    eprintln!("  {:<20} {}", "user_id:".dim(), user_id.cyan());
    eprintln!();
    eprintln!("  {}", "— Statistics —".dim());
    eprintln!(
        "  {:<20} {}",
        "total_queries:".dim(),
        profile.stats.total_queries.to_string().cyan()
    );
    eprintln!(
        "  {:<20} {}",
        "total_tool_calls:".dim(),
        profile.stats.total_tool_calls.to_string().cyan()
    );
    eprintln!(
        "  {:<20} {}",
        "avg_session_secs:".dim(),
        format!("{:.1}", profile.stats.avg_session_duration_secs).cyan()
    );

    // Top tools
    if !profile.stats.tool_usage.is_empty() {
        eprintln!();
        eprintln!("  {}", "— Top Tools —".dim());
        let mut tools: Vec<_> = profile.stats.tool_usage.iter().collect();
        tools.sort_by(|a, b| b.1.cmp(a.1));
        for (tool, count) in tools.iter().take(5) {
            eprintln!("    {} {}", format!("{:>4}×", count).cyan(), tool);
        }
    }

    // Preferences
    eprintln!();
    eprintln!("  {}", "— Preferences —".dim());
    eprintln!(
        "  {:<20} {:?}",
        "verbosity:".dim(),
        profile.preferences.verbosity
    );
    eprintln!(
        "  {:<20} {:?}",
        "language_style:".dim(),
        profile.preferences.language_style
    );
    eprintln!(
        "  {:<20} {:?}",
        "response_length:".dim(),
        profile.preferences.response_length
    );

    // Current scenario
    if let Some(scenario) = profile.current_scenario {
        eprintln!();
        eprintln!("  {:<20} {:?}", "detected_scenario:".dim(), scenario);
    }

    eprintln!();
}

// ─── Help ────────────────────────────────────────────────────────────────────

fn show_help() {
    eprintln!(
        "\n{}",
        "─── Telemetry Commands ──────────────────────────"
            .bold()
            .cyan()
    );
    eprintln!("  {}          Show session summary", "/telemetry".cyan());
    eprintln!(
        "  {}    List per-turn timing breakdowns",
        "/telemetry turns".cyan()
    );
    eprintln!(
        "  {}    Check focus drift analysis",
        "/telemetry drift".cyan()
    );
    eprintln!(
        "  {}  List tool selection decisions",
        "/telemetry decisions".cyan()
    );
    eprintln!(
        "  {}  Show user profile/preferences",
        "/telemetry profile".cyan()
    );
    eprintln!();
    eprintln!("  {}", "── Deep Trace (per-turn detail) ──".bold().cyan());
    eprintln!(
        "  {}  Context assembly for turn N",
        "/telemetry context [N]".cyan()
    );
    eprintln!(
        "  {}  Hierarchical proportional analysis",
        "/telemetry context-detail [N]".cyan()
    );
    eprintln!(
        "  {}    Tool selection scoring for turn N",
        "/telemetry tools [N]".cyan()
    );
    eprintln!(
        "  {}  History compression for turn N",
        "/telemetry compression [N]".cyan()
    );
    eprintln!("  {}   Token budget evolution", "/telemetry budget".cyan());
    eprintln!();
    eprintln!("  {}", "── Session-Level Analysis ──".bold().cyan());
    eprintln!(
        "  {}  Multi-turn context evolution",
        "/telemetry session".cyan()
    );
    eprintln!();
    eprintln!(
        "  {}",
        "Omit N to show the latest turn. Use -1 for the last turn.".dim()
    );
    eprintln!();
}

// ─── Deep Trace: Context Assembly ────────────────────────────────────────────

fn show_context_trace(
    session: &std::sync::Arc<
        std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>,
    >,
    arg: &str,
) {
    let session_guard = session.read().unwrap_or_else(|e| e.into_inner());
    let traces = &session_guard.context_traces;

    if traces.is_empty() {
        eprintln!("{}", "  No context assembly traces yet.".yellow());
        return;
    }

    let trace = match resolve_turn_index(arg, traces.len()) {
        Some(idx) => &traces[idx],
        None => {
            eprintln!(
                "{}",
                format!(
                    "  Invalid turn: '{}'. Available: 1–{} or -1 for latest.",
                    arg,
                    traces.len()
                )
                .yellow()
            );
            return;
        }
    };

    eprintln!(
        "\n{}",
        format!(
            "─── Context Assembly — Turn {} ──────────────────",
            trace.turn_id
        )
        .bold()
        .cyan()
    );

    // ── System Prompt Breakdown ──
    let sp = &trace.system_prompt;
    eprintln!();
    eprintln!("  {}", "▸ System Prompt".bold());
    eprintln!(
        "    {:<22} {} tokens",
        "base_persona:".dim(),
        sp.base_persona_tokens.to_string().cyan()
    );
    eprintln!(
        "    {:<22} {} tokens",
        "environment:".dim(),
        sp.environment_tokens.to_string().cyan()
    );
    eprintln!(
        "    {:<22} {} tokens",
        "user_preferences:".dim(),
        sp.user_preferences_tokens.to_string().cyan()
    );
    if !sp.skills_injected.is_empty() {
        eprintln!(
            "    {:<22} {} skills",
            "skills:".dim(),
            sp.skills_injected.len().to_string().cyan()
        );
        for sk in &sp.skills_injected {
            let ver = sk
                .skill_version
                .as_deref()
                .map(|v| format!(" v{v}"))
                .unwrap_or_default();
            eprintln!(
                "      {} {}{} ({} tok) — {}",
                "•".dim(),
                sk.skill_name.clone().cyan(),
                ver.dim(),
                sk.tokens,
                sk.selection_reason.clone().dim()
            );
        }
    }
    if !sp.repository_memories.is_empty() {
        eprintln!(
            "    {:<22} {} memories",
            "repo_memories:".dim(),
            sp.repository_memories.len().to_string().cyan()
        );
        for mem in &sp.repository_memories {
            let preview: String = mem.content_preview.chars().take(50).collect();
            let suffix = if mem.content_preview.len() > 50 {
                "…"
            } else {
                ""
            };
            eprintln!(
                "      {} [{:.2}] {} ({} tok) {}{}",
                "•".dim(),
                mem.relevance_score,
                mem.memory_type.clone().dim(),
                mem.tokens,
                preview,
                suffix
            );
        }
    }
    eprintln!(
        "    {:<22} {} tokens",
        "total:".bold().dim(),
        sp.total_tokens.to_string().cyan().bold()
    );

    // ── Memory Retrieval ──
    let mem = &trace.memory;
    eprintln!();
    eprintln!("  {}", "▸ Memory Retrieval".bold());
    if mem.query.is_empty() && mem.candidates_considered == 0 {
        eprintln!("    {}", "(no retrieval performed)".dim());
    } else {
        let query_preview: String = mem.query.chars().take(60).collect();
        let suffix = if mem.query.len() > 60 { "…" } else { "" };
        eprintln!("    {:<22} {}{}", "query:".dim(), query_preview, suffix);
        eprintln!(
            "    {:<22} {} considered → {} selected ({} tok, {}ms)",
            "results:".dim(),
            mem.candidates_considered.to_string().cyan(),
            mem.memories_selected.len().to_string().green(),
            mem.total_tokens,
            mem.retrieval_latency_ms
        );

        if !mem.memories_selected.is_empty() {
            eprintln!("    {}", "selected:".dim());
            for m in &mem.memories_selected {
                let preview: String = m.content_preview.chars().take(45).collect();
                let suffix = if m.content_preview.len() > 45 {
                    "…"
                } else {
                    ""
                };
                eprintln!(
                    "      {} [{:.2}] {:?} {} tok — {}{}",
                    "✓".green(),
                    m.relevance_score,
                    m.source,
                    m.tokens,
                    preview,
                    suffix
                );
            }
        }
        if !mem.memories_rejected.is_empty() {
            let show_n = mem.memories_rejected.len().min(5);
            eprintln!(
                "    {} (showing {}/{})",
                "rejected:".dim(),
                show_n,
                mem.memories_rejected.len()
            );
            for m in mem.memories_rejected.iter().take(show_n) {
                let reason = format_rejection_reason(&m.rejection_reason);
                eprintln!(
                    "      {} [{:.2}] {} — {}",
                    "✗".red(),
                    m.relevance_score,
                    m.memory_id.clone().dim(),
                    reason
                );
            }
        }
    }

    // ── Token Budget ──
    let tb = &trace.token_budget;
    eprintln!();
    eprintln!("  {}", "▸ Token Budget".bold());
    let pressure_str = format!("{:.0}%", tb.budget_pressure * 100.0);
    let pressure_colored = if tb.budget_pressure > 0.9 {
        pressure_str.red().to_string()
    } else if tb.budget_pressure > 0.7 {
        pressure_str.yellow().to_string()
    } else {
        pressure_str.green().to_string()
    };
    eprintln!(
        "    {:<22} {} / {} ({} pressure{})",
        "usage:".dim(),
        tb.total_used.to_string().cyan(),
        tb.max_tokens.to_string().dim(),
        pressure_colored,
        if tb.compression_triggered {
            ", compression triggered"
        } else {
            ""
        }
    );

    // Token allocation bar chart
    let components = [
        ("system_prompt", tb.system_prompt_tokens),
        ("history", tb.history_tokens),
        ("memory", tb.memory_tokens),
        ("tool_schemas", tb.tool_schema_tokens),
        ("user_message", tb.user_message_tokens),
    ];
    let max_tok = components.iter().map(|(_, t)| *t).max().unwrap_or(1).max(1);
    let bar_max = 30;
    for (label, tokens) in &components {
        let bar_len = (*tokens as usize * bar_max) / max_tok as usize;
        let bar: String = "█".repeat(bar_len);
        let pct = if tb.total_used > 0 {
            (*tokens as f64 / tb.total_used as f64 * 100.0) as u32
        } else {
            0
        };
        eprintln!(
            "    {:<22} {:>6} ({:>2}%) {}",
            format!("{label}:").dim(),
            tokens.to_string().cyan(),
            pct,
            bar.dim()
        );
    }

    // ── Decisions ──
    if !trace.explanations.is_empty() {
        eprintln!();
        eprintln!("  {}", "▸ Decisions".bold());
        for exp in &trace.explanations {
            let type_label = format_trace_decision_type(&exp.decision_type);
            let conf_str = format!("{:.0}%", exp.confidence * 100.0);
            let conf = if exp.confidence > 0.7 {
                conf_str.green()
            } else if exp.confidence > 0.4 {
                conf_str.yellow()
            } else {
                conf_str.red()
            };
            eprintln!(
                "    {} {} — {} confidence",
                "•".dim(),
                type_label.cyan(),
                conf
            );
            let reasoning: String = exp.reasoning.chars().take(80).collect();
            let suffix = if exp.reasoning.len() > 80 { "…" } else { "" };
            eprintln!("      {}{}", reasoning.dim(), suffix);
            for alt in exp.alternatives_considered.iter().take(2) {
                eprintln!(
                    "      {} {} — {}",
                    "↳".dim(),
                    alt.description.clone().dim(),
                    alt.why_not_chosen.clone().dim()
                );
            }
        }
    }

    eprintln!();
}

// ─── Deep Trace: Tool Selection ──────────────────────────────────────────────

fn show_tool_trace(
    session: &std::sync::Arc<
        std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>,
    >,
    arg: &str,
) {
    let session_guard = session.read().unwrap_or_else(|e| e.into_inner());
    let traces = &session_guard.context_traces;

    if traces.is_empty() {
        eprintln!("{}", "  No context assembly traces yet.".yellow());
        return;
    }

    let trace = match resolve_turn_index(arg, traces.len()) {
        Some(idx) => &traces[idx],
        None => {
            eprintln!(
                "{}",
                format!(
                    "  Invalid turn: '{}'. Available: 1–{} or -1 for latest.",
                    arg,
                    traces.len()
                )
                .yellow()
            );
            return;
        }
    };

    let ts = &trace.tools;

    eprintln!(
        "\n{}",
        format!(
            "─── Tool Selection — Turn {} ────────────────────",
            trace.turn_id
        )
        .bold()
        .cyan()
    );

    eprintln!(
        "  {:<22} {}",
        "strategy:".dim(),
        ts.selection_strategy.clone().cyan()
    );
    let conf_str = format!("{:.0}%", ts.selection_confidence * 100.0);
    let conf = if ts.selection_confidence > 0.7 {
        conf_str.green()
    } else if ts.selection_confidence > 0.4 {
        conf_str.yellow()
    } else {
        conf_str.red()
    };
    eprintln!("  {:<22} {}", "confidence:".dim(), conf);
    eprintln!(
        "  {:<22} {} available → {} selected ({}ms)",
        "selection:".dim(),
        ts.tools_available.to_string().dim(),
        ts.tools_selected.len().to_string().green(),
        ts.selection_latency_ms
    );

    if !ts.tools_selected.is_empty() {
        eprintln!();
        eprintln!("  {}", "▸ Selected Tools".bold());
        eprintln!(
            "    {:<24} {:>6} {:>6}  {}",
            "Tool".dim(),
            "Score".dim(),
            "Tokens".dim(),
            "Selection Factors".dim()
        );
        eprintln!("    {}", "─".repeat(65).dim());

        for tool in &ts.tools_selected {
            let factors: String = tool
                .selection_factors
                .iter()
                .map(|f| format!("{}:{:.1}", f.factor_name, f.contribution))
                .collect::<Vec<_>>()
                .join(", ");
            eprintln!(
                "    {:<24} {:>5.2} {:>6}  {}",
                tool.tool_name.clone().green(),
                tool.score,
                tool.tokens,
                if factors.is_empty() {
                    "—".to_string()
                } else {
                    factors.dim().to_string()
                }
            );
        }
    }

    if !ts.tools_rejected.is_empty() {
        let show_n = ts.tools_rejected.len().min(10);
        eprintln!();
        eprintln!(
            "  {} (showing {}/{})",
            "▸ Rejected Tools".bold(),
            show_n,
            ts.tools_rejected.len()
        );
        eprintln!(
            "    {:<24} {:>6}  {}",
            "Tool".dim(),
            "Score".dim(),
            "Reason".dim()
        );
        eprintln!("    {}", "─".repeat(55).dim());
        for tool in ts.tools_rejected.iter().take(show_n) {
            eprintln!(
                "    {:<24} {:>5.2}  {}",
                tool.tool_name.clone().red(),
                tool.score,
                tool.rejection_reason.clone().dim()
            );
        }
    }

    eprintln!();
}

// ─── Deep Trace: History Compression ─────────────────────────────────────────

fn show_compression_trace(
    session: &std::sync::Arc<
        std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>,
    >,
    arg: &str,
) {
    let session_guard = session.read().unwrap_or_else(|e| e.into_inner());
    let traces = &session_guard.context_traces;

    if traces.is_empty() {
        eprintln!("{}", "  No context assembly traces yet.".yellow());
        return;
    }

    let trace = match resolve_turn_index(arg, traces.len()) {
        Some(idx) => &traces[idx],
        None => {
            eprintln!(
                "{}",
                format!(
                    "  Invalid turn: '{}'. Available: 1–{} or -1 for latest.",
                    arg,
                    traces.len()
                )
                .yellow()
            );
            return;
        }
    };

    let h = &trace.history;

    eprintln!(
        "\n{}",
        format!(
            "─── History Compression — Turn {} ───────────────",
            trace.turn_id
        )
        .bold()
        .cyan()
    );

    eprintln!(
        "  {:<22} {}",
        "turns_available:".dim(),
        h.total_turns_available.to_string().cyan()
    );
    let ratio_str = format!("{:.0}%", h.compression_ratio * 100.0);
    let ratio = if h.compression_ratio > 0.5 {
        ratio_str.red()
    } else if h.compression_ratio > 0.2 {
        ratio_str.yellow()
    } else {
        ratio_str.green()
    };
    eprintln!("  {:<22} {}", "compression_ratio:".dim(), ratio);
    let saved = if h.tokens_before > h.tokens_after {
        h.tokens_before - h.tokens_after
    } else {
        0
    };
    eprintln!(
        "  {:<22} {} → {} ({} saved)",
        "tokens:".dim(),
        h.tokens_before.to_string().dim(),
        h.tokens_after.to_string().cyan(),
        saved.to_string().green()
    );
    eprintln!(
        "  {:<22} {} retained · {} compressed · {} dropped",
        "breakdown:".dim(),
        h.turns_retained.len().to_string().green(),
        h.turns_compressed.len().to_string().yellow(),
        h.turns_dropped.len().to_string().red()
    );

    // Retained turns
    if !h.turns_retained.is_empty() {
        eprintln!();
        eprintln!("  {}", "▸ Retained".bold());
        for t in &h.turns_retained {
            let tool_flag = if t.has_tool_calls { " 🔧" } else { "" };
            eprintln!(
                "    {} T{:<3} {:>6} tok  {}{}",
                "✓".green(),
                t.turn_index,
                t.tokens,
                t.role.clone().dim(),
                tool_flag
            );
        }
    }

    // Compressed turns
    if !h.turns_compressed.is_empty() {
        eprintln!();
        eprintln!("  {}", "▸ Compressed".bold());
        eprintln!(
            "    {:<6} {:>8} {:>8}  {:>5}  {:<24}  {}",
            "Turn".dim(),
            "Before".dim(),
            "After".dim(),
            "Saved".dim(),
            "Method".dim(),
            "Info Lost".dim()
        );
        eprintln!("    {}", "─".repeat(72).dim());
        for t in &h.turns_compressed {
            let saved = if t.original_tokens > t.compressed_tokens {
                t.original_tokens - t.compressed_tokens
            } else {
                0
            };
            let method = format_compression_method(&t.compression_method);
            let info_lost = if t.information_lost.is_empty() {
                "—".to_string()
            } else {
                t.information_lost
                    .iter()
                    .take(2)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            let info_preview: String = info_lost.chars().take(30).collect();
            let suffix = if info_lost.len() > 30 { "…" } else { "" };
            eprintln!(
                "    T{:<4} {:>7} {:>7}  {:>5}  {:<24}  {}{}",
                t.turn_index,
                t.original_tokens,
                t.compressed_tokens,
                saved,
                method.yellow(),
                info_preview.dim(),
                suffix
            );
        }
    }

    // Dropped turns
    if !h.turns_dropped.is_empty() {
        eprintln!();
        eprintln!("  {}", "▸ Dropped".bold());
        let turns_str: String = h
            .turns_dropped
            .iter()
            .map(|t| format!("T{t}"))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("    {} {}", "✗".red(), turns_str.dim());
    }

    eprintln!();
}

// ─── Deep Trace: Token Budget Evolution ──────────────────────────────────────

fn show_budget_evolution(
    session: &std::sync::Arc<
        std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>,
    >,
) {
    let session_guard = session.read().unwrap_or_else(|e| e.into_inner());
    let traces = &session_guard.context_traces;

    if traces.is_empty() {
        eprintln!("{}", "  No context assembly traces yet.".yellow());
        return;
    }

    eprintln!(
        "\n{}",
        "─── Token Budget Evolution ─────────────────────"
            .bold()
            .cyan()
    );

    // Table header
    eprintln!(
        "  {:<6} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>8}",
        "Turn".dim(),
        "System".dim(),
        "History".dim(),
        "Memory".dim(),
        "Tools".dim(),
        "User".dim(),
        "Total".dim(),
        "Pressure".dim()
    );
    eprintln!("  {}", "─".repeat(66).dim());

    for (i, trace) in traces.iter().enumerate() {
        let tb = &trace.token_budget;
        let pressure_str = format!("{:.0}%", tb.budget_pressure * 100.0);
        let pressure = if tb.budget_pressure > 0.9 {
            pressure_str.red()
        } else if tb.budget_pressure > 0.7 {
            pressure_str.yellow()
        } else {
            pressure_str.green()
        };
        let compress_flag = if tb.compression_triggered { " ⚠" } else { "" };
        eprintln!(
            "  {:<6} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>8}{}",
            format!("T{}", i + 1).cyan(),
            tb.system_prompt_tokens,
            tb.history_tokens,
            tb.memory_tokens,
            tb.tool_schema_tokens,
            tb.user_message_tokens,
            tb.total_used,
            pressure,
            compress_flag
        );
    }

    // Summary: trends
    if traces.len() >= 2 {
        let first = &traces[0].token_budget;
        let last = &traces[traces.len() - 1].token_budget;
        eprintln!();
        eprintln!("  {}", "▸ Trends".bold());

        let history_trend = last.history_tokens as i64 - first.history_tokens as i64;
        let trend_arrow = if history_trend > 0 { "↑" } else { "↓" };
        let trend_str = format!("{}{} tokens", trend_arrow, history_trend.unsigned_abs());
        let trend_colored = if history_trend > 0 {
            trend_str.yellow()
        } else {
            trend_str.green()
        };
        eprintln!(
            "    {:<22} {} (T1→T{})",
            "history:".dim(),
            trend_colored,
            traces.len()
        );

        let pressure_trend = last.budget_pressure - first.budget_pressure;
        let p_arrow = if pressure_trend > 0.0 { "↑" } else { "↓" };
        let p_str = format!("{}{:.0}%", p_arrow, pressure_trend.abs() * 100.0);
        let p_colored = if pressure_trend > 0.0 {
            p_str.yellow()
        } else {
            p_str.green()
        };
        eprintln!("    {:<22} {}", "budget_pressure:".dim(), p_colored);

        let compress_count = traces
            .iter()
            .filter(|t| t.token_budget.compression_triggered)
            .count();
        if compress_count > 0 {
            eprintln!(
                "    {:<22} {} of {} turns",
                "compression_events:".dim(),
                compress_count.to_string().yellow(),
                traces.len()
            );
        }
    }

    eprintln!();
}

// ─── Deep Trace: Context Detail (Hierarchical Proportional) ──────────────────

fn show_context_detail(
    session: &std::sync::Arc<
        std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>,
    >,
    arg: &str,
) {
    let session_guard = session.read().unwrap_or_else(|e| e.into_inner());
    let traces = &session_guard.context_traces;

    if traces.is_empty() {
        eprintln!("{}", "  No context assembly traces yet.".yellow());
        return;
    }

    let trace = match resolve_turn_index(arg, traces.len()) {
        Some(idx) => &traces[idx],
        None => {
            eprintln!(
                "{}",
                format!(
                    "  Invalid turn: '{}'. Available: 1–{} or -1 for latest.",
                    arg,
                    traces.len()
                )
                .yellow()
            );
            return;
        }
    };

    let tb = &trace.token_budget;
    let total = tb.total_used.max(1) as f64;

    eprintln!(
        "\n{}",
        format!(
            "─── Context Detail — Turn {} ───────────────────",
            trace.turn_id
        )
        .bold()
        .cyan()
    );
    eprintln!(
        "  {} {} / {} tokens  (pressure: {})",
        "Total:".bold(),
        tb.total_used.to_string().cyan().bold(),
        tb.max_tokens.to_string().dim(),
        format_pressure(tb.budget_pressure)
    );

    // ── System Prompt (hierarchical) ──
    let sp = &trace.system_prompt;
    let sp_pct = tb.system_prompt_tokens as f64 / total * 100.0;
    eprintln!();
    eprintln!(
        "  {} {} tokens ({:.1}%)",
        "▸ System Prompt".bold(),
        tb.system_prompt_tokens.to_string().cyan(),
        sp_pct
    );
    eprintln!("  {}", proportional_bar(sp_pct, 50));

    // Sub-components of system prompt
    let sp_total = sp.total_tokens.max(1) as f64;
    let sp_items: Vec<(&str, u32)> = {
        let mut items = vec![
            ("base_persona", sp.base_persona_tokens),
            ("environment", sp.environment_tokens),
            ("user_preferences", sp.user_preferences_tokens),
        ];
        let skills_total: u32 = sp.skills_injected.iter().map(|s| s.tokens).sum();
        if skills_total > 0 {
            items.push(("skills", skills_total));
        }
        let mem_total: u32 = sp.repository_memories.iter().map(|m| m.tokens).sum();
        if mem_total > 0 {
            items.push(("repo_memories", mem_total));
        }
        items
    };

    for (label, tokens) in &sp_items {
        let sub_pct = *tokens as f64 / sp_total * 100.0;
        let global_pct = *tokens as f64 / total * 100.0;
        eprintln!(
            "    {:<20} {:>6} tok  {:>5.1}% of system  {:>5.1}% of total  {}",
            format!("{label}:").dim(),
            tokens.to_string().cyan(),
            sub_pct,
            global_pct,
            mini_bar(sub_pct, 20)
        );
    }

    // Individual skills
    if !sp.skills_injected.is_empty() {
        for sk in &sp.skills_injected {
            let sk_pct = sk.tokens as f64 / sp_total * 100.0;
            let ver = sk
                .skill_version
                .as_deref()
                .map(|v| format!(" v{v}"))
                .unwrap_or_default();
            eprintln!(
                "      {} {}{} — {} tok ({:.1}%)",
                "•".dim(),
                sk.skill_name.clone().cyan(),
                ver.dim(),
                sk.tokens,
                sk_pct
            );
        }
    }

    // Individual repo memories
    if !sp.repository_memories.is_empty() {
        for mem in &sp.repository_memories {
            let m_pct = mem.tokens as f64 / sp_total * 100.0;
            let preview: String = mem.content_preview.chars().take(40).collect();
            let suffix = if mem.content_preview.len() > 40 {
                "…"
            } else {
                ""
            };
            eprintln!(
                "      {} [{:.2}] {} tok ({:.1}%) {}{}",
                "•".dim(),
                mem.relevance_score,
                mem.tokens,
                m_pct,
                preview,
                suffix
            );
        }
    }

    // ── History ──
    let hist_pct = tb.history_tokens as f64 / total * 100.0;
    let hist = &trace.history;
    eprintln!();
    eprintln!(
        "  {} {} tokens ({:.1}%)",
        "▸ History".bold(),
        tb.history_tokens.to_string().cyan(),
        hist_pct
    );
    eprintln!("  {}", proportional_bar(hist_pct, 50));

    if hist.tokens_before > 0 {
        eprintln!(
            "    {:<20} {} → {} tokens ({:.0}% compression)",
            "compression:".dim(),
            hist.tokens_before.to_string().dim(),
            hist.tokens_after.to_string().cyan(),
            hist.compression_ratio * 100.0
        );
    }
    eprintln!(
        "    {:<20} {} retained, {} compressed, {} dropped",
        "turns:".dim(),
        hist.turns_retained.len().to_string().green(),
        hist.turns_compressed.len().to_string().yellow(),
        hist.turns_dropped.len().to_string().red()
    );

    // Show retained turns breakdown
    if !hist.turns_retained.is_empty() {
        let retained_total: u32 = hist.turns_retained.iter().map(|t| t.tokens).sum();
        eprintln!(
            "    {:<20} {} tokens across {} turns",
            "retained:".dim(),
            retained_total.to_string().cyan(),
            hist.turns_retained.len()
        );
        for t in hist.turns_retained.iter().take(5) {
            let tc_flag = if t.has_tool_calls { " ⚙" } else { "" };
            let t_pct = t.tokens as f64 / total * 100.0;
            eprintln!(
                "      T{:<3} {:>5} {} tok ({:.1}%){tc_flag}",
                t.turn_index,
                t.role.clone().dim(),
                t.tokens,
                t_pct
            );
        }
        if hist.turns_retained.len() > 5 {
            eprintln!(
                "      {} … and {} more",
                "".dim(),
                hist.turns_retained.len() - 5
            );
        }
    }

    // Show compressed turns
    if !hist.turns_compressed.is_empty() {
        let saved: u32 = hist
            .turns_compressed
            .iter()
            .map(|t| t.original_tokens.saturating_sub(t.compressed_tokens))
            .sum();
        eprintln!(
            "    {:<20} {} tokens saved",
            "compressed:".dim(),
            saved.to_string().yellow()
        );
        for t in hist.turns_compressed.iter().take(3) {
            eprintln!(
                "      T{:<3} {} → {} tok ({}) {}",
                t.turn_index,
                t.original_tokens.to_string().dim(),
                t.compressed_tokens.to_string().cyan(),
                format_compression_method(&t.compression_method),
                t.information_lost
                    .first()
                    .map(|s| {
                        let preview: String = s.chars().take(30).collect();
                        format!("lost: {preview}…")
                    })
                    .unwrap_or_default()
                    .dim()
            );
        }
    }

    // ── Memory Retrieval ──
    let mem_pct = tb.memory_tokens as f64 / total * 100.0;
    let mem = &trace.memory;
    eprintln!();
    eprintln!(
        "  {} {} tokens ({:.1}%)",
        "▸ Memory".bold(),
        tb.memory_tokens.to_string().cyan(),
        mem_pct
    );
    eprintln!("  {}", proportional_bar(mem_pct, 50));

    if mem.candidates_considered > 0 {
        eprintln!(
            "    {:<20} {} considered → {} selected ({}ms)",
            "retrieval:".dim(),
            mem.candidates_considered.to_string().dim(),
            mem.memories_selected.len().to_string().green(),
            mem.retrieval_latency_ms
        );
        for m in &mem.memories_selected {
            let m_local_pct = if tb.memory_tokens > 0 {
                m.tokens as f64 / tb.memory_tokens as f64 * 100.0
            } else {
                0.0
            };
            let preview: String = m.content_preview.chars().take(35).collect();
            let suffix = if m.content_preview.len() > 35 {
                "…"
            } else {
                ""
            };
            eprintln!(
                "      {} [{:.2}] {:>4} tok ({:>4.1}%) {:?} {}{}",
                "✓".green(),
                m.relevance_score,
                m.tokens,
                m_local_pct,
                m.source,
                preview,
                suffix
            );
        }
    } else {
        eprintln!("    {}", "(no retrieval performed)".dim());
    }

    // ── Tool Schemas ──
    let tool_pct = tb.tool_schema_tokens as f64 / total * 100.0;
    let ts = &trace.tools;
    eprintln!();
    eprintln!(
        "  {} {} tokens ({:.1}%)",
        "▸ Tool Schemas".bold(),
        tb.tool_schema_tokens.to_string().cyan(),
        tool_pct
    );
    eprintln!("  {}", proportional_bar(tool_pct, 50));

    eprintln!(
        "    {:<20} {} available → {} selected ({})",
        "selection:".dim(),
        ts.tools_available.to_string().dim(),
        ts.tools_selected.len().to_string().green(),
        ts.selection_strategy.clone().dim()
    );

    if !ts.tools_selected.is_empty() {
        for tool in &ts.tools_selected {
            let t_local_pct = if tb.tool_schema_tokens > 0 {
                tool.tokens as f64 / tb.tool_schema_tokens as f64 * 100.0
            } else {
                0.0
            };
            eprintln!(
                "      {:<22} {:>4} tok ({:>4.1}%)  score: {:.2}",
                tool.tool_name.clone().cyan(),
                tool.tokens,
                t_local_pct,
                tool.score
            );
        }
    }

    // ── User Message ──
    let user_pct = tb.user_message_tokens as f64 / total * 100.0;
    eprintln!();
    eprintln!(
        "  {} {} tokens ({:.1}%)",
        "▸ User Message".bold(),
        tb.user_message_tokens.to_string().cyan(),
        user_pct
    );
    eprintln!("  {}", proportional_bar(user_pct, 50));

    // ── Overall Proportion Summary ──
    eprintln!();
    eprintln!("  {}", "▸ Proportion Summary".bold());
    let components = [
        ("System Prompt", tb.system_prompt_tokens, sp_pct),
        ("History", tb.history_tokens, hist_pct),
        ("Memory", tb.memory_tokens, mem_pct),
        ("Tool Schemas", tb.tool_schema_tokens, tool_pct),
        ("User Message", tb.user_message_tokens, user_pct),
    ];
    for (label, tokens, pct) in &components {
        eprintln!(
            "    {:<18} {:>6} tok  {:>5.1}%  {}",
            format!("{label}:").dim(),
            tokens.to_string().cyan(),
            pct,
            proportional_bar(*pct, 30)
        );
    }

    eprintln!();
}

// ─── Session-Level Analysis ──────────────────────────────────────────────────

fn show_session_analysis(
    session: &std::sync::Arc<
        std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>,
    >,
) {
    use astra_runtime::turn::context_assembly_trace::TraceAggregation;

    let session_guard = session.read().unwrap_or_else(|e| e.into_inner());
    let traces = &session_guard.context_traces;

    if traces.is_empty() {
        eprintln!("{}", "  No context assembly traces yet.".yellow());
        return;
    }

    let agg = TraceAggregation::from_traces(traces);

    eprintln!(
        "\n{}",
        "─── Session Context Analysis ───────────────────"
            .bold()
            .cyan()
    );
    eprintln!(
        "  {} {} turns analyzed",
        "Turns:".bold(),
        traces.len().to_string().cyan()
    );

    // ── Per-Turn Timeline ──
    eprintln!();
    eprintln!("  {}", "▸ Per-Turn Timeline".bold());
    eprintln!(
        "    {:<6} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>8} {}",
        "Turn".dim(),
        "System".dim(),
        "History".dim(),
        "Memory".dim(),
        "Tools".dim(),
        "User".dim(),
        "Total".dim(),
        "Pressure".dim(),
        "".dim()
    );
    eprintln!("    {}", "─".repeat(74).dim());

    for (i, trace) in traces.iter().enumerate() {
        let tb = &trace.token_budget;
        let compress_flag = if tb.compression_triggered {
            " ⚠".yellow().to_string()
        } else {
            String::new()
        };
        eprintln!(
            "    {:<6} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>8}{}",
            format!("T{}", i + 1).cyan(),
            tb.system_prompt_tokens,
            tb.history_tokens,
            tb.memory_tokens,
            tb.tool_schema_tokens,
            tb.user_message_tokens,
            tb.total_used,
            format_pressure(tb.budget_pressure),
            compress_flag
        );
    }

    // ── Component Proportion Shift (sparkline-style) ──
    if traces.len() >= 2 {
        eprintln!();
        eprintln!("  {}", "▸ Component Proportion Shift".bold());

        let component_extractors: Vec<(
            &str,
            Box<dyn Fn(&astra_runtime::turn::context_assembly_trace::ContextAssemblyTrace) -> f64>,
        )> = vec![
            (
                "system_prompt",
                Box::new(
                    |t: &astra_runtime::turn::context_assembly_trace::ContextAssemblyTrace| {
                        let total = t.token_budget.total_used.max(1) as f64;
                        t.token_budget.system_prompt_tokens as f64 / total * 100.0
                    },
                ),
            ),
            (
                "history",
                Box::new(
                    |t: &astra_runtime::turn::context_assembly_trace::ContextAssemblyTrace| {
                        let total = t.token_budget.total_used.max(1) as f64;
                        t.token_budget.history_tokens as f64 / total * 100.0
                    },
                ),
            ),
            (
                "memory",
                Box::new(
                    |t: &astra_runtime::turn::context_assembly_trace::ContextAssemblyTrace| {
                        let total = t.token_budget.total_used.max(1) as f64;
                        t.token_budget.memory_tokens as f64 / total * 100.0
                    },
                ),
            ),
            (
                "tools",
                Box::new(
                    |t: &astra_runtime::turn::context_assembly_trace::ContextAssemblyTrace| {
                        let total = t.token_budget.total_used.max(1) as f64;
                        t.token_budget.tool_schema_tokens as f64 / total * 100.0
                    },
                ),
            ),
            (
                "user_msg",
                Box::new(
                    |t: &astra_runtime::turn::context_assembly_trace::ContextAssemblyTrace| {
                        let total = t.token_budget.total_used.max(1) as f64;
                        t.token_budget.user_message_tokens as f64 / total * 100.0
                    },
                ),
            ),
        ];

        for (label, extractor) in &component_extractors {
            let values: Vec<f64> = traces.iter().map(|t| extractor(t)).collect();
            let first = values[0];
            let last = *values.last().unwrap();
            let delta = last - first;
            let trend = if delta.abs() < 0.5 {
                "→".dim().to_string()
            } else if delta > 0.0 {
                format!("↑{:.0}%", delta).yellow().to_string()
            } else {
                format!("↓{:.0}%", delta.abs()).green().to_string()
            };
            eprintln!(
                "    {:<16} {} {}",
                format!("{label}:").dim(),
                ascii_sparkline(&values, 20),
                trend
            );
        }
    }

    // ── Aggregated Averages ──
    eprintln!();
    eprintln!("  {}", "▸ Averages".bold());
    eprintln!(
        "    {:<24} {:.0} tokens",
        "system_prompt:".dim(),
        agg.avg_system_prompt_tokens
    );
    eprintln!(
        "    {:<24} {:.0} tokens",
        "history:".dim(),
        agg.avg_history_tokens
    );
    eprintln!(
        "    {:<24} {:.0} tokens",
        "memory:".dim(),
        agg.avg_memory_tokens
    );
    eprintln!(
        "    {:<24} {:.0} tokens",
        "tool_schemas:".dim(),
        agg.avg_tool_schema_tokens
    );
    eprintln!(
        "    {:<24} {:.1} memories (avg relevance: {:.2})",
        "memory_selection:".dim(),
        agg.avg_memories_selected,
        agg.avg_memory_relevance
    );
    eprintln!(
        "    {:<24} {:.1} tools (avg confidence: {:.0}%)",
        "tool_selection:".dim(),
        agg.avg_tools_selected,
        agg.avg_selection_confidence * 100.0
    );

    // ── Peak / Min ──
    eprintln!();
    eprintln!("  {}", "▸ Peak & Min".bold());
    let peak_total = traces
        .iter()
        .map(|t| t.token_budget.total_used)
        .max()
        .unwrap_or(0);
    let peak_idx = traces
        .iter()
        .position(|t| t.token_budget.total_used == peak_total)
        .unwrap_or(0);
    let min_total = traces
        .iter()
        .map(|t| t.token_budget.total_used)
        .min()
        .unwrap_or(0);
    let min_idx = traces
        .iter()
        .position(|t| t.token_budget.total_used == min_total)
        .unwrap_or(0);
    eprintln!(
        "    {:<24} {} tokens (T{})",
        "peak_usage:".dim(),
        peak_total.to_string().red(),
        peak_idx + 1
    );
    eprintln!(
        "    {:<24} {} tokens (T{})",
        "min_usage:".dim(),
        min_total.to_string().green(),
        min_idx + 1
    );

    let peak_pressure = traces
        .iter()
        .map(|t| t.token_budget.budget_pressure)
        .fold(0.0_f64, f64::max);
    let peak_p_idx = traces
        .iter()
        .position(|t| (t.token_budget.budget_pressure - peak_pressure).abs() < f64::EPSILON)
        .unwrap_or(0);
    eprintln!(
        "    {:<24} {} (T{})",
        "peak_pressure:".dim(),
        format_pressure(peak_pressure),
        peak_p_idx + 1
    );

    // ── Compression Events ──
    let compression_turns: Vec<usize> = traces
        .iter()
        .enumerate()
        .filter(|(_, t)| t.token_budget.compression_triggered)
        .map(|(i, _)| i + 1)
        .collect();

    if !compression_turns.is_empty() {
        eprintln!();
        eprintln!("  {}", "▸ Compression Events".bold());
        eprintln!(
            "    {:<24} {} of {} turns",
            "triggered:".dim(),
            compression_turns.len().to_string().yellow(),
            traces.len()
        );
        eprintln!(
            "    {:<24} {}",
            "turns:".dim(),
            compression_turns
                .iter()
                .map(|t| format!("T{t}"))
                .collect::<Vec<_>>()
                .join(", ")
                .yellow()
        );
        eprintln!(
            "    {:<24} {:.0}%",
            "trigger_rate:".dim(),
            agg.compression_trigger_rate * 100.0
        );
        eprintln!(
            "    {:<24} {:.0}%",
            "avg_compression_ratio:".dim(),
            agg.avg_compression_ratio * 100.0
        );
    }

    // ── Budget Pressure Sparkline ──
    if traces.len() >= 2 {
        eprintln!();
        eprintln!("  {}", "▸ Budget Pressure Trend".bold());
        let pressures: Vec<f64> = traces
            .iter()
            .map(|t| t.token_budget.budget_pressure * 100.0)
            .collect();
        eprintln!("    {}", ascii_sparkline(&pressures, 40));
        eprintln!(
            "    {} {:.0}%  {} {:.0}%",
            "T1:".dim(),
            pressures[0],
            format!("T{}:", traces.len()).dim(),
            pressures.last().unwrap()
        );
    }

    eprintln!();
}

// ─── Rendering Helpers ───────────────────────────────────────────────────────

/// Render a proportional bar using block characters.
fn proportional_bar(pct: f64, width: usize) -> String {
    let filled = (pct / 100.0 * width as f64).round() as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    format!("{}{}", "█".repeat(filled).dim(), "░".repeat(empty).dim())
}

/// Render a small proportional bar (no border).
fn mini_bar(pct: f64, width: usize) -> String {
    let filled = (pct / 100.0 * width as f64).round() as usize;
    let filled = filled.min(width);
    format!("{}", "▓".repeat(filled).dim())
}

/// Format budget pressure with color.
fn format_pressure(pressure: f64) -> String {
    let s = format!("{:.0}%", pressure * 100.0);
    if pressure > 0.9 {
        s.red().to_string()
    } else if pressure > 0.7 {
        s.yellow().to_string()
    } else {
        s.green().to_string()
    }
}

/// Render ASCII sparkline from a series of values.
fn ascii_sparkline(values: &[f64], width: usize) -> String {
    if values.is_empty() {
        return String::new();
    }

    let spark_chars = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let range = (max - min).max(0.001);

    // If more values than width, sample; otherwise use all
    let sampled: Vec<f64> = if values.len() > width {
        (0..width)
            .map(|i| {
                let idx = i * values.len() / width;
                values[idx.min(values.len() - 1)]
            })
            .collect()
    } else {
        values.to_vec()
    };

    sampled
        .iter()
        .map(|v| {
            let normalized = ((v - min) / range * 7.0).round() as usize;
            spark_chars[normalized.min(7)]
        })
        .collect()
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Resolve a turn argument to a 0-based index into the traces array.
/// Supports: "" (latest), "N" (1-based), "-1" (last), "-N" (from end).
fn resolve_turn_index(arg: &str, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let arg = arg.trim();
    if arg.is_empty() {
        return Some(len - 1); // latest
    }
    if let Ok(n) = arg.parse::<i64>() {
        if n > 0 {
            let idx = (n - 1) as usize;
            if idx < len {
                return Some(idx);
            }
        } else if n < 0 {
            let from_end = (-n) as usize;
            if from_end <= len {
                return Some(len - from_end);
            }
        }
    }
    None
}

fn format_rejection_reason(
    reason: &astra_runtime::turn::context_assembly_trace::RejectionReason,
) -> String {
    use astra_runtime::turn::context_assembly_trace::RejectionReason;
    match reason {
        RejectionReason::BelowThreshold { threshold, score } => {
            format!("score {score:.2} < threshold {threshold:.2}")
        }
        RejectionReason::TokenBudgetExceeded {
            available,
            required,
        } => {
            format!("needs {required} tok, only {available} available")
        }
        RejectionReason::Duplicate { of_memory_id } => {
            format!("duplicate of {of_memory_id}")
        }
        RejectionReason::Stale { age_days } => format!("{age_days} days old"),
    }
}

fn format_compression_method(
    method: &astra_runtime::turn::context_assembly_trace::CompressionMethod,
) -> &'static str {
    use astra_runtime::turn::context_assembly_trace::CompressionMethod;
    match method {
        CompressionMethod::ToolResultTruncation => "ToolResultTrunc",
        CompressionMethod::DuplicateReadElimination => "DuplicateReadElim",
        CompressionMethod::LlmSummarization => "LlmSummarize",
        CompressionMethod::TieredCompaction => "TieredCompact",
        CompressionMethod::ReactiveCompact => "ReactiveCompact",
    }
}

fn format_trace_decision_type(
    dt: &astra_runtime::turn::context_assembly_trace::DecisionType,
) -> String {
    use astra_runtime::turn::context_assembly_trace::DecisionType;
    match dt {
        DecisionType::ToolSelection { tools } => {
            format!("ToolSelection ({})", tools.len())
        }
        DecisionType::HistoryCompression { turns_affected } => {
            format!("HistoryCompression ({} turns)", turns_affected.len())
        }
        DecisionType::MemoryRetrieval { memories } => {
            format!("MemoryRetrieval ({})", memories.len())
        }
        DecisionType::StrategyChoice { strategy } => {
            format!("Strategy: {strategy}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── proportional_bar ────────────────────────────────────────────────────

    #[test]
    fn proportional_bar_zero() {
        let bar = proportional_bar(0.0, 10);
        // Should be all empty chars
        assert!(bar.contains('░'));
        assert!(!bar.contains('█'));
    }

    #[test]
    fn proportional_bar_full() {
        let bar = proportional_bar(100.0, 10);
        assert!(bar.contains('█'));
    }

    #[test]
    fn proportional_bar_half() {
        let bar = proportional_bar(50.0, 10);
        // Should have both filled and empty
        assert!(bar.contains('█'));
        assert!(bar.contains('░'));
    }

    #[test]
    fn proportional_bar_over_100() {
        // Should not panic, clamped to width
        let bar = proportional_bar(150.0, 10);
        assert!(!bar.is_empty());
    }

    #[test]
    fn proportional_bar_negative() {
        // Should not panic
        let bar = proportional_bar(-10.0, 10);
        assert!(!bar.is_empty());
    }

    #[test]
    fn proportional_bar_zero_width() {
        // Zero width produces two empty styled strings concatenated
        let bar = proportional_bar(50.0, 0);
        // Should not panic; result contains only ANSI escape sequences
        assert!(!bar.contains('█'));
        assert!(!bar.contains('░'));
    }

    // ─── mini_bar ────────────────────────────────────────────────────────────

    #[test]
    fn mini_bar_zero() {
        let bar = mini_bar(0.0, 10);
        assert!(!bar.contains('▓'));
    }

    #[test]
    fn mini_bar_full() {
        let bar = mini_bar(100.0, 10);
        assert!(bar.contains('▓'));
    }

    // ─── format_pressure ─────────────────────────────────────────────────────

    #[test]
    fn format_pressure_low() {
        let s = format_pressure(0.3);
        assert!(s.contains("30%"));
    }

    #[test]
    fn format_pressure_medium() {
        let s = format_pressure(0.75);
        assert!(s.contains("75%"));
    }

    #[test]
    fn format_pressure_high() {
        let s = format_pressure(0.95);
        assert!(s.contains("95%"));
    }

    #[test]
    fn format_pressure_zero() {
        let s = format_pressure(0.0);
        assert!(s.contains("0%"));
    }

    #[test]
    fn format_pressure_over_one() {
        let s = format_pressure(1.2);
        assert!(s.contains("120%"));
    }

    // ─── ascii_sparkline ─────────────────────────────────────────────────────

    #[test]
    fn ascii_sparkline_empty() {
        let result = ascii_sparkline(&[], 10);
        assert!(result.is_empty());
    }

    #[test]
    fn ascii_sparkline_single_value() {
        let result = ascii_sparkline(&[50.0], 10);
        assert_eq!(result.chars().count(), 1);
    }

    #[test]
    fn ascii_sparkline_constant_values() {
        let values = vec![50.0, 50.0, 50.0, 50.0];
        let result = ascii_sparkline(&values, 10);
        // All same value — sparkline should work without division-by-zero
        assert_eq!(result.chars().count(), 4);
    }

    #[test]
    fn ascii_sparkline_ascending() {
        let values = vec![0.0, 25.0, 50.0, 75.0, 100.0];
        let result = ascii_sparkline(&values, 10);
        let chars: Vec<char> = result.chars().collect();
        assert_eq!(chars.len(), 5);
        // First should be lowest block, last should be highest
        assert!(chars[0] <= chars[4]);
    }

    #[test]
    fn ascii_sparkline_many_values_downsampled() {
        let values: Vec<f64> = (0..100).map(|i| i as f64).collect();
        let result = ascii_sparkline(&values, 20);
        // Should downsample to 20 chars
        assert_eq!(result.chars().count(), 20);
    }

    #[test]
    fn ascii_sparkline_fewer_values_than_width() {
        let values = vec![10.0, 50.0, 90.0];
        let result = ascii_sparkline(&values, 20);
        // Should use all 3 values (not pad to 20)
        assert_eq!(result.chars().count(), 3);
    }

    // ─── resolve_turn_index ──────────────────────────────────────────────────

    #[test]
    fn resolve_turn_index_empty_len() {
        assert_eq!(resolve_turn_index("1", 0), None);
    }

    #[test]
    fn resolve_turn_index_empty_arg() {
        assert_eq!(resolve_turn_index("", 5), Some(4)); // latest
    }

    #[test]
    fn resolve_turn_index_positive() {
        assert_eq!(resolve_turn_index("1", 5), Some(0));
        assert_eq!(resolve_turn_index("3", 5), Some(2));
        assert_eq!(resolve_turn_index("5", 5), Some(4));
    }

    #[test]
    fn resolve_turn_index_out_of_bounds() {
        assert_eq!(resolve_turn_index("6", 5), None);
        assert_eq!(resolve_turn_index("100", 5), None);
    }

    #[test]
    fn resolve_turn_index_negative() {
        assert_eq!(resolve_turn_index("-1", 5), Some(4)); // last
        assert_eq!(resolve_turn_index("-3", 5), Some(2)); // 3rd from end
        assert_eq!(resolve_turn_index("-5", 5), Some(0)); // first
    }

    #[test]
    fn resolve_turn_index_negative_beyond() {
        assert_eq!(resolve_turn_index("-6", 5), None);
        assert_eq!(resolve_turn_index("-100", 5), None);
    }

    #[test]
    fn resolve_turn_index_zero() {
        assert_eq!(resolve_turn_index("0", 5), None);
    }

    #[test]
    fn resolve_turn_index_non_numeric() {
        assert_eq!(resolve_turn_index("abc", 5), None);
        assert_eq!(resolve_turn_index("1.5", 5), None);
    }
}
