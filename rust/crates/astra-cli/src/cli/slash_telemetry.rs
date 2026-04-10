use super::*;

/// Handle `/telemetry` command — display observability session metrics.
///
/// Subcommands:
/// - (no arg)     Show summary: turns, timings, drift, decisions
/// - `turns`      List per-turn timing breakdowns
/// - `drift`      Check focus drift analysis
/// - `decisions`  List tool selection decisions with confidence
pub(super) fn handle_telemetry_command(arg: &str, state: &ReplState) {
    let (sub_cmd, _sub_arg) = match arg.find(char::is_whitespace) {
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
        "help" | "-h" | "--help" => {
            eprintln!(
                "\n{}",
                "─── Telemetry Commands ──────────────────────────"
                    .bold()
                    .cyan()
            );
            eprintln!("  {}   Show session summary", "/telemetry".cyan());
            eprintln!(
                "  {}   List per-turn timing breakdowns",
                "/telemetry turns".cyan()
            );
            eprintln!(
                "  {}   Check focus drift analysis",
                "/telemetry drift".cyan()
            );
            eprintln!(
                "  {}   List tool selection decisions",
                "/telemetry decisions".cyan()
            );
            eprintln!(
                "  {}   Show user profile/preferences",
                "/telemetry profile".cyan()
            );
            eprintln!();
        }
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
    let session_guard = session.read().unwrap();

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
        let llm_ms: u64 = session_guard
            .turn_timings
            .iter()
            .map(|t| t.llm_latency_ms)
            .sum();
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
    let session_guard = session.read().unwrap();

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
        let llm_str = format!("{}ms", timing.llm_latency_ms);
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
    let session_guard = session.read().unwrap();

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
    if let Some(ref goal) = state.session_goal {
        let analysis = session_guard.check_drift(goal);
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
    let session_guard = session.read().unwrap();

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
