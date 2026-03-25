use super::*;

pub(super) struct ReplTurnContext<'a> {
    pub(super) client: &'a reqwest::Client,
    pub(super) base: &'a str,
    pub(super) profile: Option<&'a str>,
    pub(super) selector: &'a dyn tool_selector::ToolSelector,
}

enum TurnAttempt {
    Completed(Result<StreamResult, String>),
    Interrupted,
}

pub(super) async fn handle_chat_input(
    line: String,
    current_token: Option<&str>,
    state: &mut ReplState,
    ctx: ReplTurnContext<'_>,
) -> Result<(), String> {
    let token = match current_token {
        Some(token) => token,
        None => {
            eprintln!(
                "{}",
                "  Not logged in. Use /login to authenticate.".yellow()
            );
            return Ok(());
        }
    };

    eprintln!();

    let effective_line = build_effective_line(&line, state);
    let turn_start = Instant::now();

    maybe_auto_compact(state, &ctx, token).await?;

    let session_id = state.session_id.clone();
    match run_chat_turn(state, &ctx, token, &effective_line, session_id.as_deref()).await {
        TurnAttempt::Interrupted => return Ok(()),
        TurnAttempt::Completed(Ok(result)) => {
            apply_turn_success(state, ctx.profile, &line, result, turn_start);
            return Ok(());
        }
        TurnAttempt::Completed(Err(error)) => {
            if is_session_not_found_error(&error) && state.session_id.is_some() {
                let _ = clear_profile_last_session(ctx.profile);
                state.session_id = None;
                eprintln!(
                    "{}",
                    "  Session not found. Creating a new session…".yellow()
                );

                match run_chat_turn(state, &ctx, token, &effective_line, None).await {
                    TurnAttempt::Interrupted => return Ok(()),
                    TurnAttempt::Completed(Ok(result)) => {
                        apply_turn_success(state, ctx.profile, &line, result, turn_start);
                        return Ok(());
                    }
                    TurnAttempt::Completed(Err(retry_error)) => {
                        report_turn_error(state, &line, &retry_error, turn_start);
                        return Ok(());
                    }
                }
            }

            report_turn_error(state, &line, &error, turn_start);
        }
    }

    Ok(())
}

pub(super) fn build_effective_line(line: &str, state: &ReplState) -> String {
    let mut effective_line = if let (Some(skill_name), Some(source)) = (
        state.skill_dev_name.as_deref(),
        state.skill_dev_context.as_deref(),
    ) {
        format!(
            "{}{line}",
            prompts::build_skill_dev_prefix(skill_name, source)
        )
    } else {
        line.to_string()
    };

    if !state.active_system_skills.is_empty() {
        let skill_block = prompts::build_skill_instructions(&state.active_system_skills);
        effective_line = format!("{skill_block}\n\n{effective_line}");
    }

    effective_line
}

async fn maybe_auto_compact(
    state: &mut ReplState,
    ctx: &ReplTurnContext<'_>,
    token: &str,
) -> Result<(), String> {
    if state.history.len() <= state.context_budget.keep_recent_turns {
        return Ok(());
    }

    let est_messages = history_as_messages(&state.history);
    let est_tokens = prompts::estimate_tokens(&est_messages);
    if !state.context_budget.should_compact(est_tokens) {
        return Ok(());
    }

    eprintln!(
        "  {}",
        format!(
            "⟳ Auto-compacting ({} turns, ~{}k tokens > {}k limit)…",
            state.history.len(),
            est_tokens / 1000,
            state.context_budget.compact_trigger() / 1000,
        )
        .dim()
    );

    let mut auto_pm_compact = PermissionManager::new(true);
    let compact_result = stream_chat_sse(ChatTurnParams {
        client: ctx.client,
        base: ctx.base,
        token,
        message: prompts::COMPACT_SUMMARY_REQUEST,
        session_id: state.session_id.as_deref(),
        model: state.model.as_deref(),
        explain: ExplainMode::Off,
        render_md: false,
        history: &state.history,
        perm_manager: &mut auto_pm_compact,
        verbose_mode: false,
        quiet: true,
        selector: ctx.selector,
        recent_tools: &[],
    })
    .await;

    if let Ok(result) = compact_result {
        apply_auto_compact_result(state, ctx, token, result).await?;
    }

    Ok(())
}

pub(super) fn history_as_messages(history: &[(String, String)]) -> Vec<serde_json::Value> {
    history
        .iter()
        .flat_map(|(user, assistant)| {
            if user.is_empty() {
                vec![serde_json::json!({"role":"assistant","content":assistant})]
            } else {
                vec![
                    serde_json::json!({"role":"user","content":user}),
                    serde_json::json!({"role":"assistant","content":assistant}),
                ]
            }
        })
        .collect()
}

async fn apply_auto_compact_result(
    state: &mut ReplState,
    ctx: &ReplTurnContext<'_>,
    token: &str,
    result: StreamResult,
) -> Result<(), String> {
    let summary = result.full_text.trim().to_string();
    if summary.is_empty() {
        return Ok(());
    }

    let keep = state.context_budget.keep_recent_turns;
    let total = state.history.len();
    let trimmed = total.saturating_sub(keep);
    if trimmed == 0 {
        return Ok(());
    }

    let summary_entry = (
        String::new(),
        format!("[Prior context — {trimmed} turns compacted]\n\n{summary}"),
    );
    let mut new_history = vec![summary_entry];
    new_history.extend_from_slice(&state.history[trimmed..]);
    state.history = new_history;

    let entry = prompts::memory_proto::MemoryEntry::new(
        prompts::memory_proto::NS_EPISODE,
        prompts::memory_proto::ST_AUTO,
        &summary,
    );
    let meta = prompts::memory_proto::EntryMeta::from_session(
        state.session_id.as_deref(),
        state.turn,
        prompts::memory_proto::SRC_AUTO_COMPACT,
    );
    let _ = ctx
        .client
        .post(format!("{}/memory/store", ctx.base))
        .headers(auth_headers(token)?)
        .json(&entry.to_store_payload_with_meta(&meta))
        .send()
        .await;

    eprintln!(
        "  {}",
        format!(
            "✓ Compacted {trimmed} turns → {} in context",
            state.history.len()
        )
        .green()
    );

    Ok(())
}

async fn run_chat_turn(
    state: &mut ReplState,
    ctx: &ReplTurnContext<'_>,
    token: &str,
    message: &str,
    session_id: Option<&str>,
) -> TurnAttempt {
    tokio::select! {
        result = stream_chat_sse(ChatTurnParams {
            client: ctx.client,
            base: ctx.base,
            token,
            message,
            session_id,
            model: state.model.as_deref(),
            explain: state.explain,
            render_md: true,
            history: &state.history,
            perm_manager: &mut state.perm_manager,
            verbose_mode: state.verbose_mode,
            quiet: false,
            selector: ctx.selector,
            recent_tools: &state.recent_tools,
        }) => TurnAttempt::Completed(result),
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\n{}", "  Interrupted.".dim());
            TurnAttempt::Interrupted
        }
    }
}

fn apply_turn_success(
    state: &mut ReplState,
    profile: Option<&str>,
    line: &str,
    result: StreamResult,
    turn_start: Instant,
) {
    if let Some(session_id) = result.session_id.as_deref() {
        persist_last_session_id(profile, session_id);
        initialize_journal(state, session_id);
        state.session_id = Some(session_id.to_string());
        state.run_id = result.run_id.clone();
    }

    state.turn += 1;
    state.total_prompt_tokens += result.prompt_tokens;
    state.total_completion_tokens += result.completion_tokens;
    state.last_response = Some(result.full_text.clone());
    state
        .history
        .push((line.to_string(), result.full_text.clone()));
    state.recent_tools = result.tools_used.clone();

    if let Some(journal) = state.journal.as_ref() {
        let _ = journal.append(
            &session_journal::JournalEvent::turn(
                state.session_id.as_deref(),
                state.turn,
                state.model.as_deref(),
                line,
                &result.full_text,
                result.tool_calls_count,
                result.prompt_tokens,
                result.completion_tokens,
                turn_start.elapsed().as_millis() as u64,
            )
            .with_tool_selection(
                result.tools_selected.clone(),
                result.tools_used.clone(),
                result.budget_used,
            ),
        );
    }

    if result.tool_calls_count == 0 && looks_like_factual_query(line) {
        eprintln!(
            "{}",
            "  ⚠ Warning: This answer was generated without tool calls. Data may be hallucinated."
                .yellow()
        );
    }
}

fn persist_last_session_id(profile: Option<&str>, session_id: &str) {
    let mut creds = load_credentials();
    let name = profile_name(profile, &creds);
    let entry = creds.profiles.entry(name).or_default();
    entry.last_session_id = Some(session_id.to_string());
    let _ = save_credentials(&creds);
}

fn initialize_journal(state: &mut ReplState, session_id: &str) {
    if state.journal.is_some() {
        return;
    }

    state.journal = session_journal::JournalWriter::new(session_id).ok();
    if let Some(journal) = state.journal.as_ref() {
        let _ = journal.append(&session_journal::JournalEvent::session_start(
            Some(session_id),
            state.model.as_deref(),
        ));
    }
}

fn report_turn_error(state: &ReplState, line: &str, error: &str, turn_start: Instant) {
    let lower = error.to_lowercase();
    if error.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("could not validate credentials")
    {
        eprintln!("{}", "  Session expired. Run /login to refresh.".yellow());
    } else {
        eprintln!("{}", format!("  ✗  {error}").red());
    }

    if let Some(journal) = state.journal.as_ref() {
        let _ = journal.append(&session_journal::JournalEvent::turn_error(
            state.session_id.as_deref(),
            state.turn + 1,
            state.model.as_deref(),
            line,
            error,
            turn_start.elapsed().as_millis() as u64,
        ));
    }
}
