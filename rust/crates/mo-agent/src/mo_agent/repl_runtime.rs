use super::*;

pub(super) fn create_tool_selector(
    client: &reqwest::Client,
    base: &str,
    profile: Option<&str>,
) -> (Box<dyn tool_selector::ToolSelector>, PipelineModules) {
    create_tool_selector_with_quality(client, base, profile, None, None)
}

/// Shared pipeline learning modules — kept accessible for cross-session persistence.
pub(super) struct PipelineModules {
    pub entity_graph:
        std::sync::Arc<std::sync::Mutex<mo_agent_runtime::pipeline::entity::EntityGraph>>,
    pub pattern_library:
        std::sync::Arc<std::sync::Mutex<mo_agent_runtime::pipeline::pattern::PatternLibrary>>,
    pub calibrator: std::sync::Arc<
        std::sync::Mutex<mo_agent_runtime::pipeline::calibration::ProgressiveCalibrator>,
    >,
}

pub(super) fn create_tool_selector_with_quality(
    client: &reqwest::Client,
    base: &str,
    profile: Option<&str>,
    quality_tracker: Option<std::sync::Arc<std::sync::Mutex<tool_registry::ToolQualityTracker>>>,
    confidence_calibrator: Option<
        std::sync::Arc<mo_agent_runtime::turn::routing_metrics::ConfidenceCalibrator>,
    >,
) -> (Box<dyn tool_selector::ToolSelector>, PipelineModules) {
    use mo_agent_runtime::pipeline::{
        calibration::ProgressiveCalibrator, entity::EntityGraph, pattern::PatternLibrary,
    };

    let all_schemas = edge_tools::all_tool_schemas();
    let mut registry = tool_registry::ToolRegistry::new(all_schemas);

    // Load skill manifests from skills/ directory and register plugin tools
    let mut plugin_registry = tool_registry::PluginRegistry::new();
    manifest_loader::load_skills_directory(&mut plugin_registry);
    registry.register_plugins(&plugin_registry);

    let mut tfidf = tool_selector::TfIdfSelector::new(registry);
    if let Some(qt) = quality_tracker {
        tfidf = tfidf.with_quality_tracker(qt);
    }
    if let Some(cal) = confidence_calibrator {
        tfidf = tfidf.with_confidence_calibrator(cal);
    }

    // Wire pipeline learning modules for progressive improvement
    let entity_graph = std::sync::Arc::new(std::sync::Mutex::new(EntityGraph::new()));
    let pattern_library = std::sync::Arc::new(std::sync::Mutex::new(PatternLibrary::new()));
    let calibrator = std::sync::Arc::new(std::sync::Mutex::new(ProgressiveCalibrator::new(0.15)));
    tfidf = tfidf
        .with_entity_graph(entity_graph.clone())
        .with_pattern_library(pattern_library.clone())
        .with_progressive_calibrator(calibrator.clone());

    let modules = PipelineModules {
        entity_graph,
        pattern_library,
        calibrator,
    };

    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    let token = creds
        .profiles
        .get(&name)
        .and_then(|p| p.access_token.as_ref())
        .cloned();

    let selector: Box<dyn tool_selector::ToolSelector> = match token {
        Some(tok) => {
            let llm = tool_selector::LlmToolSelector::new(client.clone(), base.to_string(), tok);
            Box::new(tool_selector::FallbackSelector::new(
                Box::new(llm),
                Box::new(tfidf),
            ))
        }
        None => Box::new(tfidf),
    };

    (selector, modules)
}

pub(super) async fn ensure_repl_authenticated(
    client: &reqwest::Client,
    base: &str,
    profile: Option<&str>,
) -> Result<(), String> {
    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    let prof = creds.profiles.get(&name);

    // ── 1. Try existing access_token ───────────────────────────────────────
    if let Some(token) = prof.and_then(|p| p.access_token.as_ref()) {
        // Quick validation: /auth/me — catch expired tokens early
        match client
            .get(format!("{base}/auth/me"))
            .headers(auth_headers(token)?)
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED => {
                // Token expired — try refresh below
            }
            Ok(_) => return Ok(()), // Non-401 errors: proceed anyway (server issues)
            Err(_) => {
                eprintln!(
                    "{}",
                    "  ⚠ Network unavailable, using cached credentials.".dim()
                );
                return Ok(());
            }
        }
    }

    // ── 2. Try refresh_token if access_token is expired ────────────────────
    if let Some(refresh) = prof.and_then(|p| p.refresh_token.as_ref()) {
        match try_refresh_token(client, base, profile, refresh).await {
            Ok(_) => {
                eprintln!("  {} Token refreshed", "✓".green());
                return Ok(());
            }
            Err(_) => {
                // Refresh failed — fall through to interactive login
                eprintln!("{}", "  ⚠ Session expired, login required.".dim());
            }
        }
    }

    // ── 3. Interactive auth (first-time or expired credentials) ────────────
    eprintln!();
    eprintln!(
        "  {}  {}",
        "Welcome to mo-agent!".cyan().bold(),
        "Sign in to get started.".dim()
    );
    eprintln!();

    // Check if user has a stored username (returning user)
    let has_account = prof.and_then(|p| p.username.as_ref()).is_some();

    if has_account {
        // Returning user with expired session — go straight to login
        let stored_username = prof
            .and_then(|p| p.username.as_ref())
            .map(|s| s.as_str())
            .unwrap_or("");
        eprintln!(
            "  {} {}",
            "Session expired for".dim(),
            stored_username.bold()
        );
        eprintln!();

        print!("  {} ", "Password:".bold());
        io::stdout().flush().map_err(|e| e.to_string())?;
        let pw = rpassword::read_password().map_err(|e| format!("Could not read password: {e}"))?;
        if pw.trim().is_empty() {
            return Err("Authentication cancelled.".to_string());
        }
        do_login(client, base, profile, stored_username, pw.trim()).await?;
        eprintln!("  {} {}", "✓".green().bold(), "Logged in!".green());
        return Ok(());
    }

    // New user — offer login or register
    eprintln!("  {}  Login with existing account", "L".cyan().bold());
    eprintln!("  {}  Register a new account", "R".cyan().bold());
    eprintln!("  {}  {}", "Q".cyan().bold(), "Quit".dim());
    eprintln!();
    print!("  {} ", "Choice (L/R/Q):".bold());
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut choice = String::new();
    io::stdin()
        .read_line(&mut choice)
        .map_err(|e| e.to_string())?;
    match choice.trim().to_lowercase().as_str() {
        "l" | "login" | "1" => {
            eprintln!();
            print!("  {} ", "Username:".bold());
            io::stdout().flush().ok();
            let mut un = String::new();
            io::stdin().read_line(&mut un).ok();
            if un.trim().is_empty() {
                return Err("Authentication cancelled.".to_string());
            }
            print!("  {} ", "Password:".bold());
            io::stdout().flush().ok();
            let pw =
                rpassword::read_password().map_err(|e| format!("Could not read password: {e}"))?;
            if pw.trim().is_empty() {
                return Err("Authentication cancelled.".to_string());
            }
            do_login(client, base, profile, un.trim(), pw.trim()).await?;
            eprintln!("  {} {}", "✓".green().bold(), "Logged in!".green());
        }
        "r" | "register" | "2" => {
            eprintln!();
            print!("  {} ", "Username:".bold());
            io::stdout().flush().ok();
            let mut un = String::new();
            io::stdin().read_line(&mut un).ok();
            if un.trim().is_empty() {
                return Err("Registration cancelled.".to_string());
            }
            print!("  {} ", "Email:   ".bold());
            io::stdout().flush().ok();
            let mut em = String::new();
            io::stdin().read_line(&mut em).ok();
            if em.trim().is_empty() {
                return Err("Registration cancelled.".to_string());
            }
            print!("  {} ", "Password:".bold());
            io::stdout().flush().ok();
            let pw =
                rpassword::read_password().map_err(|e| format!("Could not read password: {e}"))?;
            if pw.trim().is_empty() {
                return Err("Registration cancelled.".to_string());
            }
            do_register(client, base, un.trim(), em.trim(), pw.trim()).await?;
            do_login(client, base, profile, un.trim(), pw.trim()).await?;
            eprintln!(
                "  {} {}",
                "✓".green().bold(),
                "Account created! You're all set.".green()
            );
        }
        _ => {
            return Err("Authentication cancelled.".to_string());
        }
    }

    Ok(())
}

/// Try to refresh an expired access token using the stored refresh_token.
async fn try_refresh_token(
    client: &reqwest::Client,
    base: &str,
    profile: Option<&str>,
    refresh_token: &str,
) -> Result<(), String> {
    let resp = client
        .post(format!("{base}/auth/refresh"))
        .header(CONTENT_TYPE, "application/json")
        .json(&serde_json::json!({ "refresh_token": refresh_token }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("refresh failed: {status}"));
    }
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let new_access = value
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("missing access_token")?;
    let new_refresh = value
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or("missing refresh_token")?;
    let mut creds = load_credentials();
    let name = profile_name(profile, &creds);
    let entry = creds.profiles.entry(name).or_default();
    entry.access_token = Some(new_access.to_string());
    entry.refresh_token = Some(new_refresh.to_string());
    save_credentials(&creds)?;
    Ok(())
}

pub(super) fn build_repl_editor() -> Result<(Editor<ReplHelper, FileHistory>, PathBuf), String> {
    let hist_path = history_path();
    if let Some(parent) = hist_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let config = Config::builder()
        .completion_type(CompletionType::List)
        .build();
    let mut editor: Editor<ReplHelper, FileHistory> =
        Editor::with_config(config).map_err(|e| e.to_string())?;
    editor.set_helper(Some(ReplHelper));
    editor.bind_sequence(
        RlEvent::Any,
        RlEventHandler::Conditional(Box::new(SlashStartCompleteHandler)),
    );
    let _ = editor.load_history(&hist_path);
    Ok((editor, hist_path))
}

pub(super) fn initialize_repl_state(
    profile: Option<&str>,
    initial_model: Option<&str>,
) -> ReplState {
    let mut state = ReplState {
        session_id: resumable_last_session_id(profile),
        ..Default::default()
    };
    // Restore session state from local journal only for resumable sessions.
    if let Some(ref sid) = state.session_id {
        let restored = restore_session_state_from_journal(sid);
        state.history = restored.history;
        state.turn = restored.turn;
        state.recent_tools = restored.recent_tools;
        state.total_prompt_tokens = restored.total_prompt_tokens;
        state.total_completion_tokens = restored.total_completion_tokens;

        // Enrich with step checkpoint data if available (blocked tools, progress)
        if let Ok(Some(heavy)) =
            mo_agent_runtime::pipeline::step_checkpoint::read_latest_heavy_checkpoint(sid)
        {
            // Merge blocked tools from checkpoint (tools that were deprioritized)
            if !heavy.blocked_tools.is_empty() && state.recent_tools.is_empty() {
                // Only use checkpoint's recent_tools as fallback
                state.recent_tools = heavy.recent_tools;
            }
        }
    }
    if let Some(m) = initial_model {
        state.model = Some(m.to_string());
    }

    // Initialize local task service
    let tasks_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".mo-agent")
        .join("tasks");
    state.task_service = Some(std::sync::Arc::new(
        mo_agent_services::LocalTaskService::new(tasks_dir),
    ));

    state
}

#[derive(Debug, Default, PartialEq)]
struct RestoredSessionState {
    history: Vec<(String, String)>,
    turn: u32,
    recent_tools: Vec<String>,
    total_prompt_tokens: u64,
    total_completion_tokens: u64,
}

/// Rebuild `(user_msg, assistant_msg)` history from the session journal.
/// Only `Turn` events with both user_input and assistant_output are included.
pub(super) fn restore_history_from_journal(session_id: &str) -> Vec<(String, String)> {
    restore_session_state_from_journal(session_id).history
}

fn restore_session_state_from_journal(session_id: &str) -> RestoredSessionState {
    let Ok(events) = session_journal::read_journal(session_id) else {
        return RestoredSessionState::default();
    };

    let mut restored = RestoredSessionState::default();
    let start_idx = events
        .iter()
        .rposition(|event| event.event_type == session_journal::JournalEventType::SessionStart)
        .map(|idx| idx + 1)
        .unwrap_or(0);

    for event in events.into_iter().skip(start_idx) {
        if event.event_type != session_journal::JournalEventType::Turn {
            continue;
        }
        restored.history.push((
            event.user_input.unwrap_or_default(),
            event.assistant_output.unwrap_or_default(),
        ));
        restored.turn += 1;
        restored.total_prompt_tokens += event.tokens_in.unwrap_or(0);
        restored.total_completion_tokens += event.tokens_out.unwrap_or(0);
        if let Some(tools_used) = event.tools_used {
            restored.recent_tools = tools_used;
        }
    }

    restored
}

pub(super) fn print_repl_banner(profile: Option<&str>, state: &ReplState) {
    let creds = load_credentials();
    let pname = profile_name(profile, &creds);
    let p = creds.profiles.get(&pname);
    let user_display = p
        .and_then(|p| p.username.as_deref())
        .unwrap_or("not logged in");
    // Session: show "new" for fresh sessions, truncated ID for resumed
    let session_display = banner_session_display(state);
    let model_display = state.model.as_deref().unwrap_or("auto");
    let logged_in = p.and_then(|p| p.access_token.as_ref()).is_some();
    let version = env!("CARGO_PKG_VERSION");

    let lines_plain = [
        format!("  mo-agent  v{version}"),
        format!(
            "  profile: {}  user: {}  model: {}  session: {}",
            pname, user_display, model_display, session_display
        ),
        "  / commands · Ctrl+R search · Alt+Enter multi-line · /keys all shortcuts".to_string(),
    ];
    let w = lines_plain
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(60)
        + 2;

    let lines_colored = [
        format!(
            "  {}  {}",
            "mo-agent".cyan().bold(),
            format!("v{version}").dim()
        ),
        format!(
            "  profile: {}  user: {}  model: {}  session: {}",
            pname.cyan(),
            if logged_in {
                user_display.dim().to_string()
            } else {
                user_display.yellow().to_string()
            },
            model_display.cyan(),
            session_display.as_str().dim().to_string(),
        ),
        format!(
            "  {}",
            "/ commands · Ctrl+R search · Alt+Enter multi-line · /keys all shortcuts".dim()
        ),
    ];

    let row = |colored: &str, plain_len: usize| {
        let pad = w.saturating_sub(plain_len);
        format!("{} {colored}{} {}", "│".cyan(), " ".repeat(pad), "│".cyan())
    };

    let hr = "─".repeat(w + 2);

    eprintln!();
    print_startup_logo();
    eprintln!("{}", format!("╭{hr}╮").cyan());
    eprintln!("{}", row(&lines_colored[0], lines_plain[0].chars().count()));
    eprintln!("{}", row(&lines_colored[1], lines_plain[1].chars().count()));
    eprintln!("{}", format!("├{hr}┤").cyan().dim());
    eprintln!("{}", row(&lines_colored[2], lines_plain[2].chars().count()));
    eprintln!("{}", format!("╰{hr}╯").cyan());
    eprintln!();
}

fn banner_session_display(state: &ReplState) -> String {
    match state.session_id.as_deref() {
        Some(s) => {
            let short = if s.len() > 8 { &s[..8] } else { s };
            if state.turn > 0 {
                format!("{short} (resumed)")
            } else {
                short.to_string()
            }
        }
        None => "new".to_string(),
    }
}

fn startup_logo_lines() -> &'static [&'static str] {
    &[
        "███╗   ███╗ ██████╗",
        "████╗ ████║██╔═══██╗",
        "██╔████╔██║██║   ██║",
        "██║╚██╔╝██║██║   ██║",
        "██║ ╚═╝ ██║╚██████╔╝",
        "╚═╝     ╚═╝ ╚═════╝ ",
    ]
}

#[cfg(test)]
fn startup_logo_frames() -> Vec<String> {
    let lines = startup_logo_lines();
    (0..lines.len())
        .map(|end| lines[..=end].join("\n"))
        .collect()
}

fn print_startup_logo() {
    use std::io::Write;
    use std::time::Duration;

    let logo_lines = startup_logo_lines();
    let animated = crossterm::terminal::size().is_ok()
        && std::env::var("NO_COLOR").is_err()
        && std::env::var("CI").is_err();

    if animated {
        let delay = Duration::from_millis(28);
        for line in logo_lines {
            eprintln!("  {}", line.cyan().bold());
            let _ = std::io::stderr().flush();
            std::thread::sleep(delay);
        }
        eprintln!("  {}", "agent runtime".cyan().dim());
        std::thread::sleep(Duration::from_millis(70));
    } else {
        for line in logo_lines {
            eprintln!("  {}", line.cyan().bold());
        }
        eprintln!("  {}", "agent runtime".cyan().dim());
    }
    eprintln!();
}

pub(super) fn current_access_token(profile: Option<&str>) -> Option<String> {
    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    creds
        .profiles
        .get(&name)
        .and_then(|p| p.access_token.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_utils::CredentialsFile;
    use tempfile::tempdir;

    #[test]
    fn restore_history_empty_for_unknown_session() {
        let history = restore_history_from_journal("nonexistent-session-xyz-123");
        assert!(history.is_empty());
    }

    #[test]
    fn restore_history_from_journal_roundtrip() {
        let sid = format!("test-restore-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();

        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "what is Rust?",
                "Rust is a systems language.",
                0,
                10,
                5,
                100,
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                2,
                None,
                "show me an example",
                "fn main() {}",
                0,
                8,
                4,
                80,
            ))
            .unwrap();

        let history = restore_history_from_journal(&sid);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].0, "what is Rust?");
        assert_eq!(history[0].1, "Rust is a systems language.");
        assert_eq!(history[1].0, "show me an example");
        // No cleanup needed — test sessions are ephemeral and won't affect production
    }

    #[test]
    fn restore_history_skips_non_turn_events() {
        let sid = format!("test-skip-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();

        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::config_change(
                Some(&sid),
                "model",
                "gpt-4o",
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "hello",
                "hi there",
                0,
                5,
                3,
                50,
            ))
            .unwrap();

        let history = restore_history_from_journal(&sid);
        assert_eq!(history.len(), 1, "only Turn events should be included");
        assert_eq!(history[0].0, "hello");
    }

    #[test]
    fn restore_session_state_recovers_turn_tools_and_tokens() {
        let sid = format!("test-state-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();

        writer
            .append(
                &session_journal::JournalEvent::turn(
                    Some(&sid),
                    1,
                    None,
                    "memoria 最新的一个ci?",
                    "ok",
                    1,
                    120,
                    30,
                    100,
                )
                .with_tool_selection(
                    vec!["github_ci_status".into()],
                    vec!["github_ci_status".into()],
                    30,
                ),
            )
            .unwrap();
        writer
            .append(
                &session_journal::JournalEvent::turn(
                    Some(&sid),
                    1,
                    None,
                    "pr呢？",
                    "ok",
                    1,
                    80,
                    20,
                    90,
                )
                .with_tool_selection(
                    vec!["github_list_prs".into()],
                    vec!["github_list_prs".into()],
                    35,
                ),
            )
            .unwrap();

        let restored = restore_session_state_from_journal(&sid);
        assert_eq!(
            restored.turn, 2,
            "turn should reflect restored conversation length"
        );
        assert_eq!(restored.total_prompt_tokens, 200);
        assert_eq!(restored.total_completion_tokens, 50);
        assert_eq!(restored.recent_tools, vec!["github_list_prs".to_string()]);
        assert_eq!(restored.history.len(), 2);
    }

    #[test]
    fn restore_session_state_uses_latest_session_segment() {
        let sid = format!("test-segment-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();

        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(
                &session_journal::JournalEvent::turn(
                    Some(&sid),
                    1,
                    None,
                    "old question",
                    "old answer",
                    0,
                    500,
                    50,
                    10,
                )
                .with_tool_selection(
                    vec!["git_log".into()],
                    vec!["git_log".into()],
                    10,
                ),
            )
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::session_end(Some(&sid), 1))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(
                &session_journal::JournalEvent::turn(
                    Some(&sid),
                    1,
                    None,
                    "latest question",
                    "latest answer",
                    0,
                    80,
                    20,
                    10,
                )
                .with_tool_selection(
                    vec!["github_ci_status".into()],
                    vec!["github_ci_status".into()],
                    20,
                ),
            )
            .unwrap();

        let restored = restore_session_state_from_journal(&sid);
        assert_eq!(
            restored.history,
            vec![("latest question".into(), "latest answer".into())]
        );
        assert_eq!(restored.turn, 1);
        assert_eq!(restored.total_prompt_tokens, 80);
        assert_eq!(restored.total_completion_tokens, 20);
        assert_eq!(restored.recent_tools, vec!["github_ci_status".to_string()]);
    }

    #[test]
    fn initialize_repl_state_skips_cleanly_ended_session() {
        let creds_dir = tempdir().unwrap();
        unsafe {
            std::env::set_var("MO_AGENT_CREDENTIALS_DIR", creds_dir.path());
        }

        let sid = format!("test-ended-init-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "old question",
                "old answer",
                0,
                20,
                10,
                10,
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::session_end(Some(&sid), 1))
            .unwrap();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(sid),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let state = initialize_repl_state(None, Some("gpt-5"));
        assert_eq!(state.session_id, None);
        assert!(state.history.is_empty());
        assert_eq!(state.turn, 0);

        unsafe {
            std::env::remove_var("MO_AGENT_CREDENTIALS_DIR");
        }
    }

    // ── Session display logic ──────────────────────────────────────────────

    #[test]
    fn session_display_shows_new_for_none() {
        let state = ReplState::default();
        assert_eq!(banner_session_display(&state), "new");
    }

    #[test]
    fn session_display_shows_truncated_id_for_fresh_session() {
        let mut state = ReplState::default();
        state.session_id = Some("abcdef12-3456-7890".to_string());
        state.turn = 0;
        assert_eq!(banner_session_display(&state), "abcdef12");
    }

    #[test]
    fn session_display_shows_resumed_for_restored_session() {
        let mut state = ReplState::default();
        state.session_id = Some("abcdef12-3456-7890".to_string());
        state.turn = 3; // Has prior turns → resumed
        assert_eq!(banner_session_display(&state), "abcdef12 (resumed)");
    }

    #[test]
    fn model_display_shows_auto_when_none() {
        let state = ReplState::default();
        let display = state.model.as_deref().unwrap_or("auto");
        assert_eq!(display, "auto");
    }

    #[test]
    fn model_display_shows_actual_name_when_set() {
        let mut state = ReplState::default();
        state.model = Some("gpt-5".to_string());
        let display = state.model.as_deref().unwrap_or("auto");
        assert_eq!(display, "gpt-5");
    }

    #[test]
    fn startup_logo_has_multiple_lines_and_brand_shape() {
        let lines = startup_logo_lines();
        assert!(lines.len() >= 5);
        assert!(lines.iter().all(|line| !line.trim().is_empty()));
        assert!(lines[0].contains("███"));
        assert!(lines.iter().any(|line| line.contains("╚═════╝")));
    }

    #[test]
    fn startup_logo_frames_progressively_reveal_logo() {
        let lines = startup_logo_lines();
        let frames = startup_logo_frames();
        assert_eq!(frames.len(), lines.len());
        assert_eq!(frames[0], lines[0]);
        assert_eq!(frames.last().unwrap(), &lines.join("\n"));
        for (idx, frame) in frames.iter().enumerate() {
            assert_eq!(frame.lines().count(), idx + 1);
        }
    }
}
