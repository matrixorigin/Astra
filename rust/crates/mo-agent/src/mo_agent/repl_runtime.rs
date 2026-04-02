use super::*;

pub(super) fn create_tool_selector(
    api: &mo_thin_client::ThinClient,
    profile: Option<&str>,
) -> (Box<dyn tool_selector::ToolSelector>, PipelineModules) {
    create_tool_selector_with_quality(api, profile, None, None)
}

pub(super) fn create_tool_selector_quiet(
    api: &mo_thin_client::ThinClient,
    profile: Option<&str>,
) -> (Box<dyn tool_selector::ToolSelector>, PipelineModules) {
    create_tool_selector_with_quality_internal(api, profile, None, None, false)
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
    /// Skill registry for progressive loading.
    pub skill_registry: std::sync::Arc<std::sync::RwLock<skill_instructions::SkillRegistry>>,
    /// MCP client manager for external tool servers.
    pub mcp_manager: std::sync::Arc<std::sync::RwLock<mcp_client::McpClientManager>>,
}

pub(super) fn create_tool_selector_with_quality(
    api: &mo_thin_client::ThinClient,
    profile: Option<&str>,
    quality_tracker: Option<std::sync::Arc<std::sync::Mutex<tool_registry::ToolQualityTracker>>>,
    confidence_calibrator: Option<
        std::sync::Arc<mo_agent_runtime::turn::routing_metrics::ConfidenceCalibrator>,
    >,
) -> (Box<dyn tool_selector::ToolSelector>, PipelineModules) {
    create_tool_selector_with_quality_internal(
        api,
        profile,
        quality_tracker,
        confidence_calibrator,
        true,
    )
}

fn create_tool_selector_with_quality_internal(
    api: &mo_thin_client::ThinClient,
    profile: Option<&str>,
    quality_tracker: Option<std::sync::Arc<std::sync::Mutex<tool_registry::ToolQualityTracker>>>,
    confidence_calibrator: Option<
        std::sync::Arc<mo_agent_runtime::turn::routing_metrics::ConfidenceCalibrator>,
    >,
    announce_skills: bool,
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

    // Initialize skill registry with progressive loading
    let skill_registry = std::sync::Arc::new(std::sync::RwLock::new(
        skill_instructions::SkillRegistry::new(),
    ));

    // Discover and register skill metadata from skills/ directories
    let skills_paths = [
        std::path::PathBuf::from("skills"),
        dirs::home_dir()
            .map(|h| h.join(".mo-agent").join("skills"))
            .unwrap_or_default(),
    ];
    for skills_path in &skills_paths {
        if skills_path.is_dir()
            && let Ok(mut reg) = skill_registry.write()
        {
            let registered =
                skill_instructions::discover_and_register_metadata(skills_path, &mut reg);
            if announce_skills && !registered.is_empty() {
                eprintln!(
                    "  {} Discovered {} skills from {:?}: {:?}",
                    "✓".green(),
                    registered.len(),
                    skills_path,
                    registered
                );
            }
        }
    }

    // Initialize MCP client manager (connections happen async later)
    let mcp_manager =
        std::sync::Arc::new(std::sync::RwLock::new(mcp_client::McpClientManager::new()));

    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    let token = creds
        .profiles
        .get(&name)
        .and_then(|p| p.access_token.as_ref())
        .cloned();

    // Build skill catalog entries for tool selection (before moving skill_registry)
    let skill_catalog: Vec<tool_selector::SkillCatalogEntry> = {
        match skill_registry.read() {
            Ok(reg) => reg
                .all_skills()
                .iter()
                .map(|s| tool_selector::SkillCatalogEntry {
                    name: s.metadata.name.clone(),
                    description: s.metadata.description.clone(),
                })
                .collect(),
            Err(_) => vec![],
        }
    };

    let modules = PipelineModules {
        entity_graph,
        pattern_library,
        calibrator,
        skill_registry,
        mcp_manager,
    };

    // Use LLM selector only when logged in, with TF-IDF as fast fallback.
    // FallbackSelector tries LLM first; if it fails or returns empty, uses TF-IDF.
    let selector: Box<dyn tool_selector::ToolSelector> = match token {
        Some(tok) => {
            // Register skills with LLM selector so it can include them in selection
            let llm = tool_selector::LlmToolSelector::new(api.clone(), tok.to_string())
                .with_skills(skill_catalog);
            Box::new(tool_selector::FallbackSelector::new(
                Box::new(llm),
                Box::new(tfidf),
            ))
        }
        None => Box::new(tfidf),
    };

    (selector, modules)
}

/// Quick check whether the server has at least one LLM model configured.
/// Returns `true` on network errors (optimistic — don't block startup).
pub(super) async fn check_server_has_models(api: &mo_thin_client::ThinClient, token: &str) -> bool {
    let resp = match api
        .get_models_response_timeout(token, std::time::Duration::from_secs(3))
        .await
    {
        Ok(r) if r.status().is_success() => r,
        _ => return true,
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return true,
    };
    if let Some(arr) = body.as_array() {
        return !arr.is_empty();
    }
    if let Some(arr) = body.get("models").and_then(|v| v.as_array()) {
        return !arr.is_empty();
    }
    true
}

/// Best-effort silent auth: validate existing token or try refresh.
/// Never blocks or prompts — just ensures credentials are fresh if possible.
/// Clears stale credentials when the server rejects them.
pub(super) async fn try_silent_auth(api: &mo_thin_client::ThinClient, profile: Option<&str>) {
    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    let prof = creds.profiles.get(&name);

    // Try existing access_token
    if let Some(token) = prof.and_then(|p| p.access_token.as_ref()) {
        match api
            .get_auth_me_text_timeout(token, std::time::Duration::from_secs(3))
            .await
        {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) if resp.status().is_client_error() => {
                // 4xx (401 expired, 404 user gone, etc.) — token is invalid
            }
            _ => return, // Network error or 5xx: proceed with cached creds
        }
    } else {
        return;
    }

    // Try refresh_token first
    if let Some(refresh) = prof.and_then(|p| p.refresh_token.as_ref())
        && try_refresh_token(api, profile, refresh).await.is_ok()
    {
        eprintln!("  {} Token refreshed", "✓".green());
        return;
    }

    // Refresh failed or no refresh token — clear stale credentials
    let mut creds = load_credentials();
    let name = profile_name(profile, &creds);
    if let Some(p) = creds.profiles.get_mut(&name) {
        p.access_token = None;
        p.refresh_token = None;
    }
    let _ = save_credentials(&creds);
}

/// Try to refresh an expired access token using the stored refresh_token.
async fn try_refresh_token(
    api: &mo_thin_client::ThinClient,
    profile: Option<&str>,
    refresh_token: &str,
) -> Result<(), String> {
    let body = api
        .post_auth_refresh_json(&serde_json::json!({ "refresh_token": refresh_token }))
        .await
        .map_err(|e| e.to_string())?;
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
pub struct RestoredSessionState {
    pub history: Vec<(String, String)>,
    pub turn: u32,
    pub recent_tools: Vec<String>,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
}

/// Rebuild `(user_msg, assistant_msg)` history from the session journal.
/// Only `Turn` events with both user_input and assistant_output are included.
pub(super) fn restore_history_from_journal(session_id: &str) -> Vec<(String, String)> {
    restore_session_state_from_journal(session_id).history
}

/// Full session counters + history from local JSONL (used after `/session fork`).
pub fn session_state_from_journal(session_id: &str) -> RestoredSessionState {
    restore_session_state_from_journal(session_id)
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

fn random_tips(logged_in: bool) -> [&'static str; 2] {
    use std::time::SystemTime;

    const TIPS: &[&str] = &[
        "Type / to browse all commands",
        "Ctrl+R to search command history",
        "Alt+Enter for multi-line input",
        "/explain toggles reasoning visibility",
        "/diff for colored unified git diff; /plan on for plan-only chat (no tools)",
        "/stats shows session token usage",
        "/tools shows tool call performance",
        "/health for tool health dashboard",
        "/resume to continue a previous session",
        "/session to see current session info",
        "/learn to see learning insights",
        "End a line with \\ to continue on next line",
        "/doctor runs diagnostics if something feels off",
        "/sync shows cloud sync status",
    ];

    const TIPS_NOT_LOGGED_IN: &[&str] = &[
        "/login to authenticate with existing account",
        "/register to create a new account",
        "Most features require login to work",
    ];

    let pool = if logged_in { TIPS } else { TIPS_NOT_LOGGED_IN };
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as usize)
        .unwrap_or(0);
    let i = seed % pool.len();
    let j = (i + 1 + seed / pool.len()) % pool.len();
    // Ensure two different tips
    let j = if j == i { (i + 1) % pool.len() } else { j };
    [pool[i], pool[j]]
}

/// Approximate display width: ASCII = 1, emoji/CJK = 2.
fn display_width(s: &str) -> usize {
    s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

pub(super) fn print_repl_banner(profile: Option<&str>, state: &ReplState) {
    let creds = load_credentials();
    let pname = profile_name(profile, &creds);
    let p = creds.profiles.get(&pname);
    let logged_in = p.and_then(|p| p.access_token.as_ref()).is_some();
    let user_display = match (p.and_then(|p| p.username.as_deref()), logged_in) {
        (Some(name), true) => name.to_string(),
        (Some(name), false) => format!("{name} (not logged in)"),
        (None, _) => "not logged in".to_string(),
    };
    let session_display = banner_session_display(state);
    let model_display = state.model.as_deref().unwrap_or("auto");
    let version = env!("CARGO_PKG_VERSION");

    let [tip1, tip2] = random_tips(logged_in);
    let tip1_plain = format!("  💡 {tip1}");
    let tip2_plain = format!("  💡 {tip2}");

    let lines_plain = [
        format!("  mo-agent  v{version}"),
        format!(
            "  profile: {}  user: {}  model: {}  session: {}",
            pname, user_display, model_display, session_display
        ),
        tip1_plain,
        tip2_plain,
    ];
    let w = lines_plain
        .iter()
        .map(|l| display_width(l))
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
            session_display.as_str().dim(),
        ),
        format!("  💡 {}", tip1.dim()),
        format!("  💡 {}", tip2.dim()),
    ];

    let row = |colored: &str, plain_width: usize| {
        let pad = w.saturating_sub(plain_width);
        format!("{} {colored}{} {}", "│".cyan(), " ".repeat(pad), "│".cyan())
    };

    let hr = "─".repeat(w + 2);

    eprintln!();
    print_startup_logo();
    eprintln!("{}", format!("╭{hr}╮").cyan());
    eprintln!("{}", row(&lines_colored[0], display_width(&lines_plain[0])));
    eprintln!("{}", row(&lines_colored[1], display_width(&lines_plain[1])));
    eprintln!("{}", format!("├{hr}┤").cyan().dim());
    eprintln!("{}", row(&lines_colored[2], display_width(&lines_plain[2])));
    eprintln!("{}", row(&lines_colored[3], display_width(&lines_plain[3])));
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
                    vec![],
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
                    vec![],
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
                    vec![],
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
                    vec![],
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
        let state = ReplState {
            session_id: Some("abcdef12-3456-7890".to_string()),
            ..Default::default()
        };
        assert_eq!(banner_session_display(&state), "abcdef12");
    }

    #[test]
    fn session_display_shows_resumed_for_restored_session() {
        let state = ReplState {
            session_id: Some("abcdef12-3456-7890".to_string()),
            turn: 3,
            ..Default::default()
        };
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
        let state = ReplState {
            model: Some("gpt-5".to_string()),
            ..Default::default()
        };
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
