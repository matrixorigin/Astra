use super::*;

#[derive(Debug, Serialize, Deserialize, Default)]
pub(super) struct CredentialsFile {
    pub(super) current_profile: Option<String>,
    pub(super) profiles: HashMap<String, Profile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(super) struct Profile {
    pub(super) username: Option<String>,
    pub(super) access_token: Option<String>,
    pub(super) refresh_token: Option<String>,
    pub(super) last_session_id: Option<String>,
    pub(super) memoria_api_key: Option<String>,
}

pub(super) fn credentials_path() -> PathBuf {
    // Allow tests to override the credentials path via env var to avoid polluting real credentials.
    if let Ok(dir) = std::env::var("ASTRA_CREDENTIALS_DIR") {
        return PathBuf::from(dir).join("credentials.json");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".astra")
        .join("credentials.json")
}

/// Fetch relevant memories from Memoria synchronously and return as a string for context injection.
/// Returns empty string if Memoria is not configured or unavailable.
pub(super) fn load_credentials() -> CredentialsFile {
    let path = credentials_path();
    let Ok(content) = fs::read_to_string(path) else {
        return CredentialsFile::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

pub(super) fn save_credentials(data: &CredentialsFile) -> Result<(), String> {
    let path = credentials_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(data).map_err(|e| e.to_string())?;
    fs::write(path, body).map_err(|e| e.to_string())
}

pub(super) fn profile_name(cli_profile: Option<&str>, data: &CredentialsFile) -> String {
    cli_profile
        .map(ToString::to_string)
        .or_else(|| data.current_profile.clone())
        .unwrap_or_else(|| "default".to_string())
}

pub(super) fn get_profile_and_token(
    cli_profile: Option<&str>,
) -> Result<(CredentialsFile, String, Profile, String), String> {
    let creds = load_credentials();
    let name = profile_name(cli_profile, &creds);
    let profile = creds
        .profiles
        .get(&name)
        .cloned()
        .ok_or_else(|| format!("no profile '{name}', run login first"))?;
    let token = profile
        .access_token
        .clone()
        .ok_or_else(|| format!("profile '{name}' is not logged in"))?;
    Ok((creds, name, profile, token))
}

pub(super) fn session_is_resumable(session_id: &str) -> bool {
    match session_journal::read_journal(session_id) {
        Ok(events) if events.is_empty() => true,
        Ok(events) => {
            let last_start = events.iter().rposition(|event| {
                event.event_type == session_journal::JournalEventType::SessionStart
            });
            let last_end = events.iter().rposition(|event| {
                event.event_type == session_journal::JournalEventType::SessionEnd
            });
            match (last_start, last_end) {
                (Some(start_idx), Some(end_idx)) => start_idx > end_idx,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => true,
            }
        }
        Err(_) => true,
    }
}

pub(super) fn resumable_last_session_id(cli_profile: Option<&str>) -> Option<String> {
    let creds = load_credentials();
    let name = profile_name(cli_profile, &creds);
    creds
        .profiles
        .get(&name)
        .and_then(|profile| profile.last_session_id.clone())
        .filter(|session_id| session_is_resumable(session_id))
}

pub(super) fn read_api_error(status: u16, body: &str) -> String {
    format!("request failed ({status}): {}", compact_or_raw(body))
}

pub(super) fn map_thin_err(e: astra_thin_client::ThinClientError) -> String {
    match e {
        astra_thin_client::ThinClientError::Api { status, body } => {
            read_api_error(status.as_u16(), &body)
        }
        other => other.to_string(),
    }
}

pub(super) fn compact_or_raw(body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => value.to_string(),
        Err(_) => body.to_string(),
    }
}

pub(super) fn print_json_or_raw(body: &str) {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
        );
    } else {
        println!("{body}");
    }
}

/// Prompt user for a required string value. Uses the provided value if already set.
pub(super) fn prompt_or(label: &str, existing: Option<String>) -> Result<String, String> {
    if let Some(v) = existing {
        return Ok(v);
    }
    print!("  {}: ", label.cyan().bold());
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut val = String::new();
    io::stdin().read_line(&mut val).map_err(|e| e.to_string())?;
    let val = val.trim().to_string();
    if val.is_empty() {
        Err(format!("{label} cannot be empty"))
    } else {
        Ok(val)
    }
}

/// Prompt for a password with hidden input.
pub(super) fn prompt_password_masked(
    label: &str,
    existing: Option<String>,
) -> Result<String, String> {
    if let Some(v) = existing {
        return Ok(v);
    }
    print!("  {}: ", label.cyan().bold());
    io::stdout().flush().map_err(|e| e.to_string())?;
    let val = rpassword::read_password().map_err(|e| e.to_string())?;
    let val = val.trim().to_string();
    if val.is_empty() {
        Err(format!("{label} cannot be empty"))
    } else {
        Ok(val)
    }
}

/// Interactive list picker — renders below the prompt line, supports ↑/↓ navigation
/// and type-to-filter. Returns the selected item's label, or None on Esc/Ctrl-C.
///
/// Each item is `(label, description)`. The `current` marks the initially highlighted item.
pub(super) fn interactive_select(
    title: &str,
    items: &[(String, String)],
    current: Option<&str>,
) -> Option<String> {
    if items.is_empty() {
        eprintln!("{}", "  No items available.".yellow());
        return None;
    }

    let mut selected: usize = current
        .and_then(|c| items.iter().position(|i| i.0 == c))
        .unwrap_or(0);
    let mut filter = String::new();
    let mut prev_rendered_lines = 0usize;

    // Enter raw mode for key capture
    if terminal::enable_raw_mode().is_err() {
        return None;
    }

    let result = (|| -> Option<String> {
        loop {
            // Filter items
            let filtered: Vec<(usize, &(String, String))> = items
                .iter()
                .enumerate()
                .filter(|(_, (label, desc))| {
                    if filter.is_empty() {
                        true
                    } else {
                        let q = filter.to_lowercase();
                        label.to_lowercase().contains(&q) || desc.to_lowercase().contains(&q)
                    }
                })
                .collect();

            // Clamp selected
            if !filtered.is_empty() && selected >= filtered.len() {
                selected = filtered.len() - 1;
            }

            // Calculate max label width for alignment
            let max_label = filtered
                .iter()
                .map(|(_, (l, _))| l.len())
                .max()
                .unwrap_or(10);

            // Clear previous render
            for _ in 0..prev_rendered_lines {
                eprint!("{}", super::theme::CURSOR_UP_CLEAR);
            }
            let _ = io::stderr().flush();

            // Render: title + items
            let mut lines_rendered = 0usize;

            // Title line
            eprint!("\r  {}", title.bold());
            if !filter.is_empty() {
                eprint!(" {}", format!("(filter: {filter})").dim());
            }
            eprint!("\r\n");
            lines_rendered += 1;

            // Items
            for (idx, (_, (label, desc))) in filtered.iter().enumerate() {
                let is_current = current == Some(label.as_str());
                let marker = if idx == selected {
                    "❯"
                } else if is_current {
                    "*"
                } else {
                    " "
                };

                if idx == selected {
                    eprint!(
                        "  {} {:<width$}  {}\r\n",
                        marker.green().bold(),
                        label.as_str().green().bold(),
                        desc.as_str().dim(),
                        width = max_label,
                    );
                } else if is_current {
                    eprint!(
                        "  {} {:<width$}  {}\r\n",
                        marker.cyan(),
                        label.as_str().cyan(),
                        desc.as_str().dim(),
                        width = max_label,
                    );
                } else {
                    eprint!(
                        "  {} {:<width$}  {}\r\n",
                        marker,
                        label,
                        desc.as_str().dim(),
                        width = max_label,
                    );
                }
                lines_rendered += 1;
            }

            if filtered.is_empty() {
                eprint!("  {}\r\n", "No matches".dim());
                lines_rendered += 1;
            }

            let _ = io::stderr().flush();
            prev_rendered_lines = lines_rendered;

            // Read key
            let key = match event::read() {
                Ok(Event::Key(k)) => k,
                _ => continue,
            };

            match key {
                KeyEvent {
                    code: KeyCode::Up, ..
                }
                | KeyEvent {
                    code: KeyCode::BackTab,
                    ..
                } => {
                    if !filtered.is_empty() && selected > 0 {
                        selected -= 1;
                    }
                }
                KeyEvent {
                    code: KeyCode::Down,
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Tab, ..
                } => {
                    if !filtered.is_empty() && selected + 1 < filtered.len() {
                        selected += 1;
                    }
                }
                KeyEvent {
                    code: KeyCode::Enter,
                    ..
                } => {
                    if let Some((_, (label, _))) = filtered.get(selected) {
                        return Some(label.clone());
                    }
                    return None;
                }
                KeyEvent {
                    code: KeyCode::Esc, ..
                }
                | KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                } => {
                    return None;
                }
                KeyEvent {
                    code: KeyCode::Backspace,
                    ..
                } => {
                    filter.pop();
                    selected = 0;
                }
                KeyEvent {
                    code: KeyCode::Char(c),
                    modifiers,
                    ..
                } if !modifiers.contains(KeyModifiers::CONTROL) => {
                    filter.push(c);
                    selected = 0;
                }
                _ => {}
            }
        }
    })();

    // Clean up: clear the picker display and restore terminal
    for _ in 0..prev_rendered_lines {
        eprint!("{}", super::theme::CURSOR_UP_CLEAR);
    }
    let _ = io::stderr().flush();
    terminal::disable_raw_mode().ok();
    result
}
/// Best-effort terminal width for wrapping (matches SSE `term_width` default on error).
pub(crate) fn terminal_width_usize() -> usize {
    terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
        .max(20)
}

/// Render markdown with explicit wrap width so it matches [`StreamRenderState`] line accounting.
#[allow(dead_code)]
pub(crate) fn print_markdown_width(text: &str, width: Option<usize>) {
    let w = width.unwrap_or_else(terminal_width_usize).max(20);
    let mut skin = termimad::MadSkin::default();
    // Use crossterm colors so they match our existing palette
    use termimad::FmtText;
    use termimad::crossterm::style::Color;
    skin.bold.set_fg(Color::Cyan);
    skin.italic.set_fg(Color::Yellow);
    skin.inline_code.set_fg(Color::Green);
    let fmt = FmtText::from(&skin, text, Some(w));
    print!("{}", fmt);
}

pub(super) use astra_runtime::turn::headless_tool_status_display::truncate_str;

pub(super) fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '&' => "%26".to_string(),
            '=' => "%3D".to_string(),
            '#' => "%23".to_string(),
            _ => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ── urlencoding ───────────────────────────────────────────────────────────

    #[test]
    fn urlencoding_spaces() {
        assert_eq!(urlencoding("hello world"), "hello%20world");
    }

    #[test]
    fn urlencoding_special_chars() {
        assert_eq!(urlencoding("a&b=c#d"), "a%26b%3Dc%23d");
    }

    #[test]
    fn urlencoding_no_change() {
        assert_eq!(urlencoding("simple"), "simple");
    }

    // ── compact_or_raw ────────────────────────────────────────────────────────

    #[test]
    fn compact_or_raw_valid_json() {
        let result = compact_or_raw("{\"a\":1}");
        assert!(result.contains("\"a\""));
    }

    #[test]
    fn compact_or_raw_invalid_json() {
        let result = compact_or_raw("not json");
        assert_eq!(result, "not json");
    }

    // ── read_api_error ────────────────────────────────────────────────────────

    #[test]
    fn read_api_error_includes_status() {
        let err = read_api_error(404, "not found");
        assert!(err.contains("404"), "got: {err}");
    }

    // ── profile_name ──────────────────────────────────────────────────────────

    #[test]
    fn profile_name_uses_cli_override() {
        let creds = CredentialsFile::default();
        assert_eq!(profile_name(Some("staging"), &creds), "staging");
    }

    #[test]
    fn profile_name_uses_default_from_creds() {
        let creds = CredentialsFile {
            current_profile: Some("prod".to_string()),
            ..Default::default()
        };
        assert_eq!(profile_name(None, &creds), "prod");
    }

    #[test]
    fn profile_name_falls_back_to_default() {
        let creds = CredentialsFile::default();
        assert_eq!(profile_name(None, &creds), "default");
    }

    #[test]
    fn session_is_not_resumable_after_clean_end() {
        let sid = format!("test-ended-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::session_end(Some(&sid), 0))
            .unwrap();

        assert!(!session_is_resumable(&sid));
    }

    #[test]
    fn resumable_last_session_id_filters_ended_sessions() {
        let creds_dir = tempdir().unwrap();
        unsafe {
            std::env::set_var("ASTRA_CREDENTIALS_DIR", creds_dir.path());
        }

        let sid = format!("test-profile-ended-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::session_end(Some(&sid), 0))
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

        assert_eq!(resumable_last_session_id(None), None);

        unsafe {
            std::env::remove_var("ASTRA_CREDENTIALS_DIR");
        }
    }
}
