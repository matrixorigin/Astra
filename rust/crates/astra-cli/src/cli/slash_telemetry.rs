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
        "  {}    Tool selection scoring for turn N",
        "/telemetry tools [N]".cyan()
    );
    eprintln!(
        "  {}  History compression for turn N",
        "/telemetry compression [N]".cyan()
    );
    eprintln!("  {}   Token budget evolution", "/telemetry budget".cyan());
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
