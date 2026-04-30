use super::*;

#[derive(Debug, Serialize, Deserialize, Default)]
pub(super) struct CredentialsFile {
    pub(super) current_profile: Option<String>,
    pub(super) profiles: HashMap<String, Profile>,
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub(super) struct Profile {
    pub(super) username: Option<String>,
    pub(super) access_token: Option<String>,
    pub(super) refresh_token: Option<String>,
    pub(super) last_session_id: Option<String>,
    pub(super) memoria_api_key: Option<String>,
}

impl std::fmt::Debug for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Profile")
            .field("username", &self.username)
            .field("access_token", &self.access_token.as_ref().map(|_| "***"))
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "***"))
            .field("last_session_id", &self.last_session_id)
            .field(
                "memoria_api_key",
                &self.memoria_api_key.as_ref().map(|_| "***"),
            )
            .finish()
    }
}

pub(super) fn credentials_path() -> PathBuf {
    // Allow tests to override the credentials path via env var to avoid polluting real credentials.
    if let Ok(dir) = std::env::var("ASTRA_CLI_CREDENTIALS_DIR") {
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
    fs::write(&path, body).map_err(|e| e.to_string())?;
    // Restrict to owner-only (0o600) — credentials contain tokens and secrets
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
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
    match session_journal::classify_session_end_state(session_id) {
        Ok(session_journal::SessionEndState::Completed) => false,
        Ok(session_journal::SessionEndState::Interrupted { resumable, .. }) => resumable,
        Ok(session_journal::SessionEndState::Zombie) => true,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionResumePreflight {
    Valid,
    Missing,
    Unknown,
}

pub(super) fn clear_profile_last_session_if_matches(
    cli_profile: Option<&str>,
    session_id: &str,
) -> Result<bool, String> {
    let mut creds = load_credentials();
    let name = profile_name(cli_profile, &creds);
    let Some(entry) = creds.profiles.get_mut(&name) else {
        return Ok(false);
    };
    if entry.last_session_id.as_deref() != Some(session_id) {
        return Ok(false);
    }
    entry.last_session_id = None;
    save_credentials(&creds)?;
    Ok(true)
}

pub(super) async fn preflight_remote_resume_session(
    api: &astra_thin_client::ThinClient,
    cli_profile: Option<&str>,
    session_id: &str,
) -> SessionResumePreflight {
    let creds = load_credentials();
    let name = profile_name(cli_profile, &creds);
    let Some(token) = creds
        .profiles
        .get(&name)
        .and_then(|profile| profile.access_token.as_deref())
    else {
        return SessionResumePreflight::Unknown;
    };

    match api.get_session(Some(token), session_id).await {
        Ok(_) => SessionResumePreflight::Valid,
        Err(astra_thin_client::ThinClientError::Api { status, .. }) if status.as_u16() == 404 => {
            SessionResumePreflight::Missing
        }
        Err(_) => SessionResumePreflight::Unknown,
    }
}

pub(super) async fn validated_resumable_last_session_id(
    api: &astra_thin_client::ThinClient,
    cli_profile: Option<&str>,
) -> Option<String> {
    let session_id = resumable_last_session_id(cli_profile)?;
    match preflight_remote_resume_session(api, cli_profile, &session_id).await {
        SessionResumePreflight::Valid | SessionResumePreflight::Unknown => Some(session_id),
        SessionResumePreflight::Missing => {
            let _ = clear_profile_last_session_if_matches(cli_profile, &session_id);
            None
        }
    }
}

pub(super) const FULL_LLM_CAPTURE_METADATA_KEY: &str = "full_llm_capture";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionTraceState {
    pub session_id: String,
    pub enabled: bool,
}

fn session_metadata_object(
    session: &serde_json::Value,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    match session.get("metadata") {
        None | Some(serde_json::Value::Null) => Ok(serde_json::Map::new()),
        Some(serde_json::Value::Object(map)) => Ok(map.clone()),
        Some(_) => Err("session metadata must be a JSON object".to_string()),
    }
}

fn session_trace_state_from_value(
    session: &serde_json::Value,
) -> Result<SessionTraceState, String> {
    let session_id = session
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "session response is missing session_id".to_string())?
        .to_string();
    let metadata = session_metadata_object(session)?;
    let enabled = metadata
        .get(FULL_LLM_CAPTURE_METADATA_KEY)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(SessionTraceState {
        session_id,
        enabled,
    })
}

pub(super) async fn fetch_session_trace_state(
    api: &astra_thin_client::ThinClient,
    bearer_override: Option<&str>,
    session_id: &str,
) -> Result<SessionTraceState, String> {
    let session = api
        .get_session(bearer_override, session_id)
        .await
        .map_err(map_thin_err)?;
    session_trace_state_from_value(&session)
}

pub(super) async fn update_session_trace_state(
    api: &astra_thin_client::ThinClient,
    bearer_override: Option<&str>,
    session_id: &str,
    enabled: bool,
) -> Result<SessionTraceState, String> {
    let session = api
        .get_session(bearer_override, session_id)
        .await
        .map_err(map_thin_err)?;
    let mut metadata = session_metadata_object(&session)?;
    metadata.insert(
        FULL_LLM_CAPTURE_METADATA_KEY.to_string(),
        serde_json::Value::Bool(enabled),
    );
    let updated = api
        .update_session(
            bearer_override,
            session_id,
            &astra_thin_client::SessionUpdateRequest {
                title: None,
                metadata: Some(metadata),
                status: None,
            },
        )
        .await
        .map_err(map_thin_err)?;
    session_trace_state_from_value(&updated)
}

pub(super) fn read_api_error(status: u16, body: &str) -> String {
    // Try to extract user-friendly message from JSON error response
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        // Common API error formats: {"error": "..."} or {"message": "..."} or {"detail": "..."}
        if let Some(msg) = json
            .get("error")
            .and_then(|v| v.as_str())
            .or_else(|| json.get("message").and_then(|v| v.as_str()))
            .or_else(|| json.get("detail").and_then(|v| v.as_str()))
        {
            let base = format!("request failed ({status}): {msg}");
            let mut context_lines = Vec::new();
            if let Some(rid) = json
                .get("request_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                context_lines.push(format!("  request_id: {rid}"));
            }
            if let Some(code) = json
                .get("error_code")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                context_lines.push(format!("  error_code: {code}"));
            }
            if let Some(hint) = status_hint_for(status, msg) {
                context_lines.push(format!("  Hint: {hint}"));
            }
            if context_lines.is_empty() {
                return base;
            }
            return format!("{base}\n{}", context_lines.join("\n"));
        }
    }
    // Fallback: raw body
    format_error_with_context(status, &compact_or_raw(body))
}

/// Get a helpful hint for an HTTP status code.
pub(super) fn status_hint(status: u16) -> Option<&'static str> {
    status_hint_for(status, "")
}

/// Message-aware hint: checks error body for known patterns before falling back to status-only hints.
fn status_hint_for(status: u16, message: &str) -> Option<&'static str> {
    if (status == 500 || status == 503) && message.to_ascii_lowercase().contains("pool timed out") {
        return Some(
            "Database pool timeout — the API could not obtain a free DB connection in time (other requests may be holding connections or the DB is slow). Retry; on the server enable RUST_LOG=astra_services::auth=warn to log pool_size, pool_idle, and the auth operation name.",
        );
    }
    match status {
        400 => Some("Bad request — check your input"),
        401 => Some("Authentication required — try /login"),
        403 => Some("Permission denied — check your access rights"),
        404 => Some("Resource not found"),
        408 | 504 => Some("Request timed out — try again"),
        429 => Some("Rate limited — wait a moment and retry"),
        500 => Some("Server error — this is a bug, please report it"),
        502 | 503 => Some("Service temporarily unavailable — try again shortly"),
        _ => None,
    }
}

/// Format error with helpful context based on status code
fn format_error_with_context(status: u16, message: &str) -> String {
    match status_hint_for(status, message) {
        Some(hint) => format!("request failed ({status}): {message}\n  Hint: {hint}"),
        None => format!("request failed ({status}): {message}"),
    }
}

pub(super) fn map_thin_err(e: astra_thin_client::ThinClientError) -> String {
    match e {
        astra_thin_client::ThinClientError::Api { status, body } => {
            read_api_error(status.as_u16(), &body)
        }
        other => other.to_string(),
    }
}

/// Print an LLM/API call failure message with optional hint
pub(super) fn eprint_api_error(status: u16, context: &str) {
    use crossterm::style::Stylize;
    eprintln!("  {} {} ({})", theme::icon_err(), context, status);
    if let Some(hint) = status_hint(status) {
        eprintln!("      {}", hint.dim());
    }
}

/// Print a transport/request error with helpful hints.
pub(super) fn eprint_request_error<E: std::fmt::Display>(error: &E) {
    use crossterm::style::Stylize;
    let err_str = error.to_string().to_lowercase();

    eprintln!("  {} Request failed: {}", theme::icon_err(), error);

    // Provide hints based on common error patterns
    let hint = if err_str.contains("connection refused") || err_str.contains("connrefused") {
        Some("Server may be down — check if the service is running")
    } else if err_str.contains("timeout") || err_str.contains("timed out") {
        Some("Request timed out — check network or try again")
    } else if err_str.contains("dns") || err_str.contains("resolve") {
        Some("DNS lookup failed — check your network connection")
    } else if err_str.contains("ssl") || err_str.contains("tls") || err_str.contains("certificate")
    {
        Some("TLS/SSL error — check certificates or system time")
    } else if err_str.contains("reset") || err_str.contains("closed") {
        Some("Connection was reset — server may have restarted")
    } else {
        None
    };

    if let Some(h) = hint {
        eprintln!("      {}", h.dim());
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
                } if !filtered.is_empty() && selected > 0 => {
                    selected -= 1;
                }
                KeyEvent {
                    code: KeyCode::Down,
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Tab, ..
                } if !filtered.is_empty() && selected + 1 < filtered.len() => {
                    selected += 1;
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

pub(crate) use astra_text_utils::str_preview::{prefix_chars, truncate_str};

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

/// Read current git HEAD (short SHA) and branch name for journal git snapshots.
///
/// `cwd` should be the session's `git_root` so the snapshot reflects the
/// correct repo even if the process cwd differs. Falls back to process cwd
/// when `None`.
///
/// Returns `(git_head, git_branch)` — either or both may be `None` if not in a
/// git repo or in detached HEAD state (branch will be None).
pub(crate) fn git_snapshot(cwd: Option<&str>) -> (Option<String>, Option<String>) {
    let mut head_cmd = std::process::Command::new("git");
    head_cmd.args(["rev-parse", "--short", "HEAD"]);
    if let Some(dir) = cwd {
        head_cmd.current_dir(dir);
    }
    let head = head_cmd
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    let mut branch_cmd = std::process::Command::new("git");
    branch_cmd.args(["symbolic-ref", "--short", "HEAD"]);
    if let Some(dir) = cwd {
        branch_cmd.current_dir(dir);
    }
    let branch = branch_cmd
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    (head, branch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::{self, JournalDirGuard};
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn isolated_sessions_dir() -> (tempfile::TempDir, JournalDirGuard) {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let guard = JournalDirGuard::new(&sessions);
        (tmp, guard)
    }

    fn write_resumable_session(session_id: &str) {
        let writer = session_journal::JournalWriter::new(session_id).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(session_id),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::interruption_recorded(
                Some(session_id),
                1,
                serde_json::json!({
                    "kind": "rate_limited",
                    "resumable": true,
                    "has_checkpoint": true,
                    "tool_calls_completed": 1,
                    "turns_completed": 1,
                    "remaining_turns": 4,
                }),
            ))
            .unwrap();
    }

    fn write_profile_with_token(session_id: &str) {
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("test-token".into()),
                last_session_id: Some(session_id.to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();
    }

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

    #[test]
    fn read_api_error_pool_timeout_hint_and_request_id() {
        let body = serde_json::json!({
            "detail": "pool timed out while waiting for an open connection",
            "request_id": "req-test-123",
            "error_code": "internal"
        })
        .to_string();
        let err = read_api_error(503, &body);
        assert!(err.contains("pool timed out"), "got: {err}");
        assert!(err.contains("Database pool timeout"), "got: {err}");
        assert!(err.contains("request_id: req-test-123"), "got: {err}");
        assert!(err.contains("error_code: internal"), "got: {err}");
        // Verify ordering: request_id and error_code appear before Hint
        let rid_pos = err.find("request_id:").unwrap();
        let code_pos = err.find("error_code:").unwrap();
        let hint_pos = err.find("Hint:").unwrap();
        assert!(rid_pos < hint_pos, "request_id must appear before Hint");
        assert!(code_pos < hint_pos, "error_code must appear before Hint");
    }

    #[test]
    fn read_api_error_pool_timeout_also_matches_legacy_500() {
        let body = serde_json::json!({
            "detail": "pool timed out while waiting for an open connection"
        })
        .to_string();
        let err = read_api_error(500, &body);
        assert!(err.contains("Database pool timeout"), "got: {err}");
    }

    #[test]
    fn read_api_error_500_without_pool_timeout_gets_generic_hint() {
        let body = serde_json::json!({
            "error": "something else went wrong"
        })
        .to_string();
        let err = read_api_error(500, &body);
        assert!(err.contains("500"), "got: {err}");
        assert!(err.contains("Server error"), "got: {err}");
        assert!(!err.contains("Database pool timeout"), "got: {err}");
    }

    #[test]
    fn read_api_error_json_without_request_id_omits_it() {
        let body = serde_json::json!({
            "error": "bad input"
        })
        .to_string();
        let err = read_api_error(400, &body);
        assert!(err.contains("bad input"), "got: {err}");
        assert!(!err.contains("request_id"), "got: {err}");
    }

    #[test]
    fn status_hint_known_codes() {
        assert!(status_hint(401).unwrap().contains("login"));
        assert!(status_hint(429).unwrap().contains("Rate limited"));
        assert!(status_hint(500).unwrap().contains("Server error"));
        assert!(status_hint(200).is_none());
    }

    #[test]
    fn status_hint_for_pool_timeout_overrides_generic_500() {
        let hint = super::status_hint_for(500, "pool timed out while waiting");
        assert!(hint.unwrap().contains("Database pool timeout"));
        // Also works with 503
        let hint = super::status_hint_for(503, "pool timed out while waiting");
        assert!(hint.unwrap().contains("Database pool timeout"));
    }

    #[test]
    fn status_hint_for_normal_500_gives_generic() {
        let hint = super::status_hint_for(500, "unexpected error");
        assert!(hint.unwrap().contains("Server error"));
    }

    #[test]
    fn format_error_with_context_includes_hint() {
        let out = super::format_error_with_context(401, "unauthorized");
        assert!(out.contains("401"));
        assert!(out.contains("unauthorized"));
        assert!(out.contains("Hint:"));
    }

    #[test]
    fn format_error_with_context_no_hint_for_unknown_status() {
        let out = super::format_error_with_context(418, "I'm a teapot");
        assert!(out.contains("418"));
        assert!(!out.contains("Hint:"));
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
        let (_tmp, _guard) = isolated_sessions_dir();
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
        let (_tmp, _guard) = isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();

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
    }

    #[tokio::test]
    async fn validated_resumable_last_session_id_keeps_live_session() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("live-session-{}", uuid::Uuid::new_v4());
        write_resumable_session(&session_id);
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "status": "active"
            })))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let resolved = validated_resumable_last_session_id(&api, None).await;
        assert_eq!(resolved.as_deref(), Some(session_id.as_str()));
        assert_eq!(
            load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            Some(session_id.as_str())
        );
    }

    #[tokio::test]
    async fn validated_resumable_last_session_id_drops_stale_404_session() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("stale-session-{}", uuid::Uuid::new_v4());
        write_resumable_session(&session_id);
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "detail": "Session not found"
            })))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let resolved = validated_resumable_last_session_id(&api, None).await;
        assert_eq!(resolved, None);
        assert_eq!(
            load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            None
        );
    }

    #[tokio::test]
    async fn validated_resumable_last_session_id_keeps_session_on_transient_server_error() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("transient-session-{}", uuid::Uuid::new_v4());
        write_resumable_session(&session_id);
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "detail": "Service temporarily unavailable"
            })))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let resolved = validated_resumable_last_session_id(&api, None).await;
        assert_eq!(resolved.as_deref(), Some(session_id.as_str()));
        assert_eq!(
            load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            Some(session_id.as_str())
        );
    }

    #[tokio::test]
    async fn fetch_session_trace_state_defaults_to_disabled() {
        let session_id = format!("trace-status-{}", uuid::Uuid::new_v4());
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "metadata": {
                    "owner": "alice"
                }
            })))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let state = fetch_session_trace_state(&api, Some("test-token"), &session_id)
            .await
            .unwrap();
        assert_eq!(
            state,
            SessionTraceState {
                session_id,
                enabled: false,
            }
        );
    }

    #[tokio::test]
    async fn update_session_trace_state_preserves_existing_metadata() {
        let session_id = format!("trace-update-{}", uuid::Uuid::new_v4());
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "metadata": {
                    "owner": "alice",
                    "priority": 7
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "metadata": {
                    "owner": "alice",
                    "priority": 7,
                    "full_llm_capture": true
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "metadata": {
                    "owner": "alice",
                    "priority": 7,
                    "full_llm_capture": true
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let state = update_session_trace_state(&api, Some("test-token"), &session_id, true)
            .await
            .unwrap();
        assert_eq!(
            state,
            SessionTraceState {
                session_id,
                enabled: true,
            }
        );
    }

    #[test]
    fn test_profile_debug_masks_secrets() {
        let profile = Profile {
            username: Some("alice".into()),
            access_token: Some("sk-secret-token-12345".into()),
            refresh_token: Some("rt-refresh-abcdef".into()),
            last_session_id: Some("sess-001".into()),
            memoria_api_key: Some("mem-key-xyz".into()),
        };
        let dbg = format!("{:?}", profile);
        assert!(dbg.contains("alice"), "username should be visible");
        assert!(dbg.contains("sess-001"), "session_id should be visible");
        assert!(!dbg.contains("sk-secret"), "access_token must be masked");
        assert!(!dbg.contains("rt-refresh"), "refresh_token must be masked");
        assert!(!dbg.contains("mem-key"), "memoria_api_key must be masked");
        assert!(dbg.contains("***"), "masked fields should show ***");
    }

    #[test]
    fn test_credentials_file_permissions() {
        let _creds_guard = crate::tests::isolate_credentials();
        let creds = CredentialsFile {
            current_profile: Some("default".into()),
            profiles: HashMap::new(),
        };
        save_credentials(&creds).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = credentials_path();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "credentials.json must be 0600, got {mode:o}");
        }
    }

    #[test]
    fn git_snapshot_returns_head_and_branch_in_git_repo() {
        // This test runs inside the astra git repo, so both should be Some.
        let (head, _branch) = super::git_snapshot(None);
        assert!(
            head.is_some(),
            "git_snapshot must return Some(head) inside a git repo"
        );
        let h = head.unwrap();
        assert!(
            h.len() >= 7 && h.len() <= 40,
            "git HEAD should be 7-40 chars, got {}: '{h}'",
            h.len()
        );
        // branch may be None in CI detached HEAD, so we only check head is valid hex
        assert!(
            h.chars().all(|c| c.is_ascii_hexdigit()),
            "git HEAD must be hex, got '{h}'"
        );
    }

    #[test]
    fn git_snapshot_with_explicit_cwd_matches_none() {
        // Passing the current dir explicitly should give the same result as None.
        let cwd = std::env::current_dir().unwrap();
        let (head_none, branch_none) = super::git_snapshot(None);
        let (head_cwd, branch_cwd) = super::git_snapshot(Some(cwd.to_str().unwrap()));
        assert_eq!(head_none, head_cwd, "explicit cwd must match implicit cwd");
        assert_eq!(
            branch_none, branch_cwd,
            "explicit cwd must match implicit cwd"
        );
    }

    #[test]
    fn git_snapshot_with_non_git_dir_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, branch) = super::git_snapshot(Some(tmp.path().to_str().unwrap()));
        assert!(
            head.is_none(),
            "non-git dir must return None for head, got {head:?}"
        );
        assert!(
            branch.is_none(),
            "non-git dir must return None for branch, got {branch:?}"
        );
    }
}
