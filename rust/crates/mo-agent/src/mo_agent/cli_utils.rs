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
    if let Ok(dir) = std::env::var("MO_AGENT_CREDENTIALS_DIR") {
        return PathBuf::from(dir).join("credentials.json");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mo-agent")
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

pub(super) fn auth_headers(token: &str) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).map_err(|e| e.to_string())?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

/// Capitalize the first letter of a string.
pub(super) fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
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

pub(super) fn read_api_error(status: reqwest::StatusCode, body: &str) -> String {
    format!("request failed ({}): {}", status, compact_or_raw(body))
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
                eprint!("\x1b[A\x1b[2K");
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
        eprint!("\x1b[A\x1b[2K");
    }
    let _ = io::stderr().flush();
    terminal::disable_raw_mode().ok();
    result
}
pub(super) fn print_markdown(text: &str) {
    let mut skin = termimad::MadSkin::default();
    // Use crossterm colors so they match our existing palette
    use termimad::crossterm::style::Color;
    skin.bold.set_fg(Color::Cyan);
    skin.italic.set_fg(Color::Yellow);
    skin.inline_code.set_fg(Color::Green);
    skin.print_text(text);
}

/// Extract a brief detail string from tool call arguments for the └ line.
/// Tool categories for organized formatting
#[derive(Debug, Clone, Copy)]
enum ToolCat {
    Github,
    File,
    Shell,
    Search,
    Git,
    Mo,
    Memory,
    Other,
}

fn categorize(name: &str) -> ToolCat {
    match name {
        n if n.starts_with("github_") => ToolCat::Github,
        "read_file" | "view_file" | "write_file" | "edit_file" | "str_replace" => ToolCat::File,
        "run_command" | "shell" | "exec" | "bash" => ToolCat::Shell,
        "search" | "grep" | "find" | "glob" | "list_dir" => ToolCat::Search,
        "git_diff" | "git_log" | "git_show" | "git_blame" | "git_log_search" | "git_status" => {
            ToolCat::Git
        }
        "mo_query" => ToolCat::Mo,
        n if n.starts_with("memoria_") || n.starts_with("memory_") => ToolCat::Memory,
        _ => ToolCat::Other,
    }
}

fn fmt_github_tool(
    _name: &str,
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let owner = obj.get("owner").and_then(|v| v.as_str());
    let repo = obj.get("repo").and_then(|v| v.as_str());
    match (owner, repo) {
        (Some(o), Some(r)) => Some(format!("{o}/{r}")),
        _ => obj
            .get("query")
            .and_then(|v| v.as_str())
            .map(|q| truncate_str(q, 60)),
    }
}

fn fmt_file_tool(name: &str, obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    match name {
        "read_file" | "view_file" => {
            let path = obj.get("path").and_then(|v| v.as_str())?;
            let start = obj.get("start_line").and_then(|v| v.as_u64());
            let end = obj.get("end_line").and_then(|v| v.as_u64());
            let outline = obj
                .get("outline")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if outline {
                Some(format!("{path} (outline)"))
            } else {
                match (start, end) {
                    (Some(s), Some(e)) => Some(format!("{path}:{s}-{e}")),
                    (Some(s), None) => Some(format!("{path}:{s}-")),
                    _ => Some(path.to_string()),
                }
            }
        }
        "write_file" | "edit_file" => obj
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| p.to_string()),
        "str_replace" => {
            let path = obj.get("path").and_then(|v| v.as_str())?;
            let old = obj.get("old_str").and_then(|v| v.as_str());
            match old {
                Some(s) => {
                    let first_line = s.lines().next().unwrap_or("");
                    let preview = truncate_str(first_line, 40);
                    let line_count = s.lines().count();
                    if line_count > 1 {
                        Some(format!("{path} ({line_count} lines)"))
                    } else {
                        Some(format!("{path}: \"{preview}\""))
                    }
                }
                None => Some(path.to_string()),
            }
        }
        _ => None,
    }
}

fn fmt_shell_tool(_name: &str, obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    obj.get("command")
        .and_then(|v| v.as_str())
        .map(|c| truncate_str(c, 60))
}

fn fmt_search_tool(name: &str, obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    match name {
        "search" | "grep" | "find" => {
            let pattern = obj
                .get("query")
                .or_else(|| obj.get("pattern"))
                .and_then(|v| v.as_str());
            let path = obj.get("path").and_then(|v| v.as_str());
            match (pattern, path) {
                (Some(p), Some(dir)) => Some(format!("\"{}\" in {dir}", truncate_str(p, 40))),
                (Some(p), None) => Some(format!("\"{}\"", truncate_str(p, 50))),
                _ => None,
            }
        }
        "glob" => obj
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|p| truncate_str(p, 60)),
        "list_dir" => obj
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| p.to_string()),
        _ => None,
    }
}

fn fmt_git_tool(name: &str, obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    match name {
        "git_diff" => {
            let path = obj.get("path").and_then(|v| v.as_str());
            let staged = obj.get("staged").and_then(|v| v.as_bool()).unwrap_or(false);
            let suffix = if staged { " (staged)" } else { "" };
            match path {
                Some(p) => Some(format!("{p}{suffix}")),
                None => Some(format!("working tree{suffix}")),
            }
        }
        "git_log" => {
            let n = obj.get("max_count").and_then(|v| v.as_u64());
            let path = obj.get("path").and_then(|v| v.as_str());
            match (path, n) {
                (Some(p), Some(n)) => Some(format!("{p} (last {n})")),
                (Some(p), None) => Some(p.to_string()),
                (None, Some(n)) => Some(format!("last {n} commits")),
                _ => None,
            }
        }
        "git_show" => obj
            .get("revision")
            .and_then(|v| v.as_str())
            .map(|r| truncate_str(r, 40)),
        "git_blame" => {
            let path = obj.get("path").and_then(|v| v.as_str())?;
            let start = obj.get("start_line").and_then(|v| v.as_u64());
            let end = obj.get("end_line").and_then(|v| v.as_u64());
            match (start, end) {
                (Some(s), Some(e)) => Some(format!("{path}:{s}-{e}")),
                _ => Some(path.to_string()),
            }
        }
        "git_log_search" => obj
            .get("query")
            .and_then(|v| v.as_str())
            .map(|q| format!("\"{}\"", truncate_str(q, 50))),
        _ => None,
    }
}

fn fmt_mo_tool(_name: &str, obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    obj.get("sql")
        .and_then(|v| v.as_str())
        .map(|s| truncate_str(s, 60))
}

fn fmt_memory_tool(
    _name: &str,
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    obj.get("query")
        .or_else(|| obj.get("content"))
        .and_then(|v| v.as_str())
        .map(|q| truncate_str(q, 50))
}

fn fmt_default(obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    obj.values()
        .find_map(|v| v.as_str())
        .map(|s| truncate_str(s, 60))
}

pub(super) fn tool_call_detail(name: &str, args: &serde_json::Value) -> Option<String> {
    let obj = args.as_object()?;
    match categorize(name) {
        ToolCat::Github => fmt_github_tool(name, obj),
        ToolCat::File => fmt_file_tool(name, obj),
        ToolCat::Shell => fmt_shell_tool(name, obj),
        ToolCat::Search => fmt_search_tool(name, obj),
        ToolCat::Git => fmt_git_tool(name, obj),
        ToolCat::Mo => fmt_mo_tool(name, obj),
        ToolCat::Memory => fmt_memory_tool(name, obj),
        ToolCat::Other => fmt_default(obj),
    }
}

/// Build a brief summary of a tool result for the status line.
/// Called AFTER execution — extracts useful metrics from the result string.
pub(super) fn tool_result_summary(name: &str, result: &str) -> Option<String> {
    match name {
        "read_file" | "view_file" => {
            let lines = result.lines().count();
            let truncated = result.contains("[truncated");
            if truncated {
                Some(format!("{lines} lines [truncated]"))
            } else if lines > 0 {
                Some(format!("{lines} lines"))
            } else {
                None
            }
        }
        "write_file" => {
            // Parse JSON result: {"success": true, "bytes_written": N, "path": "..."}
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(result) {
                if json
                    .get("success")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    if let Some(n) = json.get("bytes_written").and_then(|v| v.as_u64()) {
                        if n >= 1024 {
                            Some(format!("{:.1}KB written", n as f64 / 1024.0))
                        } else {
                            Some(format!("{n} bytes written"))
                        }
                    } else {
                        Some("written".to_string())
                    }
                } else {
                    json.get("error")
                        .and_then(|v| v.as_str())
                        .map(|err| format!("Error: {err}"))
                }
            } else {
                // Fallback for legacy format
                None
            }
        }
        "str_replace" => {
            if result.starts_with("Replaced successfully") {
                let line_count = result.lines().skip(1).count(); // diff lines
                if line_count > 0 {
                    Some(format!("{line_count} lines changed"))
                } else {
                    Some("replaced".to_string())
                }
            } else {
                None
            }
        }
        "bash" | "run_command" | "shell" | "exec" => {
            let lines = result.lines().count();
            let truncated = result.contains("[truncated]");
            if truncated {
                Some(format!("{lines} lines [truncated]"))
            } else if lines > 3 {
                Some(format!("{lines} lines"))
            } else {
                None // short output, don't clutter
            }
        }
        "grep" | "search" | "find" => {
            if result == "No matches found" {
                Some("0 matches".to_string())
            } else {
                let lines = result.lines().count();
                let truncated = result.contains("[truncated]");
                if truncated {
                    Some(format!("{lines}+ matches [truncated]"))
                } else {
                    Some(format!("{lines} matches"))
                }
            }
        }
        "glob" => {
            if result == "No files found" {
                Some("0 files".to_string())
            } else {
                let count = result.lines().count();
                Some(format!("{count} files"))
            }
        }
        "list_dir" => {
            let count = result.lines().count();
            Some(format!("{count} entries"))
        }
        "git_diff" => {
            if result.trim().is_empty() {
                Some("no changes".to_string())
            } else {
                let adds = result.lines().filter(|l| l.starts_with('+')).count();
                let dels = result.lines().filter(|l| l.starts_with('-')).count();
                if adds > 0 || dels > 0 {
                    Some(format!("+{adds} -{dels}"))
                } else {
                    let lines = result.lines().count();
                    Some(format!("{lines} lines"))
                }
            }
        }
        "git_log" => {
            let commits = result
                .lines()
                .filter(|l| l.starts_with("commit ") || l.starts_with("* "))
                .count();
            if commits > 0 {
                Some(format!("{commits} commits"))
            } else {
                None
            }
        }
        "git_status" => {
            let changed = result.lines().filter(|l| !l.trim().is_empty()).count();
            if changed == 0 {
                Some("clean".to_string())
            } else {
                Some(format!("{changed} files"))
            }
        }
        _ => None,
    }
}

pub(super) fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
}

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

    // ── truncate_str ──────────────────────────────────────────────────────────

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        let result = truncate_str("hello world", 5);
        assert_eq!(result, "hello…");
    }

    #[test]
    fn truncate_exact_length_unchanged() {
        assert_eq!(truncate_str("abc", 3), "abc");
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

    // ── capitalize ────────────────────────────────────────────────────────────

    #[test]
    fn capitalize_lowercase() {
        assert_eq!(capitalize("hello"), "Hello");
    }

    #[test]
    fn capitalize_empty() {
        assert_eq!(capitalize(""), "");
    }

    #[test]
    fn capitalize_already_upper() {
        assert_eq!(capitalize("Hello"), "Hello");
    }

    // ── tool_call_detail ──────────────────────────────────────────────────────

    #[test]
    fn tool_call_detail_github_shows_owner_repo() {
        let detail = tool_call_detail(
            "github_ci_status",
            &serde_json::json!({"owner": "matrixorigin", "repo": "matrixone"}),
        );
        assert_eq!(detail.as_deref(), Some("matrixorigin/matrixone"));
    }

    #[test]
    fn tool_call_detail_bash_shows_command() {
        let detail = tool_call_detail("bash", &serde_json::json!({"command": "ls -la"}));
        assert_eq!(detail.as_deref(), Some("ls -la"));
    }

    #[test]
    fn tool_call_detail_read_file_shows_path() {
        let detail = tool_call_detail("read_file", &serde_json::json!({"path": "src/main.rs"}));
        assert_eq!(detail.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn tool_call_detail_memory_shows_query() {
        let detail = tool_call_detail(
            "memory_search",
            &serde_json::json!({"query": "memoria repo"}),
        );
        assert_eq!(detail.as_deref(), Some("memoria repo"));
    }

    #[test]
    fn tool_call_detail_grep_shows_pattern() {
        let detail = tool_call_detail("grep", &serde_json::json!({"pattern": "TODO"}));
        assert_eq!(detail.as_deref(), Some("\"TODO\""));
    }

    #[test]
    fn tool_call_detail_grep_with_path() {
        let detail = tool_call_detail(
            "grep",
            &serde_json::json!({"pattern": "TODO", "path": "src/"}),
        );
        assert_eq!(detail.as_deref(), Some("\"TODO\" in src/"));
    }

    #[test]
    fn tool_call_detail_read_file_with_line_range() {
        let detail = tool_call_detail(
            "read_file",
            &serde_json::json!({"path": "src/main.rs", "start_line": 10, "end_line": 50}),
        );
        assert_eq!(detail.as_deref(), Some("src/main.rs:10-50"));
    }

    #[test]
    fn tool_call_detail_read_file_outline() {
        let detail = tool_call_detail(
            "read_file",
            &serde_json::json!({"path": "src/main.rs", "outline": true}),
        );
        assert_eq!(detail.as_deref(), Some("src/main.rs (outline)"));
    }

    #[test]
    fn tool_call_detail_str_replace_shows_path_and_lines() {
        let detail = tool_call_detail(
            "str_replace",
            &serde_json::json!({"path": "src/lib.rs", "old_str": "line1\nline2\nline3"}),
        );
        assert_eq!(detail.as_deref(), Some("src/lib.rs (3 lines)"));
    }

    #[test]
    fn tool_call_detail_git_diff_staged() {
        let detail = tool_call_detail("git_diff", &serde_json::json!({"staged": true}));
        assert_eq!(detail.as_deref(), Some("working tree (staged)"));
    }

    #[test]
    fn tool_call_detail_git_log_with_count() {
        let detail = tool_call_detail("git_log", &serde_json::json!({"max_count": 5}));
        assert_eq!(detail.as_deref(), Some("last 5 commits"));
    }

    // ── tool_result_summary ───────────────────────────────────────────────────

    #[test]
    fn result_summary_read_file_line_count() {
        let result = "fn main() {\n    println!(\"hi\");\n}\n";
        let summary = tool_result_summary("read_file", result);
        assert_eq!(summary.as_deref(), Some("3 lines"));
    }

    #[test]
    fn result_summary_read_file_truncated() {
        let result = "some content\n[truncated]";
        let summary = tool_result_summary("read_file", result);
        assert_eq!(summary.as_deref(), Some("2 lines [truncated]"));
    }

    #[test]
    fn result_summary_write_file_bytes() {
        let result = r#"{"success": true, "bytes_written": 2048, "path": "/tmp/foo.rs"}"#;
        let summary = tool_result_summary("write_file", result);
        assert_eq!(summary.as_deref(), Some("2.0KB written"));
    }

    #[test]
    fn result_summary_write_file_small() {
        let result = r#"{"success": true, "bytes_written": 128, "path": "/tmp/foo.rs"}"#;
        let summary = tool_result_summary("write_file", result);
        assert_eq!(summary.as_deref(), Some("128 bytes written"));
    }

    #[test]
    fn result_summary_str_replace() {
        let summary = tool_result_summary("str_replace", "Replaced successfully\n- old\n+ new");
        assert_eq!(summary.as_deref(), Some("2 lines changed"));
    }

    #[test]
    fn result_summary_grep_matches() {
        let result = "src/a.rs:10:match1\nsrc/b.rs:20:match2\nsrc/c.rs:30:match3";
        let summary = tool_result_summary("grep", result);
        assert_eq!(summary.as_deref(), Some("3 matches"));
    }

    #[test]
    fn result_summary_grep_no_matches() {
        let summary = tool_result_summary("grep", "No matches found");
        assert_eq!(summary.as_deref(), Some("0 matches"));
    }

    #[test]
    fn result_summary_glob_files() {
        let result = "src/a.rs\nsrc/b.rs";
        let summary = tool_result_summary("glob", result);
        assert_eq!(summary.as_deref(), Some("2 files"));
    }

    #[test]
    fn result_summary_git_diff_empty() {
        let summary = tool_result_summary("git_diff", "");
        assert_eq!(summary.as_deref(), Some("no changes"));
    }

    #[test]
    fn result_summary_git_status_clean() {
        let summary = tool_result_summary("git_status", "");
        assert_eq!(summary.as_deref(), Some("clean"));
    }

    #[test]
    fn result_summary_bash_short_no_summary() {
        // Short output (<=3 lines) should not clutter the display
        let summary = tool_result_summary("bash", "ok\ndone");
        assert!(summary.is_none());
    }

    #[test]
    fn result_summary_bash_long_shows_lines() {
        let result = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let summary = tool_result_summary("bash", &result);
        assert_eq!(summary.as_deref(), Some("10 lines"));
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
        let err = read_api_error(reqwest::StatusCode::NOT_FOUND, "not found");
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
            std::env::set_var("MO_AGENT_CREDENTIALS_DIR", creds_dir.path());
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
            std::env::remove_var("MO_AGENT_CREDENTIALS_DIR");
        }
    }
}
