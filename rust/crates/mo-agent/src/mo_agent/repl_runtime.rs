use super::*;

pub(super) fn create_tool_selector(
    client: &reqwest::Client,
    base: &str,
    profile: Option<&str>,
) -> Box<dyn tool_selector::ToolSelector> {
    let all_schemas = edge_tools::all_tool_schemas();
    let registry = tool_registry::ToolRegistry::new(all_schemas);
    let tfidf = tool_selector::TfIdfSelector::new(registry);

    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    let token = creds
        .profiles
        .get(&name)
        .and_then(|p| p.access_token.as_ref())
        .cloned();

    match token {
        Some(tok) => {
            let llm = tool_selector::LlmToolSelector::new(client.clone(), base.to_string(), tok);
            Box::new(tool_selector::FallbackSelector::new(
                Box::new(llm),
                Box::new(tfidf),
            ))
        }
        None => Box::new(tfidf),
    }
}

pub(super) async fn ensure_repl_authenticated(
    client: &reqwest::Client,
    base: &str,
    profile: Option<&str>,
) -> Result<(), String> {
    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    let has_token = creds
        .profiles
        .get(&name)
        .and_then(|p| p.access_token.as_ref())
        .is_some();

    if has_token {
        return Ok(());
    }

    eprintln!();
    eprintln!("  {}", "⚠  Not logged in".yellow().bold());
    eprintln!();
    eprintln!("    {}  Login", "1".bold());
    eprintln!("    {}  Register", "2".bold());
    eprintln!("    {}  Exit", "3".bold());
    eprintln!();
    print!("  Choose [1/2/3]: ");
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut choice = String::new();
    io::stdin()
        .read_line(&mut choice)
        .map_err(|e| e.to_string())?;
    match choice.trim() {
        "1" => {
            print!("Username: ");
            io::stdout().flush().ok();
            let mut un = String::new();
            io::stdin().read_line(&mut un).ok();
            print!("Password: ");
            io::stdout().flush().ok();
            let pw = rpassword::read_password().unwrap_or_default();
            do_login(client, base, profile, un.trim(), pw.trim()).await?;
            eprintln!("{}", "  ✓  Logged in".green());
        }
        "2" => {
            print!("Username: ");
            io::stdout().flush().ok();
            let mut un = String::new();
            io::stdin().read_line(&mut un).ok();
            print!("Email: ");
            io::stdout().flush().ok();
            let mut em = String::new();
            io::stdin().read_line(&mut em).ok();
            print!("Password: ");
            io::stdout().flush().ok();
            let pw = rpassword::read_password().unwrap_or_default();
            do_register(client, base, un.trim(), em.trim(), pw.trim()).await?;
            do_login(client, base, profile, un.trim(), pw.trim()).await?;
            eprintln!("{}", "  ✓  Registered and logged in".green());
        }
        _ => {
            println!("{}", "Goodbye.".dim());
            return Err("repl exited before authentication".to_string());
        }
    }

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
    let mut state = ReplState::default();
    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    if let Some(p) = creds.profiles.get(&name) {
        state.session_id = p.last_session_id.clone();
        // Restore conversation history from local journal so context survives restarts.
        if let Some(ref sid) = p.last_session_id {
            state.history = restore_history_from_journal(sid);
        }
    }
    if let Some(m) = initial_model {
        state.model = Some(m.to_string());
    }
    state
}

/// Rebuild `(user_msg, assistant_msg)` history from the session journal.
/// Only `Turn` events with both user_input and assistant_output are included.
pub(super) fn restore_history_from_journal(session_id: &str) -> Vec<(String, String)> {
    let Ok(events) = session_journal::read_journal(session_id) else {
        return Vec::new();
    };
    events
        .into_iter()
        .filter_map(|e| {
            if e.event_type == session_journal::JournalEventType::Turn {
                Some((
                    e.user_input.unwrap_or_default(),
                    e.assistant_output.unwrap_or_default(),
                ))
            } else {
                None
            }
        })
        .collect()
}

pub(super) fn print_repl_banner(profile: Option<&str>, state: &ReplState) {
    let creds = load_credentials();
    let pname = profile_name(profile, &creds);
    let p = creds.profiles.get(&pname);
    let user_display = p
        .and_then(|p| p.username.as_deref())
        .unwrap_or("not logged in");
    let session_display = state
        .session_id
        .as_deref()
        .map(|s| if s.len() > 12 { &s[..12] } else { s })
        .unwrap_or("new");
    let model_display = state.model.as_deref().unwrap_or("default");
    let logged_in = p.and_then(|p| p.access_token.as_ref()).is_some();
    let version = env!("CARGO_PKG_VERSION");

    let lines_plain = [
        format!("  mo-agent  v{version}"),
        format!(
            "  profile: {}  user: {}  model: {}  session: {}",
            pname, user_display, model_display, session_display
        ),
        "  / commands · Tab complete · Ctrl+R history · ↑↓ recall · ^D exit".to_string(),
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
            session_display.dim(),
        ),
        format!(
            "  {}",
            "/ commands · Tab complete · Ctrl+R history · ↑↓ recall · ^D exit".dim()
        ),
    ];

    let row = |colored: &str, plain_len: usize| {
        let pad = w.saturating_sub(plain_len);
        format!("{} {colored}{} {}", "│".cyan(), " ".repeat(pad), "│".cyan())
    };

    let hr = "─".repeat(w + 2);

    eprintln!();
    eprintln!("{}", format!("╭{hr}╮").cyan());
    eprintln!("{}", row(&lines_colored[0], lines_plain[0].chars().count()));
    eprintln!("{}", row(&lines_colored[1], lines_plain[1].chars().count()));
    eprintln!("{}", format!("├{hr}┤").cyan().dim());
    eprintln!("{}", row(&lines_colored[2], lines_plain[2].chars().count()));
    eprintln!("{}", format!("╰{hr}╯").cyan());
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
}
