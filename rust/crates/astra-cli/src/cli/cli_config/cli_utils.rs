use super::*;
use astra_services::session_journal;

pub(crate) use astra_credentials::{CredentialStore, CredentialsFile, Profile};
use crossterm::event::{Event, KeyCode, KeyModifiers};

pub(crate) fn credential_store() -> CredentialStore {
    CredentialStore::new()
}

pub(crate) fn credentials_path() -> PathBuf {
    credential_store().path().clone()
}

/// Load credentials from disk, falling back to defaults on error.
///
/// A non-default load failure (e.g. fd exhaustion, permission denied, JSON
/// corruption) used to be silently swallowed by `unwrap_or_default()`, which
/// would then surface upstream as a misleading "Not logged in" prompt. We
/// now log the underlying error so the user sees the real cause; the
/// fallback to default is preserved so callers (notably `current_access_token`
/// and `try_silent_auth`) keep their current contracts.
///
/// Repeated failures within a single process are deduplicated (we only print
/// a warning when the error string changes) to avoid flooding stderr when
/// the underlying condition persists across many calls.
pub(crate) fn load_credentials() -> CredentialsFile {
    use std::sync::Mutex;
    use std::sync::OnceLock;

    static LAST_ERR: OnceLock<Mutex<Option<String>>> = OnceLock::new();

    match credential_store().load() {
        Ok(creds) => creds,
        Err(err) => {
            let msg = err.to_string();
            let last = LAST_ERR.get_or_init(|| Mutex::new(None));
            let mut guard = astra_core::sync_poison::recover_mutex_lock(&last);
            if guard.as_deref() != Some(msg.as_str()) {
                eprintln!("  ⚠ failed to read credentials: {msg}");
                *guard = Some(msg);
            }
            CredentialsFile::default()
        }
    }
}

#[cfg(test)]
pub(crate) fn save_credentials(data: &CredentialsFile) -> Result<(), String> {
    let store = credential_store();
    store
        .mutate(|d| {
            *d = data.clone();
        })
        .map_err(|e| e.to_string())
}

pub(crate) fn mutate_credentials<F, R>(f: F) -> Result<R, String>
where
    F: FnOnce(&mut CredentialsFile) -> R,
{
    credential_store().mutate(f).map_err(|e| e.to_string())
}

pub(crate) fn profile_name(cli_profile: Option<&str>, data: &CredentialsFile) -> String {
    CredentialStore::resolve_profile_name(cli_profile, data.current_profile.as_deref())
}

pub(crate) fn normalize_model_override(model: Option<&str>) -> Option<&str> {
    astra_core::model_override::normalize_model_override(model)
}

pub(crate) fn normalize_model_override_owned(model: Option<String>) -> Option<String> {
    astra_core::model_override::normalize_model_override_owned(model)
}

pub(crate) fn cli_user_id() -> String {
    std::env::var("ASTRA_CLI_USER_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "local".to_string())
}

pub(crate) fn get_profile_and_token(
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

pub(crate) fn session_is_resumable(session_id: &str) -> bool {
    match session_journal::classify_session_end_state(session_id) {
        Ok(session_journal::SessionEndState::Completed) => false,
        Ok(session_journal::SessionEndState::Interrupted { resumable, .. }) => resumable,
        Ok(session_journal::SessionEndState::Zombie) => true,
        Err(_) => true,
    }
}

fn latest_session_segment_has_explicit_end(session_id: &str) -> bool {
    let Ok(events) = session_journal::read_journal(session_id) else {
        return false;
    };

    for event in events.iter().rev() {
        match event.event_type {
            session_journal::JournalEventType::SessionEnd => return true,
            session_journal::JournalEventType::SessionStart => return false,
            _ => {}
        }
    }

    false
}

pub(crate) fn local_session_is_resumable(session_id: &str) -> bool {
    if session_journal::validate_session_id(session_id).is_err() {
        return false;
    }
    let journal_exists = session_journal::journal_file_path(session_id).exists();
    let has_heavy_checkpoint =
        astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(session_id)
            .map(|checkpoint| checkpoint.is_some())
            .unwrap_or(false);
    let workspace = match astra_services::session_workspace::read_workspace_optional(session_id) {
        Ok(workspace) => workspace,
        Err(error) => {
            tracing::warn!(
                %session_id,
                %error,
                "failed to read workspace metadata while checking local resumability"
            );
            None
        }
    };

    if !journal_exists {
        if has_heavy_checkpoint {
            return true;
        }
        return workspace
            .as_ref()
            .is_some_and(|ws| !ws.status.eq_ignore_ascii_case("completed"));
    }

    match session_journal::classify_session_end_state(session_id) {
        Ok(session_journal::SessionEndState::Completed) => {
            has_heavy_checkpoint && !latest_session_segment_has_explicit_end(session_id)
        }
        Ok(session_journal::SessionEndState::Interrupted { resumable, .. }) => resumable,
        Ok(session_journal::SessionEndState::Zombie) => true,
        Err(_) => has_heavy_checkpoint,
    }
}

pub(crate) fn local_resumable_last_session_id(cli_profile: Option<&str>) -> Option<String> {
    stored_last_session_id(cli_profile).filter(|session_id| local_session_is_resumable(session_id))
}

pub(crate) fn stored_last_session_id(cli_profile: Option<&str>) -> Option<String> {
    let creds = load_credentials();
    let name = profile_name(cli_profile, &creds);
    let session_id = creds
        .profiles
        .get(&name)
        .and_then(|profile| profile.last_session_id.clone())?;
    if session_journal::validate_session_id(&session_id).is_ok() {
        Some(session_id)
    } else {
        clear_profile_last_session_if_matches_or_warn(
            cli_profile,
            &session_id,
            "cli_utils:stored_last_session_id",
        );
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionResumePreflight {
    Valid,
    Missing,
    NoAuth,
    Unknown,
}

pub(crate) fn clear_profile_last_session_if_matches(
    cli_profile: Option<&str>,
    session_id: &str,
) -> Result<bool, String> {
    mutate_credentials(|creds| {
        let resolved_name = profile_name(cli_profile, creds);
        if let Some(entry) = creds.profiles.get_mut(&resolved_name)
            && entry.last_session_id.as_deref() == Some(session_id)
        {
            entry.last_session_id = None;
            return true;
        }

        if cli_profile.is_some() {
            return false;
        }

        creds.profiles.iter_mut().any(|(name, entry)| {
            if name == &resolved_name || entry.last_session_id.as_deref() != Some(session_id) {
                return false;
            }
            entry.last_session_id = None;
            true
        })
    })
}

pub(crate) fn clear_profile_last_session_if_matches_or_warn(
    cli_profile: Option<&str>,
    session_id: &str,
    context: &'static str,
) {
    if let Err(error) = clear_profile_last_session_if_matches(cli_profile, session_id) {
        tracing::warn!(
            %error,
            %session_id,
            context,
            "failed to clear matching profile last_session_id"
        );
    }
}

pub(crate) fn persist_profile_last_session(
    cli_profile: Option<&str>,
    session_id: &str,
) -> Result<(), String> {
    validate_cli_session_id(session_id)?;
    mutate_credentials(|creds| {
        let name = profile_name(cli_profile, creds);
        let entry = creds.profiles.entry(name).or_default();
        entry.last_session_id = Some(session_id.to_string());
    })
}

pub(crate) fn persist_profile_last_session_or_warn(
    cli_profile: Option<&str>,
    session_id: &str,
    context: &'static str,
) {
    if let Err(error) = persist_profile_last_session(cli_profile, session_id) {
        tracing::warn!(
            %error,
            %session_id,
            context,
            "failed to persist profile last_session_id"
        );
    }
}

pub(crate) fn append_journal_event_or_warn(
    journal: &session_journal::JournalWriter,
    session_id: Option<&str>,
    event: &session_journal::JournalEvent,
    context: &'static str,
) {
    if let Err(error) = journal.append(event) {
        tracing::warn!(
            %error,
            session_id,
            context,
            "failed to append journal event"
        );
    }
}

pub(crate) fn append_session_journal_event_or_warn(
    session_id: &str,
    event: &session_journal::JournalEvent,
    context: &'static str,
) {
    match session_journal::JournalWriter::new(session_id) {
        Ok(journal) => append_journal_event_or_warn(&journal, Some(session_id), event, context),
        Err(error) => tracing::warn!(
            %error,
            %session_id,
            context,
            "failed to open journal for append"
        ),
    }
}

pub(crate) fn append_bulk_journal_events_no_sync_or_warn(
    journal: &session_journal::JournalWriter,
    session_id: Option<&str>,
    events: &[session_journal::JournalEvent],
    context: &'static str,
) {
    if let Err(error) = journal.append_bulk_no_sync(events) {
        tracing::warn!(
            %error,
            session_id,
            context,
            count = events.len(),
            "failed to append journal events"
        );
    }
}

pub(crate) fn persist_profile_memoria_api_key(
    cli_profile: Option<&str>,
    api_key: &str,
) -> Result<(), String> {
    mutate_credentials(|creds| {
        let name = profile_name(cli_profile, creds);
        let entry = creds.profiles.entry(name).or_default();
        entry.memoria_api_key = Some(api_key.to_string());
    })
}

pub(crate) fn validate_cli_session_id(session_id: &str) -> Result<(), String> {
    session_journal::validate_session_id(session_id).map_err(|e| format!("invalid session_id: {e}"))
}

pub(crate) async fn preflight_remote_resume_session(
    api: &astra_thin_client::ThinClient,
    cli_profile: Option<&str>,
    session_id: &str,
) -> SessionResumePreflight {
    let Some(token) = crate::cli::session_runtime::current_access_token(cli_profile) else {
        return SessionResumePreflight::NoAuth;
    };

    match api.get_session(Some(&token), session_id).await {
        Ok(_) => SessionResumePreflight::Valid,
        Err(astra_thin_client::ThinClientError::Api { status, .. }) if status.as_u16() == 404 => {
            SessionResumePreflight::Missing
        }
        Err(_) => SessionResumePreflight::Unknown,
    }
}

pub(crate) async fn validated_resumable_last_session_id(
    api: &astra_thin_client::ThinClient,
    cli_profile: Option<&str>,
) -> Option<String> {
    let session_id = stored_last_session_id(cli_profile)?;
    match preflight_remote_resume_session(api, cli_profile, &session_id).await {
        SessionResumePreflight::Valid | SessionResumePreflight::Unknown => Some(session_id),
        SessionResumePreflight::NoAuth => {
            local_resumable_last_session_id(cli_profile).filter(|local| local == &session_id)
        }
        SessionResumePreflight::Missing if local_session_is_resumable(&session_id) => {
            Some(session_id)
        }
        SessionResumePreflight::Missing => {
            clear_profile_last_session_if_matches_or_warn(
                cli_profile,
                &session_id,
                "cli_utils:validated_resumable_last_session_id",
            );
            None
        }
    }
}

pub(crate) fn read_api_error(status: u16, body: &str) -> String {
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
pub(crate) fn status_hint(status: u16) -> Option<&'static str> {
    status_hint_for(status, "")
}

/// Message-aware hint: checks error body for known patterns before falling back to status-only hints.
pub(crate) fn status_hint_for(status: u16, message: &str) -> Option<&'static str> {
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
pub(crate) fn format_error_with_context(status: u16, message: &str) -> String {
    match status_hint_for(status, message) {
        Some(hint) => format!("request failed ({status}): {message}\n  Hint: {hint}"),
        None => format!("request failed ({status}): {message}"),
    }
}

pub(crate) fn map_thin_err(e: astra_thin_client::ThinClientError) -> String {
    match e {
        astra_thin_client::ThinClientError::Api { status, body } => {
            format_error_with_context(status.as_u16(), &body)
        }
        astra_thin_client::ThinClientError::Http(error) => {
            if error.is_timeout() {
                "Request timed out".to_string()
            } else {
                format!("Network error: {error}")
            }
        }
        astra_thin_client::ThinClientError::Json(error) => {
            format!("API response parse error: {error}")
        }
        astra_thin_client::ThinClientError::SseParse(error) => {
            format!("SSE parse error: {error}")
        }
        astra_thin_client::ThinClientError::InvalidSseJson(value) => {
            format!("Invalid SSE JSON payload: {value}")
        }
        astra_thin_client::ThinClientError::InvalidBaseUrl(value) => {
            format!("Invalid API URL: {value}")
        }
        astra_thin_client::ThinClientError::InvalidAuthHeader => {
            "Invalid authorization header".to_string()
        }
        astra_thin_client::ThinClientError::InvalidInput(value) => {
            format!("Invalid request: {value}")
        }
    }
}

/// Session-auth shaped errors that should trigger Astra credential recovery.
///
/// Intentionally excludes generic upstream `401 Unauthorized` text: external
/// services and tools can emit that even when the Astra session is healthy.
pub(crate) fn is_astra_session_auth_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("could not validate credentials")
        || lower.contains("session expired")
        || lower.contains("token expired")
        || lower.contains("invalid token")
        || lower.contains("authentication failed")
        || lower.contains("authentication required — try /login")
        || lower.contains("hint: session expired — try /login")
        || lower.contains("hint: authentication required — try /login")
}

/// Print an LLM/API call failure message with optional hint
pub(crate) fn eprint_api_error(status: u16, context: &str) {
    use crossterm::style::Stylize;
    eprintln!("  {} {} ({})", theme::icon_err(), context, status);
    if let Some(hint) = status_hint(status) {
        eprintln!("      {}", hint.dim());
    }
}

/// Print a transport/request error with helpful hints.
pub(crate) fn eprint_request_error<E: std::fmt::Display>(error: &E) {
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

pub(crate) fn compact_or_raw(body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => value.to_string(),
        Err(_) => body.to_string(),
    }
}

pub(crate) fn print_json_or_raw(body: &str) {
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
pub(crate) fn prompt_or(label: &str, existing: Option<String>) -> Result<String, String> {
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
pub(crate) fn prompt_password_masked(
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
pub(crate) fn interactive_select(
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
                        marker.cyan().bold(),
                        label.as_str().cyan().bold(),
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

pub(crate) fn urlencoding(s: &str) -> String {
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
    use std::sync::{Mutex, OnceLock};
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    fn runtime_config_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock poisoned")
    }

    struct CliOverlayGuard;

    impl CliOverlayGuard {
        fn install(overlay: astra_config::runtime_config::RuntimeConfig) -> Self {
            astra_config::runtime_config::set_cli_overlay(Some(overlay));
            Self
        }
    }

    impl Drop for CliOverlayGuard {
        fn drop(&mut self) {
            astra_config::runtime_config::set_cli_overlay(None);
        }
    }

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
        let hint = status_hint_for(500, "pool timed out while waiting");
        assert!(hint.unwrap().contains("Database pool timeout"));
        // Also works with 503
        let hint = status_hint_for(503, "pool timed out while waiting");
        assert!(hint.unwrap().contains("Database pool timeout"));
    }

    #[test]
    fn status_hint_for_normal_500_gives_generic() {
        let hint = status_hint_for(500, "unexpected error");
        assert!(hint.unwrap().contains("Server error"));
    }

    #[test]
    fn format_error_with_context_includes_hint() {
        let out = format_error_with_context(401, "unauthorized");
        assert!(out.contains("401"));
        assert!(out.contains("unauthorized"));
        assert!(out.contains("Hint:"));
    }

    #[test]
    fn format_error_with_context_no_hint_for_unknown_status() {
        let out = format_error_with_context(418, "I'm a teapot");
        assert!(out.contains("418"));
        assert!(!out.contains("Hint:"));
    }

    #[test]
    fn astra_session_auth_error_matches_session_specific_failures() {
        let msg =
            "request failed (401): invalid token\n  Hint: Authentication required — try /login";
        assert!(is_astra_session_auth_error(msg));
    }

    #[test]
    fn astra_session_auth_error_ignores_generic_upstream_401s() {
        assert!(!is_astra_session_auth_error(
            "GitHub API Error: 401 Unauthorized"
        ));
    }

    // ── profile_name ──────────────────────────────────────────────────────────

    #[serial_test::serial]
    #[test]
    fn profile_name_uses_cli_override() {
        temp_env::with_var("ASTRA_PROFILE", None::<&str>, || {
            let creds = CredentialsFile::default();
            assert_eq!(profile_name(Some("staging"), &creds), "staging");
        });
    }

    #[serial_test::serial]
    #[test]
    fn profile_name_uses_default_from_creds() {
        temp_env::with_var("ASTRA_PROFILE", None::<&str>, || {
            let creds = CredentialsFile {
                current_profile: Some("prod".to_string()),
                ..Default::default()
            };
            assert_eq!(profile_name(None, &creds), "prod");
        });
    }

    #[serial_test::serial]
    #[test]
    fn profile_name_falls_back_to_default() {
        temp_env::with_var("ASTRA_PROFILE", None::<&str>, || {
            let creds = CredentialsFile::default();
            assert_eq!(profile_name(None, &creds), "default");
        });
    }

    #[test]
    fn normalize_model_override_treats_default_as_api_default() {
        assert_eq!(normalize_model_override(None), None);
        assert_eq!(normalize_model_override(Some("")), None);
        assert_eq!(normalize_model_override(Some(" default ")), None);
        assert_eq!(normalize_model_override(Some("DEFAULT")), None);
        assert_eq!(
            normalize_model_override(Some("MiniMax-M2.7")),
            Some("MiniMax-M2.7")
        );
    }

    #[serial_test::serial]
    #[test]
    fn persist_profile_last_session_updates_only_target_field() {
        let _creds_guard = crate::tests::isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                username: Some("user".to_string()),
                access_token: Some("tok".to_string()),
                refresh_token: Some("ref".to_string()),
                memoria_api_key: Some("mem-key".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        persist_profile_last_session(None, "sess-new").unwrap();

        let creds = load_credentials();
        let profile = &creds.profiles["default"];
        assert_eq!(profile.last_session_id.as_deref(), Some("sess-new"));
        assert_eq!(profile.memoria_api_key.as_deref(), Some("mem-key"));
        assert_eq!(profile.access_token.as_deref(), Some("tok"));
        assert_eq!(profile.refresh_token.as_deref(), Some("ref"));
    }

    #[serial_test::serial]
    #[test]
    fn persist_profile_last_session_rejects_invalid_session_id_without_mutation() {
        let _creds_guard = crate::tests::isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some("sess-old".to_string()),
                access_token: Some("tok".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let err = persist_profile_last_session(None, "../escape").unwrap_err();

        assert!(err.contains("invalid session_id"), "got: {err}");
        let creds = load_credentials();
        let profile = &creds.profiles["default"];
        assert_eq!(profile.last_session_id.as_deref(), Some("sess-old"));
        assert_eq!(profile.access_token.as_deref(), Some("tok"));
    }

    #[serial_test::serial]
    #[test]
    fn persist_profile_memoria_api_key_updates_only_target_field() {
        let _creds_guard = crate::tests::isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                username: Some("user".to_string()),
                last_session_id: Some("sess-old".to_string()),
                access_token: Some("tok".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        persist_profile_memoria_api_key(None, "mem-new").unwrap();

        let creds = load_credentials();
        let profile = &creds.profiles["default"];
        assert_eq!(profile.memoria_api_key.as_deref(), Some("mem-new"));
        assert_eq!(profile.last_session_id.as_deref(), Some("sess-old"));
        assert_eq!(profile.access_token.as_deref(), Some("tok"));
    }

    #[test]
    #[serial_test::serial]
    fn clear_profile_last_session_if_matches_falls_back_to_exact_session_match() {
        let _creds_guard = crate::tests::isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some("sess-stale".to_string()),
                ..Default::default()
            },
        );
        creds.profiles.insert(
            "other".to_string(),
            Profile {
                last_session_id: Some("sess-live".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        temp_env::with_var("ASTRA_PROFILE", Some("other"), || {
            assert!(clear_profile_last_session_if_matches(None, "sess-stale").unwrap());
        });

        let creds = load_credentials();
        assert_eq!(
            creds.profiles["default"].last_session_id.as_deref(),
            None,
            "stale session pointer should be cleared even if ASTRA_PROFILE points elsewhere"
        );
        assert_eq!(
            creds.profiles["other"].last_session_id.as_deref(),
            Some("sess-live")
        );
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

    #[serial_test::serial]
    #[test]
    fn local_resumable_last_session_id_ignores_stale_pointer_without_local_state() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let _home_guard = crate::tests::HomeGuard::temp();

        let sid = format!("test-stale-local-{}", uuid::Uuid::new_v4());
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(sid),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        assert_eq!(local_resumable_last_session_id(None), None);
    }

    #[serial_test::serial]
    #[test]
    fn local_resumable_last_session_id_keeps_workspace_only_active_session() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let _home_guard = crate::tests::HomeGuard::temp();

        let sid = format!("test-workspace-only-{}", uuid::Uuid::new_v4());
        let ws = astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-5");
        astra_services::session_workspace::write_workspace(&ws).unwrap();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(sid.clone()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        assert_eq!(
            local_resumable_last_session_id(None).as_deref(),
            Some(sid.as_str())
        );
    }

    #[serial_test::serial]
    #[test]
    fn local_resumable_last_session_id_ignores_workspace_only_completed_session() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let _home_guard = crate::tests::HomeGuard::temp();

        let sid = format!("test-workspace-completed-{}", uuid::Uuid::new_v4());
        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-5");
        ws.status = "completed".to_string();
        astra_services::session_workspace::write_workspace(&ws).unwrap();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(sid),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        assert_eq!(local_resumable_last_session_id(None), None);
    }

    #[serial_test::serial]
    #[test]
    fn local_resumable_last_session_id_ignores_unreadable_workspace_without_replay_state() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let _home_guard = crate::tests::HomeGuard::temp();

        let sid = format!("test-workspace-corrupt-{}", uuid::Uuid::new_v4());
        let path = astra_services::session_workspace::workspace_file_path(&sid).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, ":\nnot-valid-yaml").unwrap();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(sid.clone()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        assert_eq!(local_resumable_last_session_id(None), None);
        assert!(
            !local_session_is_resumable(&sid),
            "corrupt workspace without journal/checkpoint must not create a fake resumable session"
        );
    }

    #[serial_test::serial]
    #[test]
    fn local_resumable_last_session_id_keeps_checkpoint_backed_session_without_terminal_journal() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let _home_guard = crate::tests::HomeGuard::temp();

        let sid = uuid::Uuid::new_v4().to_string();
        let writer = astra_services::session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(
                &astra_services::session_journal::JournalEvent::session_start(
                    Some(&sid),
                    Some("gpt-5"),
                ),
            )
            .unwrap();
        drop(writer);

        let heavy = astra_pipeline::step_protocol::HeavyCheckpoint {
            light: astra_pipeline::step_protocol::LightCheckpoint {
                protocol_version: astra_pipeline::step_protocol::PROTOCOL_VERSION,
                cursor: Default::default(),
                step_id: "step-1".to_string(),
                task_id: "task-1".to_string(),
                agent_id: sid.clone(),
                progress: 1.0,
                total_tokens: 42,
                created_at: astra_pipeline::step_protocol::epoch_ms(),
            },
            messages: vec![
                serde_json::json!({"role": "user", "content": "previous question"}),
                serde_json::json!({"role": "assistant", "content": "previous answer"}),
            ],
            budget_remaining_tokens: 0,
            budget_remaining_rounds: 0,
            blocked_tools: Vec::new(),
            recent_tools: Vec::new(),
            memory_context: None,
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 0,
            pipeline_state: None,
            compaction_state: None,
            config_version_id: None,
        };
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &sid,
            1,
            &astra_pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy)),
        )
        .unwrap();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(sid.clone()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        assert_eq!(
            local_resumable_last_session_id(None).as_deref(),
            Some(sid.as_str())
        );
    }

    #[serial_test::serial]
    #[test]
    fn local_resumable_last_session_id_clears_invalid_pointer_without_panicking() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let _home_guard = crate::tests::HomeGuard::temp();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some("../escape".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        assert_eq!(local_resumable_last_session_id(None), None);
        assert_eq!(
            load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            None
        );
    }

    #[serial_test::serial]
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

    #[serial_test::serial]
    #[tokio::test]
    async fn validated_resumable_last_session_id_keeps_local_state_when_remote_404s() {
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
        assert_eq!(resolved.as_deref(), Some(session_id.as_str()));
        assert_eq!(
            load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            Some(session_id.as_str())
        );
    }

    #[serial_test::serial]
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

    #[serial_test::serial]
    #[tokio::test]
    async fn validated_resumable_last_session_id_uses_env_token_when_credentials_token_missing() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("env-token-session-{}", uuid::Uuid::new_v4());
        write_resumable_session(&session_id);
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(session_id.clone()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

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

        let _token = EnvGuard::set("ASTRA_ACCESS_TOKEN", "env-token-xyz");
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let resolved = validated_resumable_last_session_id(&api, None).await;
        assert_eq!(resolved.as_deref(), Some(session_id.as_str()));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn validated_resumable_last_session_id_keeps_live_remote_session_without_local_journal() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("remote-only-session-{}", uuid::Uuid::new_v4());
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

    #[serial_test::serial]
    #[tokio::test]
    async fn validated_resumable_last_session_id_ignores_remote_pointer_without_auth_or_local_state()
     {
        let (_tmp, _guard) = isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let _token = EnvGuard::set("ASTRA_ACCESS_TOKEN", "");
        let session_id = format!("unauthed-remote-only-{}", uuid::Uuid::new_v4());
        write_profile_with_token(&session_id);
        mutate_credentials(|creds| {
            if let Some(entry) = creds.profiles.get_mut("default") {
                entry.access_token = None;
            }
        })
        .unwrap();

        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let resolved = validated_resumable_last_session_id(&api, None).await;

        assert_eq!(resolved, None);
        assert_eq!(
            load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            Some(session_id.as_str()),
            "missing auth must not clear the stored pointer; it is only unusable in this process"
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

    #[serial_test::serial]
    #[test]
    fn test_credentials_file_permissions() {
        let _creds_guard = crate::tests::isolate_credentials();
        let creds = CredentialsFile {
            current_profile: Some("default".to_string()),
            ..Default::default()
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
        let (head, _branch) = git_snapshot(None);
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
        let (head_none, branch_none) = git_snapshot(None);
        let (head_cwd, branch_cwd) = git_snapshot(Some(cwd.to_str().unwrap()));
        assert_eq!(head_none, head_cwd, "explicit cwd must match implicit cwd");
        assert_eq!(
            branch_none, branch_cwd,
            "explicit cwd must match implicit cwd"
        );
    }

    #[test]
    fn git_snapshot_with_non_git_dir_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, branch) = git_snapshot(Some(tmp.path().to_str().unwrap()));
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
