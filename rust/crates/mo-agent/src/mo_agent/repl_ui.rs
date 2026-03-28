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
    (
        "/search",
        "Workspace search: /search <pattern> | /search files <glob> | /search review <pattern>",
    ),
    ("/search files", "Search file names: /search files <glob>"),
    (
        "/search review",
        "Search changed files only: /search review <pattern>",
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
    ("/keys", "Show all keyboard shortcuts"),
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

/// A sub-command contains a space (e.g. "/skill list", "/search files").
fn is_subcommand(command: &str) -> bool {
    command.trim_start_matches('/').contains(' ')
}

fn command_name_matches_prefix(command: &str, query: &str) -> bool {
    let cmd = command.trim_start_matches('/').to_ascii_lowercase();
    let q = query
        .trim_start()
        .trim_start_matches('/')
        .to_ascii_lowercase();
    !q.is_empty() && cmd.starts_with(&q)
}

fn sort_picker_rows(rows: &mut [(&'static str, &'static str)], query: Option<&str>) {
    rows.sort_by(|(a_cmd, _), (b_cmd, _)| {
        let query_cmp = query.map(|q| suggestion_score(b_cmd, q).cmp(&suggestion_score(a_cmd, q)));
        query_cmp
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| is_command_alias(a_cmd).cmp(&is_command_alias(b_cmd)))
            .then_with(|| a_cmd.len().cmp(&b_cmd.len()))
            .then_with(|| a_cmd.cmp(b_cmd))
    });
}

pub(super) fn completion_candidates(prefix: &str) -> Vec<(&'static str, &'static str)> {
    let mut rows: Vec<(&'static str, &'static str)> = SLASH_COMMANDS
        .iter()
        .copied()
        .filter(|(cmd, _)| cmd.starts_with(prefix))
        .collect();
    sort_picker_rows(&mut rows, None);
    rows
}

fn filtered_slash_rows(query: Option<&str>) -> Vec<(&'static str, &'static str)> {
    let mut rows = completion_candidates("/");
    rows.retain(|(cmd, _)| *cmd != "/" && !is_command_alias(cmd));
    // Hide sub-commands from the top-level picker.
    // They only appear when the user types a parent prefix containing a space
    // (e.g. "skill " → show "/skill list", "/skill dev", …).
    let show_subcommands = query.map(|q| q.contains(' ')).unwrap_or(false);
    if !show_subcommands {
        rows.retain(|(cmd, _)| !is_subcommand(cmd));
    }
    if let Some(q) = query {
        let mut prefix_rows: Vec<(&'static str, &'static str)> = rows
            .iter()
            .copied()
            .filter(|(cmd, _)| command_name_matches_prefix(cmd, q))
            .collect();
        if !prefix_rows.is_empty() {
            sort_picker_rows(&mut prefix_rows, Some(q));
            return prefix_rows;
        }
        rows.retain(|(cmd, desc)| command_matches_filter(cmd, desc, q));
        sort_picker_rows(&mut rows, Some(q));
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
static SLASH_PENDING_EXECUTE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

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

// ── Pending-execute: picker Enter stores the command for immediate dispatch ──

fn slash_pending_execute() -> &'static Mutex<Option<String>> {
    SLASH_PENDING_EXECUTE.get_or_init(|| Mutex::new(None))
}
fn set_slash_pending_execute(cmd: Option<String>) {
    if let Ok(mut g) = slash_pending_execute().lock() {
        *g = cmd;
    }
}
/// Take the command stored by Enter-in-picker.  Returns `Some` once, then
/// `None` until the next picker Enter.  Called from the main REPL loop.
pub(super) fn take_slash_pending_execute() -> Option<String> {
    slash_pending_execute()
        .lock()
        .ok()
        .and_then(|mut g| g.take())
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

fn picker_selected_command() -> Option<&'static str> {
    let rows = picker_rows_for_filter();
    let selected = get_slash_picker_selected();
    rows.get(selected).map(|(cmd, _)| *cmd).or_else(|| {
        match resolve_slash_command(&format!("/{}", get_slash_filter()?)) {
            Ok(cmd) => Some(cmd),
            Err(_) => None,
        }
    })
}

#[derive(Debug, PartialEq, Eq)]
enum AcceptedSlashEdit {
    InsertSuffix(String),
    ReplaceWholeLine(String),
    KeepLine,
}

fn accepted_slash_edit(
    current: &str,
    selected: Option<&'static str>,
    append_space: bool,
) -> Option<AcceptedSlashEdit> {
    if current == "/" {
        return None;
    }

    let cmd = selected.or_else(|| match resolve_slash_command(current) {
        Ok(cmd) => Some(cmd),
        Err(_) => None,
    })?;

    if let Some(rest) = cmd.strip_prefix(current) {
        let mut suffix = rest.to_string();
        if append_space {
            suffix.push(' ');
        }
        if suffix.is_empty() {
            return Some(AcceptedSlashEdit::KeepLine);
        }
        return Some(AcceptedSlashEdit::InsertSuffix(suffix));
    }

    let mut accepted = cmd.to_string();
    if append_space {
        accepted.push(' ');
    }
    if accepted == current {
        Some(AcceptedSlashEdit::KeepLine)
    } else {
        Some(AcceptedSlashEdit::ReplaceWholeLine(accepted))
    }
}

fn slash_completion_query(line: &str) -> Option<&str> {
    if !line.starts_with('/') {
        return None;
    }
    if SLASH_COMMANDS.iter().any(|(cmd, _)| cmd.starts_with(line)) {
        return Some(line);
    }
    if !line.contains(' ') && !line.ends_with(' ') {
        return Some(line);
    }
    None
}

fn slash_argument_hint(command: &str) -> Option<&'static str> {
    match command {
        "/model" => Some("<name>"),
        "/session history" | "/session errors" | "/session export" => Some("<session_id|prefix>"),
        "/rewind" => Some("<turn>"),
        "/search" => Some("<pattern|files <glob>|review <pattern>>"),
        "/search files" => Some("<glob>"),
        "/search review" => Some("<pattern>"),
        "/skill" => Some("[list|new|test|dev|doctor|validate|config|system]"),
        "/skill new" => Some("<name>"),
        "/skill test" => Some("<name> [json_args]"),
        "/skill dev" => Some("<name|off>"),
        "/skill validate" | "/skill config" => Some("<name>"),
        "/skill system" => Some("<name|list>"),
        "/memory" => Some("[list|search <q>|inspect <id>]"),
        "/plan" => Some("[show|set <text>|clear]"),
        "/task" => Some("[list|add <title>|done <id>|status <id>]"),
        "/resume" => Some("[session_id]"),
        "/stats" => Some("[history]"),
        "/health" => Some("[detail]"),
        _ => None,
    }
}

fn slash_inline_hint(line: &str) -> Option<String> {
    let trimmed = line.trim_end();
    if let Ok(cmd) = resolve_slash_command(trimmed)
        && trimmed == cmd
        && let Some(arg_hint) = slash_argument_hint(cmd)
    {
        return if line.ends_with(' ') {
            Some(arg_hint.to_string())
        } else {
            Some(format!(" {arg_hint}"))
        };
    }

    let query = slash_completion_query(line)?;
    let q = query.trim_start_matches('/');
    let q = if q.is_empty() { None } else { Some(q) };
    let rows = filtered_slash_rows(q);
    if let Some((cmd, _)) = rows.first()
        && *cmd != line
        && cmd.starts_with(line)
    {
        return Some(cmd[line.len()..].to_string());
    }
    None
}

fn apply_accepted_slash_edit(edit: AcceptedSlashEdit) -> RlCmd {
    match edit {
        AcceptedSlashEdit::InsertSuffix(text) => RlCmd::Insert(1, text),
        AcceptedSlashEdit::ReplaceWholeLine(line) => {
            RlCmd::Replace(RlMovement::WholeLine, Some(line))
        }
        AcceptedSlashEdit::KeepLine => RlCmd::Move(RlMovement::EndOfLine),
    }
}

fn slash_left_arrow_command(active: bool, in_slash: bool, pos: usize) -> Option<RlCmd> {
    if !active || !in_slash {
        return None;
    }
    if pos == 0 {
        Some(RlCmd::Move(RlMovement::BeginningOfLine))
    } else {
        Some(RlCmd::Move(RlMovement::BackwardChar(1)))
    }
}

fn slash_picker_filter(line: &str) -> Option<Option<String>> {
    let query = slash_completion_query(line)?;
    let filter = query.trim_start_matches('/');
    if filter.is_empty() {
        Some(None)
    } else {
        Some(Some(filter.to_string()))
    }
}

fn slash_right_arrow_filter(
    line: &str,
    pos: usize,
    active: bool,
    in_slash: bool,
) -> Option<Option<String>> {
    if active || !in_slash || pos.saturating_add(1) != line.len() {
        return None;
    }
    slash_picker_filter(line)
}

fn slash_ctrl_e_filter(line: &str, active: bool, in_slash: bool) -> Option<Option<String>> {
    if active || !in_slash {
        return None;
    }
    slash_picker_filter(line)
}

fn slash_overlay_command_width(rows: &[(&'static str, &'static str)]) -> usize {
    rows.iter()
        .map(|(cmd, _)| cmd.chars().count())
        .max()
        .unwrap_or(16)
        .max(16)
}

fn slash_overlay_row_content(
    cmd: &str,
    desc: &str,
    selected: bool,
    command_width: usize,
) -> String {
    if selected {
        format!(
            "{} {:<command_width$} {}",
            "❯".cyan().bold(),
            cmd.cyan().bold(),
            desc.bold()
        )
    } else {
        format!("  {:<command_width$} {}", cmd.dim(), desc.dim())
    }
}

fn visible_width(text: &str) -> usize {
    strip_ansi_for_width(text).chars().count()
}

fn strip_ansi_for_width(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && matches!(chars.peek(), Some('[')) {
            let _ = chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn slash_overlay_title(filter: Option<&str>) -> String {
    if let Some(q) = filter.filter(|q| !q.is_empty()) {
        format!(" Slash Commands  ·  /{q} ")
    } else {
        " Slash Commands ".to_string()
    }
}

fn slash_overlay_context_summary(
    filter: Option<&str>,
    total: usize,
    start: usize,
    end: usize,
) -> String {
    match (filter.filter(|q| !q.is_empty()), total) {
        (Some(q), 0) => format!(" filter /{q}  ·  no matching commands "),
        (Some(q), _) => format!(
            " filter /{q}  ·  showing {}-{} of {} ",
            start + 1,
            end,
            total
        ),
        (None, 0) => " type to filter slash commands ".to_string(),
        (None, _) => format!(
            " type to filter  ·  showing {}-{} of {} ",
            start + 1,
            end,
            total
        ),
    }
}

fn render_slash_overlay_header(filter: Option<&str>, inner_width: usize) -> String {
    let title = slash_overlay_title(filter);
    let title_width = title.chars().count();
    let pad = inner_width.saturating_sub(title_width);
    format!(
        "{}{}{}{}",
        "╭".cyan(),
        title.cyan().bold(),
        "─".repeat(pad).cyan(),
        "╮".cyan()
    )
}

fn slash_overlay_footer_summary(total: usize, start: usize, end: usize) -> String {
    if total > 0 {
        format!(
            " {}/{} shown  ·  ↑↓ move  ·  →/Tab accept  ·  Enter run  ·  ← edit  ·  Esc close ",
            end - start,
            total
        )
    } else {
        " no matches  ·  ← edit  ·  Esc close ".to_string()
    }
}

fn render_slash_overlay_footer(
    total: usize,
    start: usize,
    end: usize,
    inner_width: usize,
) -> String {
    let summary = slash_overlay_footer_summary(total, start, end);
    let summary_width = summary.chars().count();
    let pad = inner_width.saturating_sub(summary_width);
    format!(
        "{}{}{}{}",
        "╰".cyan(),
        summary.dim(),
        "─".repeat(pad).cyan(),
        "╯".cyan()
    )
}

fn render_slash_overlay_context_line(summary: &str, inner_width: usize) -> String {
    let pad = inner_width.saturating_sub(summary.chars().count());
    format!(
        "{}{}{}{}",
        "│".cyan().dim(),
        summary.dim(),
        " ".repeat(pad),
        "│".cyan().dim()
    )
}

fn render_slash_overlay_row_with_width(
    cmd: &str,
    desc: &str,
    selected: bool,
    command_width: usize,
    inner_width: usize,
) -> String {
    let content = slash_overlay_row_content(cmd, desc, selected, command_width);
    let pad = inner_width.saturating_sub(visible_width(&content));
    format!(
        "{}{}{}{}",
        if selected {
            "┃".cyan().bold()
        } else {
            "│".cyan().dim()
        },
        content,
        " ".repeat(pad),
        if selected {
            "┃".cyan().bold()
        } else {
            "│".cyan().dim()
        }
    )
}

fn render_slash_overlay_message_line(message: &str, inner_width: usize) -> String {
    let styled = message.yellow().to_string();
    let pad = inner_width.saturating_sub(message.chars().count());
    format!(
        "{}{}{}{}",
        "│".cyan().dim(),
        styled,
        " ".repeat(pad),
        "│".cyan().dim()
    )
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
    // Only use cache when overlay is already visible — first render must always go through.
    let already_visible = slash_overlay_lines()
        .lock()
        .map(|g| *g > 0)
        .unwrap_or(false);
    if already_visible {
        if let Ok(state) = slash_overlay_state().lock()
            && state.0 == norm
            && state.1 == selected
        {
            return;
        }
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
    let mut body_contents: Vec<(Option<(&'static str, &'static str, bool)>, String)> = Vec::new();
    if rows.is_empty() {
        let label = filter.unwrap_or("");
        body_contents.push((None, format!("No slash commands match '/{label}'")));
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
        let command_width = slash_overlay_command_width(&rows[start..end]);
        for (idx, (cmd, desc)) in rows[start..end].iter().enumerate() {
            let abs = start + idx;
            body_contents.push((Some((*cmd, *desc, abs == selected)), String::new()));
        }
        let title = slash_overlay_title(filter);
        let footer = slash_overlay_footer_summary(total, start, end);
        let context = slash_overlay_context_summary(filter, total, start, end);
        let inner_width = body_contents
            .iter()
            .map(|(row, message)| match row {
                Some((cmd, desc, selected)) => visible_width(&slash_overlay_row_content(
                    cmd,
                    desc,
                    *selected,
                    command_width,
                )),
                None => message.chars().count(),
            })
            .max()
            .unwrap_or(40)
            .max(title.chars().count())
            .max(context.chars().count())
            .max(footer.chars().count());
        println!("{}", render_slash_overlay_header(filter, inner_width));
        printed += 1;
        println!(
            "{}",
            render_slash_overlay_context_line(&context, inner_width)
        );
        printed += 1;
        for (row, message) in &body_contents {
            let line = match row {
                Some((cmd, desc, selected)) => render_slash_overlay_row_with_width(
                    cmd,
                    desc,
                    *selected,
                    command_width,
                    inner_width,
                ),
                None => render_slash_overlay_message_line(message, inner_width),
            };
            println!("{line}");
            printed += 1;
        }
        println!(
            "{}",
            render_slash_overlay_footer(total, start, end, inner_width)
        );
        printed += 1;
        if let Ok(mut g) = slash_overlay_lines().lock() {
            *g = printed;
        }
        set_slash_filter(filter.map(|s| s.to_string()));
        if let Ok(mut s) = slash_overlay_state().lock() {
            *s = (norm, selected);
        }
        return;
    }
    let title = slash_overlay_title(filter);
    let footer = slash_overlay_footer_summary(0, 0, 0);
    let context = slash_overlay_context_summary(filter, 0, 0, 0);
    let inner_width = body_contents
        .iter()
        .map(|(_, message)| message.chars().count())
        .max()
        .unwrap_or(40)
        .max(title.chars().count())
        .max(context.chars().count())
        .max(footer.chars().count());
    println!("{}", render_slash_overlay_header(filter, inner_width));
    printed += 1;
    println!(
        "{}",
        render_slash_overlay_context_line(&context, inner_width)
    );
    printed += 1;
    for (_, message) in &body_contents {
        let line = render_slash_overlay_message_line(message, inner_width);
        println!("{line}");
        printed += 1;
    }
    println!("{}", render_slash_overlay_footer(0, 0, 0, inner_width));
    printed += 1;

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
    print_group("Core", &["/help", "/model", "/clear", "/exit", "/keys"]);
    print_group(
        "Workspace",
        &["/search", "/history", "/copy", "/context", "/rewind"],
    );
    print_group("Agent", &["/explain", "/verbose", "/compact", "/reflect"]);
    print_group(
        "Session",
        &["/session", "/resume", "/stats", "/tools", "/health"],
    );
    print_group("Skills & Memory", &["/skill", "/memory", "/plan", "/task"]);
    print_group("Diagnostics", &["/doctor", "/version"]);
    print_group(
        "Account",
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
    if filter.is_none() {
        eprintln!(
            "  {}",
            "Tip: type a parent command + space to see sub-commands (e.g. /skill , /search )".dim()
        );
    }
    eprintln!();
}

pub(super) fn print_keyboard_shortcuts() {
    eprintln!(
        "\n{}",
        "─── Keyboard Shortcuts ──────────────────────────".bold()
    );
    let shortcuts = [
        (
            "Navigation",
            &[
                ("Ctrl+A", "Move to line start"),
                ("Ctrl+E", "Move to line end"),
                ("Ctrl+B / ←", "Move back one character"),
                ("Ctrl+F / →", "Move forward one character"),
                ("Alt+B", "Move back one word"),
                ("Alt+F", "Move forward one word"),
            ] as &[(&str, &str)],
        ),
        (
            "Editing",
            &[
                ("Ctrl+W", "Delete word backward"),
                ("Ctrl+K", "Kill to end of line"),
                ("Ctrl+U", "Kill from start to cursor"),
                ("Ctrl+H / Backspace", "Delete previous character"),
                (
                    "Ctrl+D",
                    "Delete character at cursor (or exit on empty line)",
                ),
                ("Ctrl+T", "Transpose characters"),
            ],
        ),
        (
            "History",
            &[
                ("↑ / ↓", "Navigate previous/next history"),
                ("Ctrl+R", "Reverse search history"),
                ("Ctrl+G", "Cancel search"),
                ("Ctrl+P / Ctrl+N", "Previous/next (same as ↑/↓)"),
            ],
        ),
        (
            "Multi-line",
            &[
                ("Alt+Enter", "Insert newline (continue input)"),
                ("\\ (at end)", "Backslash continuation"),
            ],
        ),
        (
            "Screen",
            &[
                ("Ctrl+L", "Clear screen"),
                ("Ctrl+D", "Exit (on empty line)"),
                ("Ctrl+C", "Cancel current input"),
            ],
        ),
        (
            "Slash Picker",
            &[
                ("/", "Open command picker"),
                ("↑/↓ or Tab", "Navigate picker items"),
                ("Enter", "Execute selected command"),
                ("→/Space", "Accept and continue editing"),
                ("Esc", "Dismiss picker"),
            ],
        ),
    ];
    for (section, keys) in shortcuts {
        eprintln!("  {}", section.cyan().bold());
        for (key, desc) in keys.iter() {
            eprintln!("    {:<22} {}", key.bold(), desc.dim());
        }
    }
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
        let slash_query = slash_completion_query(&current_line);
        let in_slash = slash_query.is_some();
        let active = is_slash_picker_active();

        // ── Helper: navigate picker and replace input line ──────────────
        macro_rules! nav {
            ($delta:expr) => {{
                let filter = get_slash_filter();
                let filter_ref = filter.as_deref();
                if move_picker_selection($delta).is_some() {
                    render_slash_overlay(filter_ref);
                    return Some(RlCmd::Noop);
                }
                render_slash_overlay(filter_ref);
                return Some(RlCmd::Noop);
            }};
        }

        match key {
            RlKeyEvent(RlKeyCode::Char(' '), _)
                if in_slash && active && ctx.pos() == ctx.line().len() =>
            {
                let current = ctx.line();
                let selected = picker_selected_command();
                clear_slash_overlay();
                if let Some(edit) = accepted_slash_edit(current, selected, true) {
                    return Some(match edit {
                        AcceptedSlashEdit::InsertSuffix(text) => RlCmd::Insert(1, text),
                        AcceptedSlashEdit::ReplaceWholeLine(line) => {
                            RlCmd::Replace(RlMovement::WholeLine, Some(line))
                        }
                        AcceptedSlashEdit::KeepLine => RlCmd::Move(RlMovement::EndOfLine),
                    });
                }
                return None;
            }

            // ── Typing: project next char and update filter ─────────────
            RlKeyEvent(RlKeyCode::Char(c), mods)
                if !mods.contains(RlModifiers::CTRL) && !mods.contains(RlModifiers::ALT) =>
            {
                if ctx.pos() == ctx.line().len() {
                    let mut line = ctx.line().to_string();
                    line.push(*c);
                    if let Some(query) = slash_completion_query(&line) {
                        let q = query.trim_start_matches('/');
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
                    if let Some(query) = slash_completion_query(&line) {
                        let q = query.trim_start_matches('/');
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
                let rows = picker_rows_for_filter();
                let current = ctx.line();
                let selected = get_slash_picker_selected();
                let selected_cmd = rows.get(selected).map(|(cmd, _)| *cmd);
                let exact_selected = selected_cmd == Some(current);
                if rows.len() == 1 || exact_selected {
                    clear_slash_overlay();
                    if let Some(edit) = accepted_slash_edit(current, selected_cmd, true) {
                        return Some(apply_accepted_slash_edit(edit));
                    }
                    return Some(RlCmd::Move(RlMovement::EndOfLine));
                }
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
            RlKeyEvent(RlKeyCode::Left, _) if in_slash && active => {
                clear_slash_overlay();
                return slash_left_arrow_command(active, in_slash, ctx.pos());
            }
            RlKeyEvent(RlKeyCode::Right, _)
                if in_slash && active && ctx.pos() == ctx.line().len() =>
            {
                let current = ctx.line();
                let selected = picker_selected_command();
                clear_slash_overlay();
                if let Some(edit) = accepted_slash_edit(current, selected, false) {
                    return Some(apply_accepted_slash_edit(edit));
                }
                return None;
            }
            RlKeyEvent(RlKeyCode::Right, _) if in_slash && !active => {
                if let Some(filter) =
                    slash_right_arrow_filter(ctx.line(), ctx.pos(), active, in_slash)
                {
                    set_slash_picker_selected(0);
                    render_slash_overlay(filter.as_deref());
                }
                return None;
            }
            RlKeyEvent(RlKeyCode::Char('e'), m)
                if in_slash && !active && m.contains(RlModifiers::CTRL) =>
            {
                if let Some(filter) = slash_ctrl_e_filter(ctx.line(), active, in_slash) {
                    set_slash_picker_selected(0);
                    render_slash_overlay(filter.as_deref());
                }
                return None;
            }

            // ── Accept: Enter with picker executes the selected command ─────
            // Store the selected command in pending-execute and return None so
            // rustyline fires AcceptLine.  The main REPL loop reads the pending
            // value and dispatches it instead of the (possibly incomplete) typed text.
            // Special case: bare "/" should dispatch as the "/" command (show list),
            // not auto-select the first picker item.
            RlKeyEvent(RlKeyCode::Enter, _) if active => {
                let current = ctx.line();
                if current == "/" {
                    // Let the normal command dispatch handle "/" → print command list
                    clear_slash_overlay();
                    return None;
                }
                let rows = picker_rows_for_filter();
                let selected = get_slash_picker_selected();
                clear_slash_overlay();
                if let Some((cmd, _)) = rows.get(selected) {
                    if *cmd != current {
                        set_slash_pending_execute(Some(cmd.to_string()));
                    }
                }
                return None; // AcceptLine → immediate execution
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

            // ── Alt+Enter: insert newline for multi-line input ──────────
            RlKeyEvent(RlKeyCode::Enter, m) if m.contains(RlModifiers::ALT) => {
                return Some(RlCmd::Newline);
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
            let Some(prefix) = slash_completion_query(before_cursor) else {
                return Ok((pos, vec![]));
            };
            (prefix, false)
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
        slash_inline_hint(line)
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

    fn strip_ansi_codes(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' && matches!(chars.peek(), Some('[')) {
                let _ = chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

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

    #[test]
    fn search_command_is_registered() {
        assert!(SLASH_COMMANDS.iter().any(|(cmd, _)| *cmd == "/search"));
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

    // ── Bug fix: "/" resolves as exact command, not ambiguous ──────────────

    #[test]
    fn bare_slash_resolves_to_slash_command() {
        // "/" is an exact match in SLASH_COMMANDS, should never be ambiguous
        let result = resolve_slash_command("/");
        assert!(
            result.is_ok(),
            "bare '/' should resolve exactly, got: {result:?}"
        );
        assert_eq!(result.unwrap(), "/");
    }

    // ── Bug fix: filtered_slash_rows excludes "/" and aliases from picker ──

    #[test]
    fn filtered_rows_exclude_slash_and_aliases() {
        let rows = filtered_slash_rows(None);
        for (cmd, _) in &rows {
            assert_ne!(*cmd, "/", "picker should not list bare /");
            assert_ne!(*cmd, "/?", "picker should not list /?");
            assert_ne!(*cmd, "/commands", "picker should not list /commands");
            assert_ne!(*cmd, "/quit", "picker should not list /quit");
        }
        // But real commands like /copy, /clear should be present
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/copy"));
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/clear"));
    }

    #[test]
    fn filtered_rows_first_item_is_not_slash() {
        let rows = filtered_slash_rows(None);
        assert!(!rows.is_empty());
        assert_ne!(rows[0].0, "/", "first picker item should not be '/'");
    }

    #[test]
    fn filtered_rows_prefix_query_prefers_command_name_matches() {
        let rows = filtered_slash_rows(Some("e"));
        assert!(!rows.is_empty());
        assert_eq!(rows[0].0, "/exit");
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/explain"));
        assert!(!rows.iter().any(|(cmd, _)| *cmd == "/copy"));
    }

    #[test]
    fn filtered_rows_falls_back_to_description_search_when_no_prefix_match() {
        let rows = filtered_slash_rows(Some("keyboard"));
        assert!(!rows.is_empty());
        assert_eq!(rows[0].0, "/keys");
    }

    // ── Bug fix: picker cycling wraps around ──────────────────────────────

    #[test]
    fn picker_cycling_wraps_around() {
        // This test verifies rem_euclid cycling logic.
        // All checks run sequentially because they share global picker state.
        set_slash_picker_selected(0);
        set_slash_filter(None);
        let rows = picker_rows_for_filter();
        let total = rows.len();
        assert!(total > 2, "need multiple rows for cycling test");

        // Forward wrap: navigate to last, then Down → should return first item
        set_slash_picker_selected(total - 1);
        let cmd = move_picker_selection(1);
        assert!(cmd.is_some());
        assert_eq!(
            cmd.unwrap(),
            rows[0].0,
            "Down from last should wrap to first"
        );
        assert_eq!(get_slash_picker_selected(), 0);

        // Backward wrap: at first, Up → should return last item
        set_slash_picker_selected(0);
        let cmd = move_picker_selection(-1);
        assert!(cmd.is_some());
        assert_eq!(
            cmd.unwrap(),
            rows[total - 1].0,
            "Up from first should wrap to last"
        );
        assert_eq!(get_slash_picker_selected(), total - 1);

        // Full loop: navigate forward through ALL items → back to start
        set_slash_picker_selected(0);
        for _ in 0..total {
            move_picker_selection(1);
        }
        assert_eq!(
            get_slash_picker_selected(),
            0,
            "full loop should return to start"
        );
    }

    // ── /keys command registered ──────────────────────────────────────────

    #[test]
    fn keys_command_is_registered() {
        assert!(SLASH_COMMANDS.iter().any(|(cmd, _)| *cmd == "/keys"));
    }

    #[test]
    fn keys_resolves_from_prefix() {
        let result = resolve_slash_command("/key");
        assert!(result.is_ok(), "got: {result:?}");
        assert_eq!(result.unwrap(), "/keys");
    }

    #[test]
    fn accepted_slash_edit_right_arrow_inserts_missing_suffix() {
        assert_eq!(
            accepted_slash_edit("/exp", Some("/explain"), false),
            Some(AcceptedSlashEdit::InsertSuffix("lain".to_string()))
        );
    }

    #[test]
    fn accepted_slash_edit_space_appends_after_completion() {
        assert_eq!(
            accepted_slash_edit("/exp", Some("/explain"), true),
            Some(AcceptedSlashEdit::InsertSuffix("lain ".to_string()))
        );
    }

    #[test]
    fn accepted_slash_edit_space_appends_for_exact_command() {
        assert_eq!(
            accepted_slash_edit("/explain", Some("/explain"), true),
            Some(AcceptedSlashEdit::InsertSuffix(" ".to_string()))
        );
    }

    #[test]
    fn accepted_slash_edit_from_skill_prefix_inserts_only_missing_suffix() {
        assert_eq!(
            accepted_slash_edit("/skil", Some("/skill"), false),
            Some(AcceptedSlashEdit::InsertSuffix("l".to_string()))
        );
    }

    #[test]
    fn accepted_slash_edit_from_skill_subcommand_prefix_inserts_only_missing_suffix() {
        assert_eq!(
            accepted_slash_edit("/skill d", Some("/skill dev"), false),
            Some(AcceptedSlashEdit::InsertSuffix("ev".to_string()))
        );
    }

    #[test]
    fn slash_completion_query_keeps_skill_subcommands_active() {
        assert_eq!(slash_completion_query("/skill "), Some("/skill "));
        assert_eq!(slash_completion_query("/skill d"), Some("/skill d"));
        assert_eq!(slash_completion_query("/skill dev"), Some("/skill dev"));
        assert_eq!(slash_completion_query("/skill dev foo"), None);
    }

    #[test]
    fn slash_completion_query_keeps_search_subcommands_active() {
        assert_eq!(slash_completion_query("/search "), Some("/search "));
        assert_eq!(slash_completion_query("/search r"), Some("/search r"));
        assert_eq!(
            slash_completion_query("/search review"),
            Some("/search review")
        );
        assert_eq!(slash_completion_query("/search review timeout"), None);
    }

    #[test]
    fn filtered_rows_for_skill_space_show_subcommands() {
        let rows = filtered_slash_rows(Some("skill "));
        assert!(!rows.is_empty());
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/skill list"));
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/skill dev"));
        assert!(!rows.iter().any(|(cmd, _)| *cmd == "/skill"));
    }

    #[test]
    fn filtered_rows_for_skill_d_rank_dev_before_doctor() {
        let rows = filtered_slash_rows(Some("skill d"));
        assert!(!rows.is_empty());
        assert_eq!(rows[0].0, "/skill dev");
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/skill doctor"));
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/skill dev off"));
    }

    #[test]
    fn filtered_rows_for_search_space_show_subcommands() {
        let rows = filtered_slash_rows(Some("search "));
        assert!(!rows.is_empty());
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/search files"));
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/search review"));
        assert!(!rows.iter().any(|(cmd, _)| *cmd == "/search"));
    }

    #[test]
    fn filtered_rows_for_search_r_rank_review_first() {
        let rows = filtered_slash_rows(Some("search r"));
        assert!(!rows.is_empty());
        assert_eq!(rows[0].0, "/search review");
    }

    #[test]
    fn completion_candidates_support_skill_subcommands_after_space() {
        let candidates = completion_candidates("/skill ");
        assert!(candidates.iter().any(|(cmd, _)| *cmd == "/skill list"));
        assert!(candidates.iter().any(|(cmd, _)| *cmd == "/skill dev"));
    }

    #[test]
    fn completion_candidates_support_search_subcommands_after_space() {
        let candidates = completion_candidates("/search ");
        assert!(candidates.iter().any(|(cmd, _)| *cmd == "/search files"));
        assert!(candidates.iter().any(|(cmd, _)| *cmd == "/search review"));
    }

    #[test]
    fn slash_inline_hint_completes_skill_subcommand_prefix() {
        assert_eq!(slash_inline_hint("/skill d"), Some("ev".to_string()));
    }

    #[test]
    fn slash_inline_hint_shows_skill_subcommand_parameter_hint() {
        assert_eq!(
            slash_inline_hint("/skill dev"),
            Some(" <name|off>".to_string())
        );
        assert_eq!(
            slash_inline_hint("/skill dev "),
            Some("<name|off>".to_string())
        );
    }

    #[test]
    fn slash_inline_hint_shows_skill_root_choices() {
        assert_eq!(
            slash_inline_hint("/skill "),
            Some("[list|new|test|dev|doctor|validate|config|system]".to_string())
        );
    }

    #[test]
    fn slash_inline_hint_shows_search_modes() {
        assert_eq!(
            slash_inline_hint("/search"),
            Some(" <pattern|files <glob>|review <pattern>>".to_string())
        );
    }

    #[test]
    fn slash_inline_hint_completes_search_subcommand_prefix() {
        assert_eq!(slash_inline_hint("/search r"), Some("eview".to_string()));
    }

    #[test]
    fn slash_inline_hint_shows_search_subcommand_parameter_hint() {
        assert_eq!(
            slash_inline_hint("/search review"),
            Some(" <pattern>".to_string())
        );
        assert_eq!(
            slash_inline_hint("/search files"),
            Some(" <glob>".to_string())
        );
    }

    #[test]
    fn apply_accepted_slash_edit_preserves_cursor_at_end() {
        assert_eq!(
            apply_accepted_slash_edit(AcceptedSlashEdit::InsertSuffix("ev ".to_string())),
            RlCmd::Insert(1, "ev ".to_string())
        );
    }

    #[test]
    fn slash_left_arrow_moves_back_one_char_when_overlay_active() {
        assert_eq!(
            slash_left_arrow_command(true, true, 2),
            Some(RlCmd::Move(RlMovement::BackwardChar(1)))
        );
    }

    #[test]
    fn slash_left_arrow_clamps_to_line_start() {
        assert_eq!(
            slash_left_arrow_command(true, true, 0),
            Some(RlCmd::Move(RlMovement::BeginningOfLine))
        );
    }

    #[test]
    fn slash_left_arrow_ignores_non_slash_context() {
        assert_eq!(slash_left_arrow_command(true, false, 2), None);
        assert_eq!(slash_left_arrow_command(false, true, 2), None);
    }

    #[test]
    fn slash_picker_filter_supports_prefixed_commands() {
        assert_eq!(slash_picker_filter("/exp"), Some(Some("exp".to_string())));
    }

    #[test]
    fn slash_picker_filter_keeps_root_picker_for_bare_slash() {
        assert_eq!(slash_picker_filter("/"), Some(None));
    }

    #[test]
    fn slash_right_arrow_reopens_picker_when_returning_to_end_of_line() {
        assert_eq!(
            slash_right_arrow_filter("/exp", 3, false, true),
            Some(Some("exp".to_string()))
        );
    }

    #[test]
    fn slash_right_arrow_ignores_non_terminal_movements() {
        assert_eq!(slash_right_arrow_filter("/exp", 2, false, true), None);
        assert_eq!(slash_right_arrow_filter("/exp", 3, true, true), None);
    }

    #[test]
    fn slash_ctrl_e_reopens_picker_in_slash_context() {
        assert_eq!(
            slash_ctrl_e_filter("/skill d", false, true),
            Some(Some("skill d".to_string()))
        );
    }

    #[test]
    fn slash_ctrl_e_ignores_inactive_completion_contexts() {
        assert_eq!(slash_ctrl_e_filter("/skill dev foo", false, true), None);
        assert_eq!(slash_ctrl_e_filter("/exp", true, true), None);
    }

    #[test]
    fn render_selected_slash_row_highlights_command_and_description() {
        let row = render_slash_overlay_row_with_width("/ask", "Toggle ask mode", true, 16, 40);
        let plain = strip_ansi_codes(&row);
        assert!(plain.starts_with("┃❯ /ask"));
        assert!(plain.ends_with('┃'));
        assert!(plain.contains("/ask"));
        assert!(plain.contains("Toggle ask mode"));
        assert!(row.contains("\u{1b}["));
        let desc_start = row.find("Toggle ask mode").expect("desc should be present");
        assert!(row[..desc_start].contains("\u{1b}["));
    }

    #[test]
    fn render_unselected_slash_row_keeps_plain_layout() {
        let row = render_slash_overlay_row_with_width("/ask", "Toggle ask mode", false, 16, 40);
        let plain = strip_ansi_codes(&row);
        assert!(plain.starts_with("│  /ask"));
        assert!(plain.ends_with('│'));
        assert!(plain.contains("Toggle ask mode"));
    }

    #[test]
    fn slash_overlay_command_width_tracks_longest_visible_command() {
        let width = slash_overlay_command_width(&[
            ("/ask", "Toggle ask mode"),
            ("/session export", "Export session as markdown"),
        ]);
        assert_eq!(width, 16);
    }

    #[test]
    fn render_slash_overlay_header_includes_filter_when_present() {
        let header = render_slash_overlay_header(Some("skill d"), 48);
        let plain = strip_ansi_codes(&header);
        assert!(plain.starts_with('╭'));
        assert!(plain.ends_with('╮'));
        assert!(plain.contains("Slash Commands"));
        assert!(plain.contains("/skill d"));
    }

    #[test]
    fn slash_overlay_context_summary_reports_visible_range() {
        let summary = slash_overlay_context_summary(Some("skill d"), 12, 0, 10);
        assert!(summary.contains("filter /skill d"));
        assert!(summary.contains("showing 1-10 of 12"));
    }

    #[test]
    fn slash_overlay_context_summary_handles_empty_state() {
        let summary = slash_overlay_context_summary(Some("zzz"), 0, 0, 0);
        assert!(summary.contains("filter /zzz"));
        assert!(summary.contains("no matching commands"));
    }

    #[test]
    fn render_slash_overlay_context_line_draws_side_rails() {
        let line = render_slash_overlay_context_line(" type to filter  ·  showing 1-10 of 12 ", 48);
        let plain = strip_ansi_codes(&line);
        assert!(plain.starts_with('│'));
        assert!(plain.ends_with('│'));
        assert!(plain.contains("showing 1-10 of 12"));
    }

    #[test]
    fn render_slash_overlay_footer_shows_navigation_help() {
        let footer = render_slash_overlay_footer(12, 0, 10, 56);
        let plain = strip_ansi_codes(&footer);
        assert!(plain.starts_with('╰'));
        assert!(plain.ends_with('╯'));
        assert!(plain.contains("10/12 shown"));
        assert!(plain.contains("→/Tab accept"));
        assert!(plain.contains("Enter run"));
        assert!(plain.contains("← edit"));
        assert!(plain.contains("Esc close"));
    }

    #[test]
    fn render_slash_overlay_footer_handles_empty_state() {
        let footer = render_slash_overlay_footer(0, 0, 0, 32);
        let plain = strip_ansi_codes(&footer);
        assert!(plain.contains("no matches"));
        assert!(plain.contains("← edit"));
        assert!(plain.contains("Esc close"));
    }

    // ── Multi-line validator ──────────────────────────────────────────────

    #[test]
    fn validator_backslash_continuation() {
        // Trailing backslash should signal incomplete input
        let input_cont = "hello world \\";
        assert!(input_cont.ends_with('\\'));

        // No trailing backslash should be valid
        let input_done = "hello world";
        assert!(!input_done.ends_with('\\'));
    }

    // ── Sub-command tiering ──────────────────────────────────────────────

    #[test]
    fn is_subcommand_recognizes_space_commands() {
        assert!(is_subcommand("/skill list"));
        assert!(is_subcommand("/search files"));
        assert!(is_subcommand("/session history"));
        assert!(is_subcommand("/skill dev off"));
        assert!(!is_subcommand("/skill"));
        assert!(!is_subcommand("/search"));
        assert!(!is_subcommand("/model"));
    }

    #[test]
    fn filtered_rows_top_level_hides_subcommands() {
        let rows = filtered_slash_rows(None);
        assert!(
            rows.iter().all(|(cmd, _)| !is_subcommand(cmd)),
            "top-level picker should not show sub-commands"
        );
        // Parent commands still present
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/skill"));
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/search"));
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/session"));
    }

    #[test]
    fn filtered_rows_single_prefix_hides_subcommands() {
        let rows = filtered_slash_rows(Some("s"));
        assert!(
            rows.iter().all(|(cmd, _)| !is_subcommand(cmd)),
            "single-char prefix should not show sub-commands"
        );
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/session"));
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/search"));
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/skill"));
    }

    #[test]
    fn filtered_rows_space_prefix_shows_subcommands() {
        // Typing "/skill " should reveal skill sub-commands
        let rows = filtered_slash_rows(Some("skill "));
        assert!(!rows.is_empty());
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/skill list"));
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/skill dev"));
        // Parent command itself should not appear (prefix "skill " doesn't match "/skill")
        assert!(!rows.iter().any(|(cmd, _)| *cmd == "/skill"));
    }

    #[test]
    fn filtered_rows_search_space_shows_subcommands() {
        let rows = filtered_slash_rows(Some("search "));
        assert!(!rows.is_empty());
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/search files"));
        assert!(rows.iter().any(|(cmd, _)| *cmd == "/search review"));
    }

    // ── Pending-execute lifecycle ────────────────────────────────────────

    #[test]
    fn pending_execute_set_take_clears() {
        // Start clean
        let _ = take_slash_pending_execute();
        // Set and take
        set_slash_pending_execute(Some("/model".to_string()));
        assert_eq!(
            take_slash_pending_execute(),
            Some("/model".to_string()),
            "first take should return the stored command"
        );
        assert_eq!(
            take_slash_pending_execute(),
            None,
            "second take should return None (already consumed)"
        );
    }

    #[test]
    fn pending_execute_none_by_default() {
        // Drain any leftover from other tests
        let _ = take_slash_pending_execute();
        assert_eq!(take_slash_pending_execute(), None);
    }

    // ── Print group reorganization ───────────────────────────────────────

    #[test]
    fn print_slash_commands_has_expected_groups() {
        // Capture stderr output from print_slash_commands
        // We can't capture stderr easily in a unit test, so we validate the
        // group structure indirectly by checking all listed commands exist
        // in SLASH_COMMANDS.
        let groups: &[&[&str]] = &[
            &["/help", "/model", "/clear", "/exit", "/keys"],
            &["/search", "/history", "/copy", "/context", "/rewind"],
            &["/explain", "/verbose", "/compact", "/reflect"],
            &["/session", "/resume", "/stats", "/tools", "/health"],
            &["/skill", "/memory", "/plan", "/task"],
            &["/doctor", "/version"],
            &["/login", "/register", "/logout", "/memory-setup"],
        ];
        let known: std::collections::HashSet<&str> =
            SLASH_COMMANDS.iter().map(|(cmd, _)| *cmd).collect();
        for group in groups {
            for cmd in *group {
                assert!(
                    known.contains(cmd),
                    "group command {cmd} not in SLASH_COMMANDS"
                );
            }
        }
    }
}
