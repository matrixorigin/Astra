use super::*;

pub(super) struct StateCommandContext<'a> {
    pub(super) api: &'a astra_thin_client::ThinClient,
    pub(super) profile: Option<&'a str>,
    pub(super) token: Option<&'a str>,
    pub(super) selector: &'a dyn tool_selector::ToolSelector,
}

pub(super) async fn handle_state_command(
    cmd: &str,
    arg: &str,
    ctx: StateCommandContext<'_>,
    state: &mut ReplState,
) -> Result<(), String> {
    let StateCommandContext {
        api,
        profile,
        token,
        selector,
    } = ctx;
    match cmd {
        "/clear" => {
            let Some(tok) = token else {
                eprintln!("{}", "  Not logged in. Use /login.".yellow());
                return Ok(());
            };
            let body = api
                .post_sessions_json(tok, &serde_json::json!({}))
                .await
                .map_err(map_thin_err)?;
            let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let new_sid = value
                .get("session_id")
                .or_else(|| value.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(sid) = &new_sid {
                let mut creds = load_credentials();
                let pname = profile_name(profile, &creds);
                let p = creds.profiles.entry(pname).or_default();
                p.last_session_id = Some(sid.clone());
                let _ = save_credentials(&creds);
            }
            state.session_id = new_sid.clone();
            state.turn = 0;
            state.run_id = None;
            state.history.clear();
            state.total_prompt_tokens = 0;
            state.total_completion_tokens = 0;
            if let Some(ref sid) = new_sid {
                state.journal = session_journal::JournalWriter::new(sid).ok();
                if let Some(ref j) = state.journal {
                    let _ = j.append(&session_journal::JournalEvent::session_start(
                        Some(sid),
                        state.model.as_deref(),
                    ));
                }
            }
            let display = new_sid.as_deref().unwrap_or("(none)");
            eprintln!(
                "{}",
                format!("  \u{2713}  New session: {}", display).green()
            );
        }

        "/explain" => {
            state.explain = match state.explain {
                ExplainMode::Off => ExplainMode::On,
                ExplainMode::On => ExplainMode::Verbose,
                ExplainMode::Verbose => ExplainMode::Off,
            };
            let s = match state.explain {
                ExplainMode::Off => "off".yellow().to_string(),
                ExplainMode::On => "on".green().to_string(),
                ExplainMode::Verbose => "verbose".green().to_string(),
            };
            eprintln!("  Explain mode: {}", s);
            if matches!(state.explain, ExplainMode::On) {
                eprintln!("{}", "  (verbose: selector + skill lines on stderr)".dim());
            }
            if let Some(ref j) = state.journal {
                let explain_val = match state.explain {
                    ExplainMode::Off => "off",
                    ExplainMode::On => "on",
                    ExplainMode::Verbose => "verbose",
                };
                let _ = j.append(&session_journal::JournalEvent::config_change(
                    state.session_id.as_deref(),
                    "explain",
                    explain_val,
                ));
            }
        }

        "/verbose" => {
            state.verbose_mode = true;
            eprintln!("  Verbose mode on");
        }

        "/compact" => {
            if state.history.is_empty() {
                eprintln!(
                    "  {}",
                    "Nothing to compact — no conversation history yet.".dim()
                );
                return Ok(());
            }
            let Some(tok) = token else {
                eprintln!("{}", "  Not logged in. Use /login.".yellow());
                return Ok(());
            };

            eprintln!("  {}", "Summarizing…".dim());
            let mut auto_pm =
                PermissionManager::with_project(true, &std::env::current_dir().unwrap_or_default());
            let mut _cancel_token_guard: Option<std::sync::Arc<tokio_util::sync::CancellationToken>> = None;
            let summary_result = tokio::select! {
                r = stream_chat_sse(ChatTurnParams {
                    api,
                    token: tok,
                    message: prompts::COMPACT_SUMMARY_REQUEST,
                    session_id: state.session_id.as_deref(),
                    model: state.model.as_deref(),
                    explain: ExplainMode::Off,
                    render_md: false,
                    history: &state.history,
                    perm_manager: &mut auto_pm,
                    verbose_mode: false,
                    quiet: true,
                    suppress_intermediate_output: false,
                    selector,
                    recent_tools: &[],
                    tool_health_entries: &[],
                    skill_registry: crate::skill_instructions::empty_registry(),
                    plan_only_chat: false,
                    hide_streaming_assistant_text: false,
                    is_plan_subtask: false,
                    plan_subtask_id: None,
                    delegation_engine: None,
                    cancel_token: {
                        let token = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
                        _cancel_token_guard = Some(token.clone());
                        Some(token)
                    },
                    plan_assemble_line_release: None,
                    stream_event_tx: None,
                approval_request_tx: None,
                }) => r,
                _ = tokio::signal::ctrl_c() => {
                    if let Some(ref t) = _cancel_token_guard { t.cancel(); }
                    eprintln!("{}", "  Interrupted.".dim());
                    return Ok(());
                }
            };

            let summary = match summary_result {
                Ok(sr) => {
                    let text = sr.full_text.trim().to_string();
                    if text.is_empty() {
                        eprintln!("{}", "  ✗ Empty summary returned.".yellow());
                        return Ok(());
                    }
                    text
                }
                Err(e) => {
                    eprintln!("{}", format!("  ✗ Failed to summarize: {e}").red());
                    return Ok(());
                }
            };

            // Save summary to Memoria via server proxy (preserves user isolation)
            let mut saved_to_memoria = false;
            let mut facts_stored = 0usize;
            if let Some(tok) = token {
                let meta = prompts::memory_proto::EntryMeta::from_session(
                    state.session_id.as_deref(),
                    state.turn,
                    prompts::memory_proto::SRC_COMPACT,
                );
                let entry = prompts::memory_proto::MemoryEntry::new(
                    prompts::memory_proto::NS_EPISODE,
                    prompts::memory_proto::ST_SUMMARY,
                    &summary,
                );
                match api
                    .post_memory_store_json(tok, &entry.to_store_payload_with_meta(&meta))
                    .await
                {
                    Ok(r) if r.status().is_success() => saved_to_memoria = true,
                    Ok(r) => eprintln!(
                        "{}",
                        format!("  ⚠ Memory save failed ({})", r.status()).yellow()
                    ),
                    Err(e) => eprintln!(
                        "{}",
                        format!("  ⚠ Memory service unreachable: {e}").yellow()
                    ),
                }

                // Extract structured facts from summary and store each individually
                if saved_to_memoria {
                    let extract_msg = format!("{}{summary}", prompts::MEMORY_EXTRACTOR_PROMPT);
                    let mut auto_pm2 = PermissionManager::with_project(
                        true,
                        &std::env::current_dir().unwrap_or_default(),
                    );
                    let extract_result = stream_chat_sse(ChatTurnParams {
                        api,
                        token: tok,
                        message: &extract_msg,
                        session_id: state.session_id.as_deref(),
                        model: state.model.as_deref(),
                        explain: ExplainMode::Off,
                        render_md: false,
                        history: &[],
                        perm_manager: &mut auto_pm2,
                        verbose_mode: false,
                        quiet: true,
                        suppress_intermediate_output: false,
                        selector,
                        recent_tools: &[],
                        tool_health_entries: &[],
                        skill_registry: crate::skill_instructions::empty_registry(),
                        plan_only_chat: false,
                        hide_streaming_assistant_text: false,
                        is_plan_subtask: false,
                        plan_subtask_id: None,
                        delegation_engine: None,
                        cancel_token: None,
                        plan_assemble_line_release: None,
                        stream_event_tx: None,
                approval_request_tx: None,
                    })
                    .await;

                    if let Ok(sr) = extract_result {
                        let facts = prompts::parse_extracted_facts(&sr.full_text);
                        let fact_meta = prompts::memory_proto::EntryMeta::from_session(
                            state.session_id.as_deref(),
                            state.turn,
                            prompts::memory_proto::SRC_EXTRACTED,
                        );
                        for (fact, mem_type) in &facts {
                            let fact_entry = prompts::memory_proto::MemoryEntry::new(
                                prompts::memory_proto::NS_FACT,
                                mem_type,
                                fact,
                            );
                            let _ = api
                                .post_memory_store_json(
                                    tok,
                                    &fact_entry.to_store_payload_with_meta(&fact_meta),
                                )
                                .await;
                            facts_stored += 1;
                        }

                        // ── Knowledge synthesis: detect patterns across extracted facts ──
                        if facts.len() >= 2 {
                            let fact_lines: Vec<String> =
                                facts.iter().map(|(f, t)| format!("- [{t}] {f}")).collect();
                            let synthesis_prompt = format!(
                                "Given these extracted facts from a conversation:\n{}\n\n\
                                 If there is a higher-level pattern, theme, or insight that \
                                 connects 2+ facts, state it as ONE concise sentence (≤25 words). \
                                 If no pattern exists, respond with exactly: NONE",
                                fact_lines.join("\n")
                            );
                            let mut auto_pm3 = PermissionManager::with_project(
                                true,
                                &std::env::current_dir().unwrap_or_default(),
                            );
                            let synth_result = stream_chat_sse(ChatTurnParams {
                                api,
                                token: tok,
                                message: &synthesis_prompt,
                                session_id: state.session_id.as_deref(),
                                model: state.model.as_deref(),
                                explain: ExplainMode::Off,
                                render_md: false,
                                history: &[],
                                perm_manager: &mut auto_pm3,
                                verbose_mode: false,
                                quiet: true,
                                suppress_intermediate_output: false,
                                selector,
                                recent_tools: &[],
                                tool_health_entries: &[],
                                skill_registry: crate::skill_instructions::empty_registry(),
                                plan_only_chat: false,
                                hide_streaming_assistant_text: false,
                                is_plan_subtask: false,
                                plan_subtask_id: None,
                                delegation_engine: None,
                                cancel_token: None,
                                plan_assemble_line_release: None,
                                stream_event_tx: None,
                approval_request_tx: None,
                            })
                            .await;
                            if let Ok(sr2) = synth_result {
                                let insight = sr2.full_text.trim().to_string();
                                if !insight.is_empty()
                                    && !insight.eq_ignore_ascii_case("NONE")
                                    && !insight.contains("no pattern")
                                    && insight.len() < 200
                                {
                                    let insight_entry = prompts::memory_proto::MemoryEntry::new(
                                        prompts::memory_proto::NS_INSIGHT,
                                        prompts::memory_proto::ST_ACTIVE,
                                        &insight,
                                    );
                                    let synth_meta = prompts::memory_proto::EntryMeta::from_session(
                                        state.session_id.as_deref(),
                                        state.turn,
                                        prompts::memory_proto::SRC_SYNTHESIS,
                                    );
                                    let _ = api
                                        .post_memory_store_json(
                                            tok,
                                            &insight_entry.to_store_payload_with_meta(&synth_meta),
                                        )
                                        .await;
                                    facts_stored += 1; // count insight as a stored fact
                                }
                            }
                        }
                    }
                }
            }

            // Truncate history: keep last 3 turns, prepend summary as system context
            const KEEP_RECENT: usize = 3;
            let total = state.history.len();
            let trimmed_count = total.saturating_sub(KEEP_RECENT);
            if trimmed_count > 0 {
                // ── Context swap: store trimmed turns to memory for later retrieval ──
                if let Some(tok) = token {
                    // Build a compact representation of the swapped turns
                    let mut swap_lines: Vec<String> = Vec::new();
                    for (user_msg, assistant_msg) in &state.history[..trimmed_count] {
                        if !user_msg.is_empty() {
                            let preview: String = user_msg.chars().take(100).collect();
                            swap_lines.push(format!("U: {preview}"));
                        }
                        if !assistant_msg.is_empty() {
                            let preview: String = assistant_msg.chars().take(150).collect();
                            swap_lines.push(format!("A: {preview}"));
                        }
                    }
                    if !swap_lines.is_empty() {
                        let swap_body = format!(
                            "Turns 1-{trimmed_count} swapped out:\n{}",
                            swap_lines.join("\n")
                        );
                        // Cap at 2000 chars to avoid storing excessive content
                        let capped: String = swap_body.chars().take(2000).collect();
                        let swap_entry = prompts::memory_proto::MemoryEntry::new(
                            prompts::memory_proto::NS_SWAP,
                            prompts::memory_proto::ST_ARCHIVED,
                            &capped,
                        );
                        let swap_meta = prompts::memory_proto::EntryMeta::from_session(
                            state.session_id.as_deref(),
                            state.turn,
                            prompts::memory_proto::SRC_COMPACT,
                        );
                        let _ = api
                            .post_memory_store_json(
                                tok,
                                &swap_entry.to_store_payload_with_meta(&swap_meta),
                            )
                            .await;
                    }
                }

                let anchor = crate::repl_turn::fetch_compact_memory_anchor_snippet(
                    api,
                    tok,
                    state.session_id.as_deref(),
                    &summary,
                )
                .await;
                let assistant_text = crate::repl_turn::compact_assistant_message(
                    trimmed_count,
                    &summary,
                    anchor.as_deref(),
                );
                let context_entry = (String::new(), assistant_text);
                let mut new_hist = vec![context_entry];
                new_hist.extend_from_slice(&state.history[trimmed_count..]);
                state.history = new_hist;
                state.recent_tools.clear();
            }

            // Print the summary box
            eprintln!();
            eprintln!(
                "{}",
                "─── Compact Summary ────────────────────────────────────────".dim()
            );
            for line in summary.lines() {
                eprintln!("  {line}");
            }
            eprintln!(
                "{}",
                "────────────────────────────────────────────────────────────".dim()
            );
            eprintln!();
            let mem_note = if saved_to_memoria {
                if facts_stored > 0 {
                    format!(" · saved to memory ({facts_stored} facts extracted)")
                } else {
                    " · saved to memory".to_string()
                }
            } else {
                String::new()
            };
            eprintln!(
                "  {} {} turns compacted · {} turns in context{}",
                "✓".green(),
                trimmed_count,
                state.history.len(),
                mem_note,
            );
            if state.plan_mode.is_some() || state.executing_plan.is_some() {
                eprintln!(
                    "{}",
                    "  Tip: Plan context was shortened — if steps feel stale, refresh `/plan` or your plan view."
                        .dim()
                );
            }
            // Journal: log compact event
            if let Some(ref j) = state.journal {
                let _ = j.append(&session_journal::JournalEvent::compact(
                    state.session_id.as_deref(),
                    state.turn,
                    trimmed_count,
                    facts_stored,
                ));
            }
        }

        "/reflect" => {
            let Some(tok) = token else {
                eprintln!("{}", "  Not logged in. Use /login.".yellow());
                return Ok(());
            };
            let sid = match state.session_id.as_deref() {
                Some(s) => s.to_string(),
                None => {
                    eprintln!("{}", "  No active session.".yellow());
                    return Ok(());
                }
            };
            let known_focuses = [
                "auto",
                "skill_failure",
                "unexpected_result",
                "data_quality",
                "tool_selection",
                "history",
                "performance",
            ];
            let mut rel = astra_thin_client::paths::chat_session_reflect(&sid)
                .trim_start_matches('/')
                .to_string();
            let mut query_parts: Vec<String> = Vec::new();
            let mut parts2 = arg.splitn(2, ' ');
            let first = parts2.next().unwrap_or("").trim();
            let rest = parts2.next().unwrap_or("").trim();
            if known_focuses.contains(&first) {
                if !first.is_empty() {
                    query_parts.push(format!("focus={}", first));
                }
                if !rest.is_empty() {
                    query_parts.push(format!("question={}", urlencoding(rest)));
                }
            } else if !arg.is_empty() {
                query_parts.push(format!("question={}", urlencoding(arg)));
            }
            if !query_parts.is_empty() {
                rel = format!("{rel}?{}", query_parts.join("&"));
            }
            match api.get_authed_path_text(tok, &rel).await {
                Ok(body) => render_reflect_report(&body, &sid),
                Err(astra_thin_client::ThinClientError::Api {
                    status,
                    body: err_body,
                }) => {
                    eprintln!(
                        "{}",
                        format!("  ✗ API Error ({}): {}", status, compact_or_raw(&err_body)).red()
                    );
                }
                Err(e) => eprintln!("{}", format!("  ✗ Reflect failed: {e}").red()),
            }
        }
        _ => unreachable!("unexpected state command: {cmd}"),
    }

    Ok(())
}

/// Render a `ReflectReport` JSON as a compact, colored terminal report.
fn render_reflect_report(body: &str, session_id: &str) {
    let Ok(report) = serde_json::from_str::<serde_json::Value>(body) else {
        print_json_or_raw(body);
        return;
    };

    let overview = &report["overview"];
    let short_sid = if session_id.len() > 8 {
        &session_id[..8]
    } else {
        session_id
    };

    // Header
    eprintln!(
        "{}",
        format!("🔍 Session Diagnosis — {short_sid}").cyan().bold()
    );
    eprintln!("{}", "─────────────────────────────────────".dim());

    // Overview line
    let total_events = overview["total_events"].as_i64().unwrap_or(0);
    let total_decisions = overview["total_decisions"].as_i64().unwrap_or(0);
    let dur = overview["duration_minutes"]
        .as_f64()
        .map(|d| format!(", {d:.0}min"))
        .unwrap_or_default();
    eprintln!(
        "  {} {total_events} events, {total_decisions} decisions{dur}",
        "Overview:".bold()
    );

    // Top skills
    if let Some(skills) = overview["top_skills"].as_array()
        && !skills.is_empty()
    {
        let skill_strs: Vec<String> = skills
            .iter()
            .filter_map(|s| {
                let name = s[0].as_str()?;
                let cnt = s[1].as_i64()?;
                Some(format!("{name}({cnt})"))
            })
            .collect();
        eprintln!("  {} {}", "Skills:".bold(), skill_strs.join(", "));
    }

    // Errors summary
    let error_count = overview["error_count"].as_i64().unwrap_or(0);
    let error_rate = overview["error_rate_pct"].as_f64().unwrap_or(0.0);
    if error_count > 0 {
        let err_str = format!("  Errors: {error_count} ({error_rate:.1}%)");
        if error_rate > 30.0 {
            eprintln!("{}", err_str.red().bold());
        } else if error_rate > 15.0 {
            eprintln!("{}", err_str.yellow());
        } else {
            eprintln!("  {} {error_count} ({error_rate:.1}%)", "Errors:".bold());
        }
    }

    // ── Diagnoses (primary output — root-cause analysis) ────────────
    let has_diagnoses = report["diagnoses"]
        .as_array()
        .is_some_and(|d| !d.is_empty());
    let has_insights = report["insights"]
        .as_array()
        .is_some_and(|arr| arr.iter().any(|i| i["severity"].as_str() != Some("info")));
    let has_recs = report["recommendations"]
        .as_array()
        .is_some_and(|r| !r.is_empty());

    if has_diagnoses {
        eprintln!();
        eprintln!("  {}", "Root-Cause Analysis:".bold());
        for diag in report["diagnoses"].as_array().unwrap() {
            let severity = diag["severity"].as_str().unwrap_or("info");
            let summary = diag["summary"].as_str().unwrap_or("");
            let fix = diag["fix_hint"].as_str().unwrap_or("");

            match severity {
                "critical" => eprintln!("  🔴 {}", summary.red().bold()),
                "warning" => eprintln!("  ⚠️ {}", summary.yellow()),
                _ => eprintln!("  ℹ️ {}", summary),
            }

            // Show sample errors (truncated)
            if let Some(samples) = diag["samples"].as_array() {
                for (i, sample) in samples.iter().enumerate() {
                    if i >= 2 {
                        break;
                    }
                    if let Some(s) = sample.as_str() {
                        let truncated: String = s.chars().take(80).collect();
                        eprintln!("    {} {}", "│".dim(), truncated.dim());
                    }
                }
            }

            if !fix.is_empty() {
                eprintln!("    {} {}", "→".green(), fix.green());
            }
        }
    }

    // ── Insights (secondary — statistical observations) ─────────────
    if has_insights {
        eprintln!();
        let non_info_insights: Vec<_> = report["insights"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["severity"].as_str() != Some("info"))
            .collect();
        for insight in non_info_insights {
            let severity = insight["severity"].as_str().unwrap_or("info");
            let message = insight["message"].as_str().unwrap_or("");
            let evidence = insight["evidence"].as_str().unwrap_or("");
            let line = if evidence.is_empty() {
                message.to_string()
            } else {
                format!("{message} — {evidence}")
            };
            match severity {
                "critical" => eprintln!("  🔴 {}", line.red().bold()),
                "warning" => eprintln!("  ⚠️ {}", line.yellow()),
                _ => eprintln!("  ℹ️ {}", line.dim()),
            }
        }
    }

    // ── Recommendations ─────────────────────────────────────────────
    if has_recs {
        eprintln!();
        eprintln!("  {}", "Fix Actions:".bold());
        for rec in report["recommendations"].as_array().unwrap() {
            if let Some(r) = rec.as_str() {
                eprintln!("    {} {}", "→".green(), r);
            }
        }
    }

    // ── Healthy session or empty? Show appropriate feedback ─────────
    if !has_diagnoses && !has_insights && !has_recs {
        eprintln!();
        if total_events == 0 {
            eprintln!("  ℹ️ {}", "Empty session — no events recorded yet.".dim());
        } else if error_count == 0 {
            eprintln!("  ✅ {}", "Session healthy — no errors detected.".green());
            // Show event distribution as useful info
            if let Some(types) = overview["top_event_types"].as_array()
                && !types.is_empty()
            {
                let type_strs: Vec<String> = types
                    .iter()
                    .filter_map(|t| {
                        let name = t[0].as_str()?;
                        let cnt = t[1].as_i64()?;
                        Some(format!("{name}({cnt})"))
                    })
                    .collect();
                eprintln!("  {} {}", "Events:".bold(), type_strs.join(", "));
            }
        } else {
            eprintln!("  ℹ️ {}", "No actionable issues found.".dim());
        }
        eprintln!();
        eprintln!(
            "  {}",
            "Tip: /reflect skill_failure — focus on tool errors".dim()
        );
        eprintln!("  {}", "     /reflect performance — focus on latency".dim());
    }
}
