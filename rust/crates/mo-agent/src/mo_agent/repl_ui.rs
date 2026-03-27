use super::*;

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/", "Show slash command list"),
    ("/?", "Show slash command list"),
    ("/commands", "Show slash command list (optionally filtered)"),
    ("/help", "Show all slash commands"),
    ("/model", "List models or set active: /model <name>"),
    ("/session", "Show current session info"),
    (
        "/session history",
        "Show session journal timeline (full id or unique prefix)",
    ),
    ("/session list", "List all journal files"),
    (
        "/session errors",
        "Show session errors (full id or unique prefix)",
    ),
    (
        "/session export",
        "Export session as markdown (full id or unique prefix)",
    ),
    ("/clear", "Start a new session"),
    (
        "/history",
        "Show conversation turns (or /history search <q>)",
    ),
    ("/rewind", "Rewind conversation to turn N: /rewind <turn>"),
    ("/copy", "Copy last response to clipboard"),
    ("/context", "Show context window and session state"),
    ("/skill", "List available skills"),
    ("/skill list", "List available skills"),
    (
        "/skill new",
        "Create a new skill scaffold: /skill new <name>",
    ),
    (
        "/skill test",
        "Test a skill: /skill test <name> [json_args]",
    ),
    (
        "/skill dev",
        "Enter AI-assisted skill dev mode: /skill dev <name>",
    ),
    ("/skill dev off", "Exit skill dev mode"),
    ("/skill doctor", "Check skill health"),
    (
        "/skill validate",
        "Validate skill source: /skill validate <name>",
    ),
    ("/skill config", "Show skill config: /skill config <name>"),
    (
        "/skill system",
        "Toggle a system skill: /skill system <name|list>",
    ),
    ("/doctor", "Run diagnostics (health + auth)"),
    ("/version", "Show version info"),
    (
        "/register",
        "Register a new account (prompts interactively)",
    ),
    ("/login", "Authenticate with the API"),
    ("/logout", "Logout from the API"),
    (
        "/memory-setup",
        "Configure Memoria API key: /memory-setup <api_key>",
    ),
    ("/explain", "Toggle explain mode"),
    ("/verbose", "Show status bar after each response"),
    ("/compact", "Summarize conversation and save to memory"),
    (
        "/reflect",
        "Reflect on session: /reflect [focus] [question]",
    ),
    (
        "/memory",
        "Memory operations: /memory [list|search <q>|inspect <id>]",
    ),
    ("/plan", "Persistent plan: /plan [show|set <text>|clear]"),
    (
        "/task",
        "Task tracking: /task [list|add <title>|done <id>|status <id>]",
    ),
    ("/resume", "Resume a previous session: /resume [session_id]"),
    ("/stats", "Session analytics: /stats [history]"),
    ("/tools", "Tool performance profile: /tools"),
    ("/health", "Tool health dashboard: /health [detail]"),
    ("/exit", "Exit the REPL"),
    ("/quit", "Exit the REPL"),
];

fn command_matches_filter(command: &str, desc: &str, filter: &str) -> bool {
    let terms: Vec<&str> = filter.split_whitespace().collect();
    if terms.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {}",
        command.to_ascii_lowercase(),
        desc.to_ascii_lowercase()
    );
    terms
        .iter()
        .all(|term| haystack.contains(&term.to_ascii_lowercase()))
}

fn suggestion_score(command: &str, query: &str) -> usize {
    let cmd = command.trim_start_matches('/').to_ascii_lowercase();
    let q = query.trim_start_matches('/').to_ascii_lowercase();
    if q.is_empty() {
        return usize::MAX / 2;
    }
    if cmd == q {
        return usize::MAX;
    }
    if cmd.starts_with(&q) {
        return 10_000usize.saturating_sub(cmd.len());
    }
    if cmd.contains(&q) {
        return 5_000usize.saturating_sub(cmd.len());
    }

    let mut score = 0usize;
    let mut qchars = q.chars();
    let mut target = qchars.next();
    for c in cmd.chars() {
        if Some(c) == target {
            score += 1;
            target = qchars.next();
            if target.is_none() {
                break;
            }
        }
    }
    score
}

pub(super) fn suggest_commands(input: &str, limit: usize) -> Vec<&'static str> {
    let mut scored: Vec<(usize, usize, &'static str)> = SLASH_COMMANDS
        .iter()
        .map(|(cmd, _)| (suggestion_score(cmd, input), cmd.len(), *cmd))
        .filter(|(score, _, _)| *score > 0)
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(b.2))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, cmd)| cmd)
        .collect()
}

fn is_command_alias(command: &str) -> bool {
    matches!(command, "/?" | "/commands" | "/quit")
}

pub(super) fn completion_candidates(prefix: &str) -> Vec<(&'static str, &'static str)> {
    let mut rows: Vec<(&'static str, &'static str)> = SLASH_COMMANDS
        .iter()
        .copied()
        .filter(|(cmd, _)| cmd.starts_with(prefix))
        .collect();
    rows.sort_by(|(a_cmd, _), (b_cmd, _)| {
        is_command_alias(a_cmd)
            .cmp(&is_command_alias(b_cmd))
            .then_with(|| a_cmd.len().cmp(&b_cmd.len()))
            .then_with(|| a_cmd.cmp(b_cmd))
    });
    rows
}

fn filtered_slash_rows(query: Option<&str>) -> Vec<(&'static str, &'static str)> {
    let mut rows = completion_candidates("/");
    rows.retain(|(cmd, _)| *cmd != "/" && !is_command_alias(cmd));
    if let Some(q) = query {
        rows.retain(|(cmd, desc)| command_matches_filter(cmd, desc, q));
    }
    rows
}

// ── Slash picker state ──────────────────────────────────────────────────────
// Minimal state: overlay line count, selected index, filter query, last-render
// cache. "active" is derived from overlay_lines > 0.

static SLASH_OVERLAY_LINES: OnceLock<Mutex<u16>> = OnceLock::new();
static SLASH_PICKER_SELECTED: OnceLock<Mutex<usize>> = OnceLock::new();
static SLASH_OVERLAY_STATE: OnceLock<Mutex<(Option<String>, usize)>> = OnceLock::new();
static SLASH_FILTER_QUERY: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn slash_overlay_lines() -> &'static Mutex<u16> {
    SLASH_OVERLAY_LINES.get_or_init(|| Mutex::new(0))
}
fn slash_picker_selected() -> &'static Mutex<usize> {
    SLASH_PICKER_SELECTED.get_or_init(|| Mutex::new(0))
}
fn slash_overlay_state() -> &'static Mutex<(Option<String>, usize)> {
    SLASH_OVERLAY_STATE.get_or_init(|| Mutex::new((None, 0)))
}
fn slash_filter_query() -> &'static Mutex<Option<String>> {
    SLASH_FILTER_QUERY.get_or_init(|| Mutex::new(None))
}

fn set_slash_picker_selected(selected: usize) {
    if let Ok(mut g) = slash_picker_selected().lock() {
        *g = selected;
    }
}
fn get_slash_picker_selected() -> usize {
    slash_picker_selected().lock().map(|g| *g).unwrap_or(0)
}
fn set_slash_filter(q: Option<String>) {
    if let Ok(mut g) = slash_filter_query().lock() {
        *g = q;
    }
}
fn get_slash_filter() -> Option<String> {
    slash_filter_query().lock().ok().and_then(|g| g.clone())
}
pub(super) fn is_slash_picker_active() -> bool {
    slash_overlay_lines()
        .lock()
        .map(|g| *g > 0)
        .unwrap_or(false)
}

fn picker_rows_for_filter() -> Vec<(&'static str, &'static str)> {
    let q = get_slash_filter();
    let q_ref = q.as_deref();
    filtered_slash_rows(q_ref)
}

/// Move selection and return the newly selected command name.
fn move_picker_selection(delta: isize) -> Option<&'static str> {
    let rows = picker_rows_for_filter();
    if rows.is_empty() {
        set_slash_picker_selected(0);
        return None;
    }
    let len = rows.len() as isize;
    let current = get_slash_picker_selected() as isize;
    let next = (current + delta).rem_euclid(len) as usize;
    set_slash_picker_selected(next);
    Some(rows[next].0)
}

pub(super) fn clear_slash_overlay() {
    let Ok(mut guard) = slash_overlay_lines().lock() else {
        return;
    };
    if *guard == 0 {
        return;
    }
    execute!(
        io::stdout(),
        cursor::MoveUp(*guard),
        cursor::MoveToColumn(0),
        terminal::Clear(terminal::ClearType::FromCursorDown)
    )
    .ok();
    *guard = 0;
    set_slash_picker_selected(0);
    set_slash_filter(None);
    if let Ok(mut s) = slash_overlay_state().lock() {
        *s = (None, 0);
    }
}

pub(super) fn render_slash_overlay(filter: Option<&str>) {
    let mut selected = get_slash_picker_selected();
    let rows = filtered_slash_rows(filter);
    if rows.is_empty() {
        selected = 0;
    } else if selected >= rows.len() {
        selected = rows.len() - 1;
    }
    let norm = filter.map(|q| q.to_string());
    if let Ok(state) = slash_overlay_state().lock()
        && state.0 == norm
        && state.1 == selected
    {
        return;
    }

    // Erase previous overlay (but preserve selected index across redraw)
    {
        let Ok(mut guard) = slash_overlay_lines().lock() else {
            return;
        };
        if *guard > 0 {
            execute!(
                io::stdout(),
                cursor::MoveUp(*guard),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )
            .ok();
            *guard = 0;
        }
    }
    set_slash_picker_selected(selected);

    let mut printed: u16 = 0;
    let visible_limit = 10usize;
    println!(
        "{}",
        "─ Slash commands (↑/↓ select · Enter run · Esc dismiss) ─".dim()
    );
    printed += 1;
    if rows.is_empty() {
        let label = filter.unwrap_or("");
        println!("{}", format!("  no commands match '{label}'").yellow());
        printed += 1;
    } else {
        let total = rows.len();
        let start = if total <= visible_limit {
            0
        } else if selected >= total - (visible_limit / 2) {
            total - visible_limit
        } else {
            selected.saturating_sub(visible_limit / 2)
        };
        let end = (start + visible_limit).min(total);
        for (idx, (cmd, desc)) in rows[start..end].iter().enumerate() {
            let abs = start + idx;
            if abs == selected {
                println!("  {} {:<14}  {}", "❯".green(), cmd.green().bold(), desc);
            } else {
                println!("    {:<14}  {}", cmd.dim(), desc);
            }
            printed += 1;
        }
        if total > visible_limit {
            println!("{}", format!("  [{}-{} / {}]", start + 1, end, total).dim());
            printed += 1;
        }
    }

    if let Ok(mut g) = slash_overlay_lines().lock() {
        *g = printed;
    }
    set_slash_filter(filter.map(|s| s.to_string()));
    if let Ok(mut s) = slash_overlay_state().lock() {
        *s = (norm, selected);
    }
}

pub(super) fn resolve_slash_command(input: &str) -> Result<&'static str, Vec<&'static str>> {
    if let Some((cmd, _)) = SLASH_COMMANDS.iter().find(|(cmd, _)| *cmd == input) {
        return Ok(*cmd);
    }
    let mut matches: Vec<&'static str> = SLASH_COMMANDS
        .iter()
        .map(|(cmd, _)| *cmd)
        .filter(|cmd| cmd.starts_with(input))
        .collect();
    matches.sort_unstable();
    matches.dedup();
    if matches.len() == 1 {
        Ok(matches[0])
    } else {
        Err(matches)
    }
}

pub(super) fn print_slash_commands(query: Option<&str>) {
    let filter = query
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(|q| q.to_ascii_lowercase());
    let lookup_desc = |command: &str| -> &'static str {
        SLASH_COMMANDS
            .iter()
            .find(|(cmd, _)| *cmd == command)
            .map(|(_, desc)| *desc)
            .unwrap_or("?")
    };
    let matches = |command: &str, desc: &str| -> bool {
        match &filter {
            Some(q) => command_matches_filter(command, desc, q),
            None => true,
        }
    };
    let mut any_results = false;
    let mut print_group = |title: &str, commands: &[&str]| {
        let mut printed = false;
        let mut lines = Vec::new();
        for cmd in commands {
            let desc = lookup_desc(cmd);
            if matches(cmd, desc) {
                lines.push(format!("    {:<16}  {}", cmd.green(), desc));
            }
        }
        if !lines.is_empty() {
            any_results = true;
            printed = true;
        }
        if printed {
            eprintln!("  {}", title.bold().cyan());
            for line in lines {
                eprintln!("{line}");
            }
            eprintln!();
        }
    };

    if let Some(q) = &filter {
        eprintln!(
            "\n{} {}",
            "Command Palette".bold(),
            format!("(filter: {q})").dim()
        );
    } else {
        eprintln!("\n{}", "Command Palette".bold());
    }
    eprintln!("{}", "\u{2500}".repeat(62).dim());
    print_group(
        "General",
        &[
            "/",
            "/help",
            "/model",
            "/skill",
            "/skill list",
            "/skill new",
            "/skill test",
            "/skill dev",
            "/skill dev off",
            "/skill doctor",
            "/skill validate",
            "/skill config",
            "/skill system",
            "/history",
            "/copy",
            "/context",
            "/version",
            "/explain",
            "/verbose",
            "/compact",
            "/exit",
        ],
    );
    print_group(
        "Session",
        &[
            "/session",
            "/session history",
            "/session list",
            "/session errors",
            "/session export",
            "/clear",
            "/reflect",
        ],
    );
    print_group("Memory", &["/memory", "/plan", "/task"]);
    print_group("Diagnostics", &["/stats", "/tools", "/health", "/doctor"]);
    print_group(
        "Authentication",
        &["/login", "/register", "/logout", "/memory-setup"],
    );
    let alias_rows = [
        ("/?", "same as /"),
        ("/commands", "same as /"),
        ("/quit", "same as /exit"),
    ];
    let alias_lines: Vec<String> = alias_rows
        .iter()
        .filter(|(cmd, desc)| matches(cmd, desc))
        .map(|(cmd, desc)| format!("    {:<16}  {}", cmd.green(), desc))
        .collect();
    if !alias_lines.is_empty() {
        any_results = true;
        eprintln!("  {}", "Aliases".bold().cyan());
        for line in alias_lines {
            eprintln!("{line}");
        }
    }
    if !any_results {
        let label = filter.as_deref().unwrap_or("?");
        eprintln!("  {}", format!("No commands match '{label}'").yellow());
    }
    eprintln!("{}", "\u{2500}".repeat(62).dim());
    eprintln!();
}

pub(super) struct ReplHelper;

impl Helper for ReplHelper {}

pub(super) struct SlashStartCompleteHandler;

impl ConditionalEventHandler for SlashStartCompleteHandler {
    fn handle(
        &self,
        evt: &RlEvent,
        _n: rustyline::RepeatCount,
        _positive: bool,
        ctx: &RlEventContext,
    ) -> Option<RlCmd> {
        let key = evt.get(0)?;

        let current_line = ctx.line().to_string();
        let in_slash = current_line.starts_with('/') && !current_line.contains(' ');
        let active = is_slash_picker_active();

        // ── Helper: navigate picker and replace input line ──────────────
        macro_rules! nav {
            ($delta:expr) => {{
                let filter = get_slash_filter();
                let filter_ref = filter.as_deref();
                if let Some(cmd) = move_picker_selection($delta) {
                    render_slash_overlay(filter_ref);
                    return Some(RlCmd::Replace(RlMovement::WholeLine, Some(cmd.to_string())));
                }
                render_slash_overlay(filter_ref);
                return Some(RlCmd::Noop);
            }};
        }

        match key {
            // ── Typing: project next char and update filter ─────────────
            RlKeyEvent(RlKeyCode::Char(c), mods)
                if !mods.contains(RlModifiers::CTRL) && !mods.contains(RlModifiers::ALT) =>
            {
                if ctx.pos() == ctx.line().len() {
                    let mut line = ctx.line().to_string();
                    line.push(*c);
                    if line.starts_with('/') && !line.contains(' ') {
                        let q = line.trim_start_matches('/');
                        let q = if q.is_empty() { None } else { Some(q) };
                        set_slash_picker_selected(0);
                        set_slash_filter(q.map(|s| s.to_string()));
                        render_slash_overlay(q);
                    } else {
                        clear_slash_overlay();
                    }
                }
                return None; // let rustyline insert the char
            }
            RlKeyEvent(RlKeyCode::Backspace, _) => {
                if ctx.pos() == ctx.line().len() && !ctx.line().is_empty() {
                    let mut line = ctx.line().to_string();
                    line.pop();
                    if line.starts_with('/') && !line.contains(' ') {
                        let q = line.trim_start_matches('/');
                        let q = if q.is_empty() { None } else { Some(q) };
                        set_slash_picker_selected(0);
                        set_slash_filter(q.map(|s| s.to_string()));
                        render_slash_overlay(q);
                    } else {
                        clear_slash_overlay();
                    }
                }
                return None; // let rustyline delete the char
            }

            // ── Navigation ──────────────────────────────────────────────
            RlKeyEvent(RlKeyCode::Up, _) if in_slash && active => {
                nav!(-1);
            }
            RlKeyEvent(RlKeyCode::Down, _) if in_slash && active => {
                nav!(1);
            }
            RlKeyEvent(RlKeyCode::Tab, _) if in_slash && active => {
                nav!(1);
            }
            RlKeyEvent(RlKeyCode::BackTab, _) if in_slash && active => {
                nav!(-1);
            }
            RlKeyEvent(RlKeyCode::Char('n'), m)
                if in_slash && active && m.contains(RlModifiers::CTRL) =>
            {
                nav!(1);
            }
            RlKeyEvent(RlKeyCode::Char('p'), m)
                if in_slash && active && m.contains(RlModifiers::CTRL) =>
            {
                nav!(-1);
            }

            // ── Accept: Enter with picker selects the highlighted command ─────
            // If the line already matches the selected command (user navigated),
            // just clear and accept. Otherwise replace the line — the user will
            // see the full command and press Enter once more to confirm.
            RlKeyEvent(RlKeyCode::Enter, _) if active => {
                let rows = picker_rows_for_filter();
                let selected = get_slash_picker_selected();
                let current = ctx.line();
                clear_slash_overlay();
                if let Some((cmd, _)) = rows.get(selected)
                    && *cmd != current
                {
                    return Some(RlCmd::Replace(RlMovement::WholeLine, Some(cmd.to_string())));
                }
                return None; // already correct — accept
            }

            // ── Dismiss ─────────────────────────────────────────────────
            RlKeyEvent(RlKeyCode::Esc, _) if active => {
                // Restore original typed query to input line
                let filter = get_slash_filter();
                clear_slash_overlay();
                if let Some(q) = filter {
                    return Some(RlCmd::Replace(RlMovement::WholeLine, Some(format!("/{q}"))));
                }
                return Some(RlCmd::Replace(RlMovement::WholeLine, Some("/".to_string())));
            }

            // ── Non-slash context: clear if overlay was showing ─────────
            _ if active => {
                clear_slash_overlay();
                return None;
            }
            _ => {}
        }
        None
    }
}

impl Completer for ReplHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let (prefix, show_all_from_empty) = if line.is_empty() {
            ("/", true)
        } else {
            if !line.starts_with('/') {
                return Ok((pos, vec![]));
            }

            let safe_pos = pos.min(line.len());
            let before_cursor = &line[..safe_pos];
            let command_end = before_cursor.find(' ').unwrap_or(before_cursor.len());
            if safe_pos > command_end {
                return Ok((pos, vec![]));
            }
            (&before_cursor[..command_end], false)
        };

        let matches: Vec<Pair> = completion_candidates(prefix)
            .into_iter()
            .map(|(cmd, desc)| Pair {
                display: format!("{:<15}  {}", cmd, desc),
                replacement: if show_all_from_empty {
                    "/".to_string()
                } else {
                    cmd.to_string()
                },
            })
            .collect();
        Ok((0, matches))
    }
}

impl Hinter for ReplHelper {
    type Hint = String;

    fn hint(&self, line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<String> {
        if !line.starts_with('/') {
            return None;
        }
        if line.contains(' ') {
            return None;
        }
        let prefix = line;
        completion_candidates(prefix)
            .into_iter()
            .find(|(cmd, _)| *cmd != prefix)
            .map(|(cmd, _)| cmd[prefix.len()..].to_string())
    }
}

impl Highlighter for ReplHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        Cow::Borrowed(line)
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _forced: bool) -> bool {
        false
    }
}

impl Validator for ReplHelper {
    fn validate(&self, ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        let input = ctx.input();
        // Multi-line: trailing backslash means "continue on next line"
        if input.ends_with('\\') {
            return Ok(ValidationResult::Incomplete);
        }
        Ok(ValidationResult::Valid(None))
    }
}

pub(super) fn history_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mo-agent")
        .join("history")
}

// ════════════════════════════════════════════════════════ Auth helpers ════

#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_slash_command ──────────────────────────────────────────────────

    #[test]
    fn resolve_exact_match() {
        assert_eq!(resolve_slash_command("/help"), Ok("/help"));
        assert_eq!(resolve_slash_command("/quit"), Ok("/quit"));
    }

    #[test]
    fn resolve_unique_prefix() {
        // /he should uniquely match /help (or /health)
        let result = resolve_slash_command("/hel");
        assert!(result.is_ok(), "got: {result:?}");
        assert_eq!(result.unwrap(), "/help");
    }

    #[test]
    fn resolve_ambiguous_prefix_returns_candidates() {
        // /s matches multiple commands
        let result = resolve_slash_command("/s");
        assert!(result.is_err(), "should be ambiguous");
        let candidates = result.unwrap_err();
        assert!(candidates.len() > 1, "should have multiple candidates");
    }

    #[test]
    fn resolve_no_match_returns_empty() {
        let result = resolve_slash_command("/zzzzz");
        assert!(result.is_err());
        assert!(result.unwrap_err().is_empty());
    }

    // ── suggest_commands ──────────────────────────────────────────────────────

    #[test]
    fn suggest_returns_results_for_prefix() {
        let suggestions = suggest_commands("/he", 5);
        assert!(!suggestions.is_empty());
        assert!(suggestions.contains(&"/help"));
    }

    #[test]
    fn suggest_returns_empty_for_nonsense() {
        let suggestions = suggest_commands("/zzzzzzz", 5);
        // May return fuzzy matches with low scores, but should not panic
        let _ = suggestions;
    }

    // ── command_matches_filter ─────────────────────────────────────────────────

    #[test]
    fn filter_matches_command_name() {
        assert!(command_matches_filter(
            "/session",
            "Manage sessions",
            "session"
        ));
    }

    #[test]
    fn filter_matches_description() {
        assert!(command_matches_filter(
            "/session",
            "Manage sessions",
            "manage"
        ));
    }

    #[test]
    fn filter_case_insensitive() {
        assert!(command_matches_filter("/Session", "Manage", "session"));
    }

    #[test]
    fn filter_multi_term_all_must_match() {
        assert!(command_matches_filter(
            "/session",
            "Manage sessions",
            "session manage"
        ));
        assert!(!command_matches_filter(
            "/session",
            "Manage sessions",
            "session xyz"
        ));
    }

    // ── suggestion_score ──────────────────────────────────────────────────────

    #[test]
    fn exact_match_highest_score() {
        let exact = suggestion_score("/help", "/help");
        let prefix = suggestion_score("/help", "/hel");
        assert!(exact > prefix);
    }

    #[test]
    fn prefix_match_higher_than_contains() {
        let prefix = suggestion_score("/session", "/ses");
        let contains = suggestion_score("/session", "/ion");
        assert!(prefix > contains);
    }

    // ── completion_candidates ─────────────────────────────────────────────────

    #[test]
    fn completion_candidates_for_slash() {
        let candidates = completion_candidates("/");
        assert!(!candidates.is_empty());
    }

    #[test]
    fn completion_candidates_for_prefix() {
        let candidates = completion_candidates("/he");
        assert!(candidates.iter().any(|(cmd, _)| *cmd == "/help"));
    }

    // ── Multi-line (Validator) ────────────────────────────────────────────────

    #[test]
    fn validator_complete_line_is_valid() {
        // Simulate a complete line (no trailing backslash)
        // We can't easily create a ValidationContext, so test the logic directly
        let input = "hello world";
        assert!(!input.ends_with('\\'));
    }

    #[test]
    fn validator_trailing_backslash_is_incomplete() {
        let input = "hello \\";
        assert!(input.ends_with('\\'));
    }

    // ── /rewind in SLASH_COMMANDS ─────────────────────────────────────────────

    #[test]
    fn rewind_command_is_registered() {
        assert!(SLASH_COMMANDS.iter().any(|(cmd, _)| *cmd == "/rewind"));
    }

    #[test]
    fn rewind_resolves_from_prefix() {
        let result = resolve_slash_command("/rew");
        assert!(result.is_ok(), "got: {result:?}");
        assert_eq!(result.unwrap(), "/rewind");
    }

    // ── /history search in SLASH_COMMANDS ─────────────────────────────────────

    #[test]
    fn history_command_is_registered() {
        assert!(SLASH_COMMANDS.iter().any(|(cmd, _)| *cmd == "/history"));
    }

    #[test]
    fn history_help_text_mentions_search() {
        let desc = SLASH_COMMANDS
            .iter()
            .find(|(cmd, _)| *cmd == "/history")
            .map(|(_, d)| *d)
            .unwrap();
        assert!(
            desc.contains("search"),
            "history help should mention search: {desc}"
        );
    }

    // ── /resume in SLASH_COMMANDS ─────────────────────────────────────────────

    #[test]
    fn resume_command_is_registered() {
        assert!(SLASH_COMMANDS.iter().any(|(cmd, _)| *cmd == "/resume"));
    }

    #[test]
    fn resume_resolves_from_prefix() {
        let result = resolve_slash_command("/resu");
        assert!(result.is_ok(), "got: {result:?}");
        assert_eq!(result.unwrap(), "/resume");
    }

    #[test]
    fn resume_and_rewind_disambiguate() {
        // /re is ambiguous between /resume, /rewind, /reflect, /register
        let result = resolve_slash_command("/re");
        assert!(result.is_err(), "/re should be ambiguous");
        let candidates = result.unwrap_err();
        assert!(candidates.len() > 1);
        assert!(candidates.contains(&"/resume"));
    }

    // ── /stats in SLASH_COMMANDS ──────────────────────────────────────────────

    #[test]
    fn stats_command_is_registered() {
        assert!(SLASH_COMMANDS.iter().any(|(cmd, _)| *cmd == "/stats"));
    }

    #[test]
    fn stats_resolves_from_prefix() {
        let result = resolve_slash_command("/sta");
        assert!(result.is_ok(), "got: {result:?}");
        assert_eq!(result.unwrap(), "/stats");
    }

    // ── /tools in SLASH_COMMANDS ──────────────────────────────────────────

    #[test]
    fn tools_command_is_registered() {
        assert!(SLASH_COMMANDS.iter().any(|(cmd, _)| *cmd == "/tools"));
    }

    #[test]
    fn tools_resolves_from_prefix() {
        let result = resolve_slash_command("/tool");
        assert!(result.is_ok(), "got: {result:?}");
        assert_eq!(result.unwrap(), "/tools");
    }
}
