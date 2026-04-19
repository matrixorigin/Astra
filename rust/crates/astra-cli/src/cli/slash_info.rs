use super::*;

// ── Context status thresholds ────────────────────────────────────────────────
// Used for color-coding pressure indicators in /info and context displays.
/// Pressure below this is "healthy" (green).
const PRESSURE_HEALTHY_THRESHOLD: f64 = 0.5;
/// Pressure below this (but above healthy) is "getting full" (yellow).
const PRESSURE_WARNING_THRESHOLD: f64 = 0.85;
/// Usage percentage below this is "healthy" (green).
const USAGE_HEALTHY_PCT: f64 = 60.0;
/// Usage percentage below this (but above healthy) is "getting full" (yellow).
const USAGE_WARNING_PCT: f64 = 85.0;

fn format_bytes(bytes: u32) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
    }
}

fn copy_to_clipboard(text: &str) -> bool {
    // Try Wayland first (if WAYLAND_DISPLAY is set), then X11 tools, then macOS.
    let wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
    let mut candidates: Vec<(&str, &[&str])> = Vec::new();
    if wayland {
        candidates.push(("wl-copy", &[]));
    }
    candidates.extend_from_slice(&[
        ("xclip", &["-selection", "clipboard"] as &[&str]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
    ]);
    for (cmd, args) in &candidates {
        if let Ok(mut child) = SysCommand::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

#[derive(Debug, PartialEq, Eq)]
enum GrepRequest {
    Content(String),
    Files(String),
    Review(String),
}

#[derive(Debug, PartialEq, Eq)]
struct ReviewMatch<'a> {
    path: &'a str,
    line: &'a str,
    text: &'a str,
}

fn parse_grep_request(arg: &str) -> Result<GrepRequest, &'static str> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return Err("Usage: /grep <pattern> | /grep files <glob> | /grep review <pattern>");
    }
    if let Some(rest) = trimmed.strip_prefix("files ").map(str::trim) {
        if rest.is_empty() {
            return Err("Usage: /grep files <glob>");
        }
        return Ok(GrepRequest::Files(rest.to_string()));
    }
    if let Some(rest) = trimmed.strip_prefix("review ").map(str::trim) {
        if rest.is_empty() {
            return Err("Usage: /grep review <pattern>");
        }
        return Ok(GrepRequest::Review(rest.to_string()));
    }
    Ok(GrepRequest::Content(trimmed.to_string()))
}

fn collect_changed_files(staged: &str, unstaged: &str, untracked: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    staged
        .lines()
        .chain(unstaged.lines())
        .chain(untracked.lines())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let normalized = line.to_string();
            if seen.insert(normalized.clone()) {
                Some(normalized)
            } else {
                None
            }
        })
        .collect()
}

fn run_git_lines(project_root: &std::path::Path, args: &[&str]) -> Vec<String> {
    match SysCommand::new("git")
        .args(args)
        .current_dir(project_root)
        .output()
    {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn run_git_stdout(project_root: &std::path::Path, args: &[&str]) -> String {
    match SysCommand::new("git")
        .args(args)
        .current_dir(project_root)
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
        }
        _ => String::new(),
    }
}

fn lsp_backend_label(name: &str) -> &str {
    match name {
        "rust" => "Rust",
        "typescript" => "TypeScript",
        _ => name,
    }
}

fn lsp_session_state_summary(session_state: &str) -> String {
    match session_state {
        "running" => format!("{} {}", theme::icon_ok(), "running".green()),
        "idle" => format!("{} {}", "○".cyan(), "ready".cyan()),
        "workspace_not_detected" => format!("{} {}", "·".dim(), "no workspace".dim()),
        "disabled" => format!("{} {}", "·".dim(), "disabled".dim()),
        "command_missing" => format!("{} {}", "✗".yellow(), "command missing".yellow()),
        "config_error" => format!("{} {}", "✗".red(), "config error".red()),
        "error" => format!("{} {}", "✗".red(), "startup error".red()),
        other => format!("{} {other}", "?".yellow()),
    }
}

fn print_lsp_status_report(parsed: &serde_json::Value) {
    eprintln!(
        "\n{}",
        "─── LSP Status ───────────────────────────────────────────────"
            .bold()
            .cyan()
    );

    let active_backends = parsed
        .get("active_backends")
        .and_then(serde_json::Value::as_object);
    let mut printed_any = false;
    if let Some(backends) = active_backends {
        for key in ["rust", "typescript"] {
            let Some(status) = backends.get(key) else {
                continue;
            };
            printed_any = true;
            let enabled = status
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or_else(|| {
                    status["passive_diagnostics_enabled"]
                        .as_bool()
                        .unwrap_or(false)
                });
            let workspace = status["workspace_detected"].as_bool().unwrap_or(false);
            let command = status["command"].as_str().unwrap_or("?");
            let command_available = status["command_available"].as_bool().unwrap_or(false);
            let session_started = status["session_started"].as_bool().unwrap_or(false);
            let session_state = status["session_state"].as_str().unwrap_or("unknown");
            let enabled_source = status["enabled_source"].as_str().unwrap_or("default");
            let command_source = status["command_source"].as_str().unwrap_or("default");

            eprintln!(
                "  {} {}",
                lsp_backend_label(key).bold(),
                lsp_session_state_summary(session_state)
            );
            eprintln!(
                "     enabled: {}   workspace: {}   session started: {}",
                if enabled { "yes".green() } else { "no".dim() },
                if workspace { "yes".green() } else { "no".dim() },
                if session_started {
                    "yes".green()
                } else {
                    "no".dim()
                }
            );
            if command_available {
                eprintln!("     command: {}", command);
            } else {
                eprintln!(
                    "     command: {}{}",
                    command,
                    " (not found on PATH)".yellow()
                );
            }
            if let Some(config_file) = status["config_file"].as_str() {
                eprintln!(
                    "     config: {}   enabled via: {}   command via: {}",
                    config_file,
                    enabled_source.dim(),
                    command_source.dim()
                );
            } else {
                eprintln!(
                    "     config: {}   enabled via: {}   command via: {}",
                    "(none)".dim(),
                    enabled_source.dim(),
                    command_source.dim()
                );
            }
            if let Some(error) = status["config_error"].as_str() {
                eprintln!("     config error: {}", truncate_str(error, 140).yellow());
            }
            if let Some(error) = status["last_start_error"].as_str() {
                eprintln!("     last error: {}", truncate_str(error, 140).yellow());
            }
            eprintln!();
        }
    }

    if !printed_any {
        eprintln!("  {}", "No LSP backend status available.".dim());
        eprintln!();
    }

    if let Some(active) = parsed
        .get("supported_languages")
        .and_then(|v| v.get("active_lsp"))
        .and_then(serde_json::Value::as_array)
    {
        let langs: Vec<&str> = active
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        if !langs.is_empty() {
            eprintln!("  Active LSP language ids: {}", langs.join(", ").dim());
        }
    }
    if let Some(note) = parsed.get("note").and_then(serde_json::Value::as_str) {
        eprintln!("  {}", note.dim());
    }
    eprintln!();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReviewGitTarget<'a> {
    Head,
    WorkingTree,
    Rev(&'a str),
}

fn parse_review_git_target(arg: &str) -> ReviewGitTarget<'_> {
    let a = arg.trim();
    if a.is_empty() {
        return ReviewGitTarget::Head;
    }
    match a.to_ascii_lowercase().as_str() {
        "latest" | "latest commit" | "last" | "last commit" | "head" | "head commit" | "tip"
        | "current commit" => ReviewGitTarget::Head,
        "working" | "working tree" | "worktree" | "working-tree" | "local" | "local changes"
        | "dirty" | "wt" => ReviewGitTarget::WorkingTree,
        _ => ReviewGitTarget::Rev(a),
    }
}

const REVIEW_PREFETCH_MAX: usize = 12_000;

fn prefetch_review_git_stat(project_root: &std::path::Path, target: ReviewGitTarget<'_>) -> String {
    let mut out = String::new();
    match target {
        ReviewGitTarget::Head => {
            let header = run_git_stdout(
                project_root,
                &["show", "--no-patch", "--format=Commit %H%n%s", "HEAD"],
            );
            if !header.trim().is_empty() {
                out.push_str(header.trim_end());
                out.push('\n');
            }
            let stat = run_git_stdout(project_root, &["show", "--stat", "HEAD"]);
            if !stat.trim().is_empty() {
                out.push_str(stat.trim_end());
                out.push('\n');
            }
        }
        ReviewGitTarget::WorkingTree => {
            let staged = run_git_stdout(project_root, &["diff", "--cached", "--stat"]);
            let unstaged = run_git_stdout(project_root, &["diff", "--stat"]);
            if !staged.trim().is_empty() {
                out.push_str("Staged:\n");
                out.push_str(&staged);
                if !staged.ends_with('\n') {
                    out.push('\n');
                }
            }
            if !unstaged.trim().is_empty() {
                out.push_str("Unstaged:\n");
                out.push_str(&unstaged);
                if !unstaged.ends_with('\n') {
                    out.push('\n');
                }
            }
            if out.trim().is_empty() {
                let untracked = run_git_lines(
                    project_root,
                    &["ls-files", "--others", "--exclude-standard"],
                );
                if !untracked.is_empty() {
                    out.push_str("Untracked files (no line counts):\n");
                    for u in untracked.iter().take(40) {
                        out.push_str(u);
                        out.push('\n');
                    }
                    if untracked.len() > 40 {
                        out.push_str(&format!("... +{} more\n", untracked.len() - 40));
                    }
                }
            }
        }
        ReviewGitTarget::Rev(rev) => {
            let stat = run_git_stdout(project_root, &["show", "--stat", rev]);
            if !stat.trim().is_empty() {
                out.push_str(stat.trim_end());
                out.push('\n');
            }
        }
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        return "(no diff stat available — repo may be clean or not a git checkout)\n".to_string();
    }
    let mut s = trimmed.to_string();
    if s.len() > REVIEW_PREFETCH_MAX {
        s.truncate(s.floor_char_boundary(REVIEW_PREFETCH_MAX));
        s.push_str("\n[truncated]");
    }
    s
}

fn fence_prefetch_block(raw: &str) -> String {
    raw.replace("```", "'''")
}

fn parse_review_match(line: &str) -> Option<ReviewMatch<'_>> {
    let mut parts = line.splitn(3, ':');
    let path = parts.next()?.trim();
    let line = parts.next()?.trim();
    let text = parts.next()?.trim_end();
    if path.is_empty() || line.is_empty() {
        return None;
    }
    Some(ReviewMatch { path, line, text })
}

fn summarize_file_list(files: &[String], limit: usize) -> String {
    let shown: Vec<&str> = files.iter().take(limit).map(String::as_str).collect();
    let mut summary = shown.join(", ");
    if files.len() > limit {
        if !summary.is_empty() {
            summary.push_str(", ");
        }
        summary.push_str(&format!("+{} more", files.len() - limit));
    }
    summary
}

fn format_review_search_result(files: &[String], raw: &str) -> String {
    if raw.trim().is_empty() {
        return format!(
            "Scope: {} changed files\nFiles: {}\n\nNo matches found in changed files\nTip: use /grep <pattern> for a workspace-wide scan.",
            files.len(),
            summarize_file_list(files, 6)
        );
    }

    let parsed: Vec<ReviewMatch<'_>> = raw.lines().filter_map(parse_review_match).collect();
    if parsed.is_empty() {
        return raw.trim().to_string();
    }

    let mut out = String::new();
    let matched_files: HashSet<&str> = parsed.iter().map(|m| m.path).collect();
    out.push_str(&format!(
        "Scope: {} changed files\nFiles: {}\n\nMatches: {} hit(s) across {} file(s)\n",
        files.len(),
        summarize_file_list(files, 6),
        parsed.len(),
        matched_files.len()
    ));

    let mut current_path: Option<&str> = None;
    for m in parsed {
        if current_path != Some(m.path) {
            if current_path.is_some() {
                out.push('\n');
            }
            out.push_str(&format!("\n{}\n", m.path));
            current_path = Some(m.path);
        }
        out.push_str(&format!("  {}: {}\n", m.line, m.text));
    }

    if out.len() > 20_000 {
        out.truncate(out.floor_char_boundary(20_000));
        out.push_str("\n[truncated]");
    }
    out
}

fn review_search(executor: &edge_tools::ToolExecutor, pattern: &str) -> String {
    let staged = run_git_lines(&executor.project_root, &["diff", "--name-only", "--cached"]);
    let unstaged = run_git_lines(&executor.project_root, &["diff", "--name-only"]);
    let untracked = run_git_lines(
        &executor.project_root,
        &["ls-files", "--others", "--exclude-standard"],
    );
    let files = collect_changed_files(
        &staged.join("\n"),
        &unstaged.join("\n"),
        &untracked.join("\n"),
    );
    if files.is_empty() {
        return "No changed files found. Use /grep <pattern> for workspace-wide search."
            .to_string();
    }

    let mut cmd = SysCommand::new("grep");
    cmd.arg("-n");
    cmd.arg("-i");
    cmd.arg("--binary-files=without-match");
    cmd.arg("--");
    cmd.arg(pattern);
    for file in &files {
        cmd.arg(file);
    }
    cmd.current_dir(&executor.project_root);

    match cmd.output() {
        Ok(output) => match output.status.code() {
            Some(0) => {
                let text = String::from_utf8_lossy(&output.stdout);
                format_review_search_result(&files, &text)
            }
            Some(1) => format_review_search_result(&files, ""),
            _ => {
                let err = String::from_utf8_lossy(&output.stderr);
                let detail = err.trim();
                if detail.is_empty() {
                    "Error: review search failed".to_string()
                } else {
                    format!("Error: {detail}")
                }
            }
        },
        Err(e) => format!("Error: {e}"),
    }
}

fn build_review_prompt(arg: &str, project_root: &std::path::Path) -> String {
    let git_target = parse_review_git_target(arg);
    let target_line = match git_target {
        ReviewGitTarget::Head => "HEAD".to_string(),
        ReviewGitTarget::WorkingTree => "WORKING_TREE".to_string(),
        ReviewGitTarget::Rev(r) => r.to_string(),
    };
    let prefetched = prefetch_review_git_stat(project_root, git_target);
    let fenced = fence_prefetch_block(&prefetched);
    format!(
        "You are an expert code reviewer working in the current local git repository.\n\
\n\
Review target: {target_line}\n\
\n\
Pre-fetched `git` summary (authoritative; do not repeat it; never reformat these lines as markdown pipe tables — they break the terminal):\n\
```text\n\
{fenced}\n\
```\n\
\n\
Process:\n\
1. Get the diff:\n\
   - HEAD -> `git_show` (gives you the full diff already)\n\
   - WORKING_TREE -> `git_diff` (use `stat_only:true` if you only need per-file +/- counts)\n\
   - Other -> `git_show <rev>`\n\
2. Review the diff directly. Do NOT read entire files.\n\
   Only use `read_file` with `start_line`/`end_line` if you need \
   ~10 lines of surrounding context to verify a specific finding.\n\
3. If you need to understand a function signature or type, use \
   `read_file` with `outline=true` instead of reading the whole file.\n\
4. Prefer `read_file`/`grep`/`glob` over `bash` unless a shell command is truly necessary.\n\
5. Ignore pure formatting churn and environment-only failures unrelated to the reviewed change.\n\
6. Do not narrate your process, do not repeat the diff or the pre-fetched stat block, and do not output XML-like tags such as `<reflect>`.\n\
7. In your answer, avoid markdown tables and lines dominated by `|` characters.\n\
\n\
Output format:\n\
- Findings: 0-3 bullets, only material issues.\n\
- Verdict: `LGTM` or `Needs changes`, with one short sentence.\n\
- If nothing material is wrong, say `LGTM` and mention residual risk only if it is real.\n"
    )
}

fn collect_journal_turns(
    events: Vec<session_journal::JournalEvent>,
) -> Vec<session_journal::JournalEvent> {
    events
        .into_iter()
        .filter(|e| {
            matches!(
                e.event_type,
                session_journal::JournalEventType::Turn
                    | session_journal::JournalEventType::TurnError
            )
        })
        .collect()
}

/// Prefer journal `turn == n` (latest match), else the *n*th `Turn` event (1-based order).
fn resolve_legacy_turn(
    turns: &[session_journal::JournalEvent],
    n: u32,
) -> Option<session_journal::JournalEvent> {
    turns
        .iter()
        .rev()
        .find(|e| e.turn == Some(n))
        .cloned()
        .or_else(|| turns.get((n as usize).saturating_sub(1)).cloned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TurnPick {
    Last,
    List,
    /// Historical behavior: match `turn == n` (latest), else nth Turn event.
    Legacy(u32),
    /// Nth Turn event in chronological order (1-based).
    Seq(u32),
    /// Strict: `event.turn == n` only (latest such event).
    Id(u32),
    /// From end: -1 last, -2 previous, … (matches journal `seq` ordering).
    Relative(i32),
}

fn turn_arg_usage() -> &'static str {
    "Usage: /turn | /turn list | /turn N | /turn seq:N | /turn #N | /turn id:N | /turn @N | /turn -1  (see /help)"
}

fn parse_turn_pick(arg: &str) -> Result<TurnPick, String> {
    let t = arg.trim();
    if t.is_empty() {
        return Ok(TurnPick::Last);
    }
    if t.eq_ignore_ascii_case("list") {
        return Ok(TurnPick::List);
    }
    let lower = t.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("seq:") {
        let n = rest
            .trim()
            .parse::<u32>()
            .map_err(|_| turn_arg_usage().to_string())?;
        if n == 0 {
            return Err("seq must be >= 1".to_string());
        }
        return Ok(TurnPick::Seq(n));
    }
    if let Some(rest) = lower.strip_prefix("id:") {
        let n = rest
            .trim()
            .parse::<u32>()
            .map_err(|_| turn_arg_usage().to_string())?;
        if n == 0 {
            return Err("turn id must be >= 1".to_string());
        }
        return Ok(TurnPick::Id(n));
    }
    if let Some(rest) = t.strip_prefix('#') {
        let n = rest
            .trim()
            .parse::<u32>()
            .map_err(|_| turn_arg_usage().to_string())?;
        if n == 0 {
            return Err("seq must be >= 1".to_string());
        }
        return Ok(TurnPick::Seq(n));
    }
    if let Some(rest) = t.strip_prefix('@') {
        let n = rest
            .trim()
            .parse::<u32>()
            .map_err(|_| turn_arg_usage().to_string())?;
        if n == 0 {
            return Err("turn id must be >= 1".to_string());
        }
        return Ok(TurnPick::Id(n));
    }
    if let Ok(i) = t.parse::<i32>() {
        if i < 0 {
            return Ok(TurnPick::Relative(i));
        }
        if i > 0 {
            return Ok(TurnPick::Legacy(i as u32));
        }
        return Err("turn number must be non-zero".to_string());
    }
    Err(turn_arg_usage().to_string())
}

fn seq_for_event(
    turns: &[session_journal::JournalEvent],
    ev: &session_journal::JournalEvent,
) -> Option<u32> {
    let key = (ev.turn, ev.ts.as_str());
    turns
        .iter()
        .enumerate()
        .rfind(|(_, e)| (e.turn, e.ts.as_str()) == key)
        .map(|(i, _)| i as u32 + 1)
}

fn resolve_turn_pick(
    turns: &[session_journal::JournalEvent],
    pick: TurnPick,
) -> Result<Option<(session_journal::JournalEvent, Option<u32>)>, String> {
    match pick {
        TurnPick::Last => unreachable!("TurnPick::Last is handled before resolve_turn_pick"),
        TurnPick::List => Ok(None),
        TurnPick::Legacy(n) => {
            let ev = resolve_legacy_turn(turns, n);
            Ok(ev.map(|e| {
                let seq = seq_for_event(turns, &e);
                (e, seq)
            }))
        }
        TurnPick::Seq(n) => {
            let ev = turns
                .get((n as usize).saturating_sub(1))
                .cloned()
                .ok_or_else(|| {
                    format!("no journal Turn at seq {n} ({} in session)", turns.len())
                })?;
            Ok(Some((ev, Some(n))))
        }
        TurnPick::Id(n) => {
            let mut found: Option<(usize, session_journal::JournalEvent)> = None;
            for (i, e) in turns.iter().enumerate() {
                if e.turn == Some(n) {
                    found = Some((i, e.clone()));
                }
            }
            let (i, ev) = found.ok_or_else(|| {
                format!("no Turn event with id {n} (use `/turn list` for seq / id columns)")
            })?;
            Ok(Some((ev, Some(i as u32 + 1))))
        }
        TurnPick::Relative(i) => {
            let len = turns.len() as i32;
            let idx = len + i;
            if idx < 0 || idx >= len {
                return Err(format!(
                    "relative index {i} out of range for {} journal turns",
                    turns.len()
                ));
            }
            let ev = turns[idx as usize].clone();
            Ok(Some((ev, Some(idx as u32 + 1))))
        }
    }
}

fn print_turn_journal_list(turns: &[session_journal::JournalEvent]) {
    eprintln!(
        "\n  {}",
        "─── Journal turns (seq = chronological index) ───────────────"
            .bold()
            .cyan()
    );
    eprintln!(
        "  {:>4} {:>6} {:>8}  {}",
        "seq".bold(),
        "id".bold(),
        "ms".bold(),
        "user (preview)".dim()
    );
    for (i, ev) in turns.iter().enumerate() {
        let seq = i + 1;
        let id = ev
            .turn
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".to_string());
        let ms = ev
            .duration_ms
            .map(|m| m.to_string())
            .unwrap_or_else(|| "-".to_string());
        let preview = ev
            .user_input
            .as_deref()
            .map(|s| {
                let s = s.trim();
                if s.chars().count() > 56 {
                    let short: String = s.chars().take(55).collect();
                    format!("{short}…")
                } else {
                    s.to_string()
                }
            })
            .unwrap_or_default();
        let err_mark = if ev.event_type == session_journal::JournalEventType::TurnError {
            " ✗".red().to_string()
        } else {
            String::new()
        };
        eprintln!(
            "  {:>4} {:>6} {:>8}  {}{}",
            seq,
            id,
            ms,
            preview.dim(),
            err_mark
        );
    }
    eprintln!("  {}", "─".repeat(60).cyan().dim());
    eprintln!(
        "{}",
        "  Pick: /turn seq:N or /turn #N  ·  strict id: /turn id:N or /turn @N  ·  legacy: /turn N  ·  relative: /turn -1"
            .dim()
    );
    eprintln!();
}

fn print_turn_trace(ev: &session_journal::JournalEvent, journal_seq: Option<u32>) {
    let total_ms = ev.duration_ms.unwrap_or(1) as f64;
    let sep = "─".repeat(42);
    let id = ev.turn.unwrap_or(0);
    let seq_note = journal_seq
        .map(|s| format!(" · journal seq {s}"))
        .unwrap_or_default();
    let err_tag = if ev.event_type == session_journal::JournalEventType::TurnError {
        " [ERROR]"
    } else {
        ""
    };
    eprintln!(
        "\n  {}",
        format!("─── Turn id {id} trace{seq_note}{err_tag} {sep}")
            .bold()
            .cyan()
    );

    if let Some(ref err) = ev.error {
        eprintln!("  {} {}", theme::icon_err(), err.as_str().red());
    }

    // Calculate tool time — prefer new observability fields, fall back to sum(tc.ms).
    let tool_time_ms: u64 = ev.total_tool_ms.unwrap_or_else(|| {
        ev.tool_calls
            .as_ref()
            .map(|calls| calls.iter().map(|tc| tc.ms).sum())
            .unwrap_or(0)
    });
    let llm_time_ms = ev
        .total_llm_ms
        .unwrap_or_else(|| ev.duration_ms.unwrap_or(0).saturating_sub(tool_time_ms));

    // Summary line
    if let Some(ms) = ev.duration_ms {
        eprintln!(
            "  {} {}",
            "Total:".bold(),
            format!("{:.2}s", ms as f64 / 1000.0).bold()
        );
    }

    // TTFT and context time if available
    if let Some(ttft) = ev.ttft_ms {
        eprintln!(
            "  {} {}ms {}",
            "TTFT:".cyan(),
            ttft,
            "(time to first token)".dim()
        );
    }
    if let Some(ctx) = ev.context_ms {
        let mut parts = Vec::new();
        if let Some(sel) = ev.selector_ms {
            let strat = ev.selector_strategy.as_deref().unwrap_or("?");
            parts.push(format!("selector: {}ms [{}]", sel, strat));
        }
        if let Some(m) = ev.memoria_ms {
            parts.push(format!("memoria: {}ms", m));
        }
        let detail = if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(", "))
        };
        eprintln!(
            "  {} {}ms{}  {}",
            "Context:".cyan(),
            ctx,
            detail,
            "(prompt assembly)".dim()
        );
    }
    if let Some(ref skills) = ev.selected_skills
        && !skills.is_empty()
    {
        eprintln!("  {} {}", "Skills:".cyan(), skills.join(", ").cyan());
    }
    if let Some(rounds) = ev.llm_rounds {
        eprintln!(
            "  {} {} {}",
            "LLM rounds:".cyan(),
            rounds,
            "(LLM→tool cycles within this turn)".dim()
        );
    }
    eprintln!();

    // Timeline visualization
    eprintln!("  {}", "Timeline".bold());
    let bar_width = 40;

    // LLM portion
    let llm_pct = (llm_time_ms as f64 / total_ms * 100.0) as u32;
    let llm_bar_len = (llm_pct as usize * bar_width / 100).max(1);
    let llm_bar = "█".repeat(llm_bar_len);
    eprintln!(
        "    {:<12} {:>6}ms {:>3}%  {}",
        "LLM".cyan(),
        llm_time_ms,
        llm_pct,
        llm_bar.blue()
    );

    // Per-tool bars with I/O sizes
    if let Some(ref calls) = ev.tool_calls {
        for tc in calls {
            let pct = (tc.ms as f64 / total_ms * 100.0) as u32;
            let bar_len = (pct as usize * bar_width / 100).max(1);
            let bar = if tc.ok {
                "█".repeat(bar_len).green()
            } else {
                "█".repeat(bar_len).red()
            };
            let status = if tc.ok { " " } else { "!" };
            let io_info = match (tc.input_bytes, tc.output_bytes) {
                (Some(i), Some(o)) => {
                    format!(" [{}/{}B]", format_bytes(i), format_bytes(o))
                }
                _ => String::new(),
            };
            let display = super::stream_render::format_tool_display_from_preview(
                &tc.name,
                tc.args_preview.as_deref(),
            );
            eprintln!(
                "    {:<12} {:>6}ms {:>3}%  {}{}{}",
                display.cyan(),
                tc.ms,
                pct,
                bar,
                status,
                io_info.dim()
            );
        }
    }

    eprintln!();

    // Detailed trace view (OpenTrace style)
    eprintln!("  {}", "Trace".bold());
    let mut offset = 0u64;

    // Context assembly (if available)
    if let Some(ctx) = ev.context_ms {
        eprintln!(
            "    {} {} Context assembly",
            format!("[{:>5}ms]", offset).dim(),
            "├─".dim()
        );
        if let Some(mem) = ev.memoria_ms {
            eprintln!(
                "    {} {}   memoria search ({}ms)",
                format!("[{:>5}ms]", offset).dim(),
                "│ ".dim(),
                mem
            );
        }
        if let Some(sel) = ev.selector_ms {
            let strat = ev.selector_strategy.as_deref().unwrap_or("unknown");
            eprintln!(
                "    {} {}   tool selection ({}ms, {}){}",
                format!("[{:>5}ms]", offset).dim(),
                "│ ".dim(),
                sel,
                strat,
                if sel > 3000 { "  ← slow" } else { "" }
            );
            if let Some(ref skills) = ev.selected_skills
                && !skills.is_empty()
            {
                eprintln!(
                    "    {} {}   selected skills: {}",
                    format!("[{:>5}ms]", offset).dim(),
                    "│ ".dim(),
                    skills.join(", ").cyan()
                );
            }
        }
        offset = ctx;
        eprintln!(
            "    {} {} complete ({}ms)",
            format!("[{:>5}ms]", offset).dim(),
            "│".dim(),
            ctx.to_string().dim()
        );
    }

    // LLM call
    eprintln!(
        "    {} {} LLM request",
        format!("[{:>5}ms]", offset).dim(),
        "├─".dim()
    );
    if let Some(ref m) = ev.model {
        eprintln!(
            "    {}    {} model: {}",
            " ".repeat(8),
            "│".dim(),
            m.as_str().dim()
        );
    }
    if let Some(t_in) = ev.tokens_in {
        let sel_note = match (ev.selector_tokens_in, ev.selector_tokens_out) {
            (Some(si), Some(so)) if si > 0 || so > 0 => {
                format!(" (+selector: {}→{})", si, so)
            }
            _ => String::new(),
        };
        eprintln!(
            "    {}    {} input: {} tokens{}",
            " ".repeat(8),
            "│".dim(),
            t_in.to_string().dim(),
            sel_note.dim()
        );
    }
    // Show TTFT inline
    if let Some(ttft) = ev.ttft_ms {
        let ttft_offset = offset + ttft;
        eprintln!(
            "    {} {} first token (TTFT: {}ms)",
            format!("[{:>5}ms]", ttft_offset).dim(),
            "│".dim(),
            ttft.to_string().yellow()
        );
    }
    if let Some(t_out) = ev.tokens_out {
        eprintln!(
            "    {}    {} output: {} tokens",
            " ".repeat(8),
            "│".dim(),
            t_out.to_string().dim()
        );
    }
    offset += llm_time_ms;
    eprintln!(
        "    {} {} LLM complete ({}ms)",
        format!("[{:>5}ms]", offset).dim(),
        "│".dim(),
        llm_time_ms.to_string().yellow()
    );

    // Tool calls — use start_offset_ms for real timeline when available.
    if let Some(ref calls) = ev.tool_calls {
        let has_real_offsets = calls.iter().any(|tc| tc.start_offset_ms.is_some());
        for (i, tc) in calls.iter().enumerate() {
            let is_last = i == calls.len() - 1;
            let branch = if is_last { "└─" } else { "├─" };
            let status = if tc.ok {
                theme::icon_ok()
            } else {
                theme::icon_err()
            };

            // Build I/O size annotation
            let io_info = match (tc.input_bytes, tc.output_bytes) {
                (Some(i), Some(o)) => {
                    format!(" (in:{} out:{})", format_bytes(i), format_bytes(o))
                }
                (Some(i), None) => format!(" (in:{})", format_bytes(i)),
                (None, Some(o)) => format!(" (out:{})", format_bytes(o)),
                (None, None) => String::new(),
            };

            let display = super::stream_render::format_tool_display_from_preview(
                &tc.name,
                tc.args_preview.as_deref(),
            );

            // Use real start_offset_ms when available; fall back to accumulated offset.
            let tool_offset = if has_real_offsets {
                tc.start_offset_ms.unwrap_or(offset)
            } else {
                offset
            };

            // Show round and parallel info when available.
            let round_tag = tc.round.map(|r| format!(" R{r}")).unwrap_or_default();
            let par_tag = if tc.parallel == Some(true) {
                " ∥"
            } else {
                ""
            };

            eprintln!(
                "    {} {} {} {}{}{}{}",
                format!("[{:>5}ms]", tool_offset).dim(),
                branch.dim(),
                status,
                display.cyan(),
                io_info.dim(),
                round_tag.dim(),
                par_tag.dim(),
            );

            if let Some(ref err) = tc.error {
                let err_preview = truncate_str(err, 50);
                let sub_branch = if is_last { "   " } else { "│  " };
                eprintln!(
                    "    {}    {} {}",
                    " ".repeat(8),
                    sub_branch.dim(),
                    err_preview.red()
                );
            }
            let end_offset = if has_real_offsets {
                tool_offset + tc.ms
            } else {
                offset += tc.ms;
                offset
            };
            let sub_branch = if is_last { "   " } else { "│  " };
            eprintln!(
                "    {}    {} complete ({}ms)",
                format!("[{:>5}ms]", end_offset).dim(),
                sub_branch.dim(),
                tc.ms.to_string().dim()
            );
        }
    }

    eprintln!();

    // Breakdown summary
    eprintln!("  {}", "Breakdown".bold());
    let llm_note = if llm_pct > 80 {
        "← bottleneck".yellow().to_string()
    } else {
        String::new()
    };
    eprintln!(
        "    {:<12} {:>6}ms  {:>3}%  {}",
        "LLM".cyan(),
        llm_time_ms,
        llm_pct,
        llm_note
    );
    let tool_pct = 100u32.saturating_sub(llm_pct);
    let tool_note = if tool_pct > 80 {
        "← bottleneck".yellow().to_string()
    } else {
        String::new()
    };
    eprintln!(
        "    {:<12} {:>6}ms  {:>3}%  {}",
        "Tools".cyan(),
        tool_time_ms,
        tool_pct,
        tool_note
    );

    // Tokens per second
    if let (Some(t_out), Some(ms)) = (ev.tokens_out, ev.duration_ms)
        && ms > 0
    {
        let tps = t_out as f64 / (ms as f64 / 1000.0);
        eprintln!("    {:<12} {:>6.1} tokens/s", "Throughput".cyan(), tps);
    }

    eprintln!("  {}", "─".repeat(56).cyan().dim());
    eprintln!();
}

pub(super) async fn handle_info_command(
    cmd: &str,
    arg: &str,
    api: &astra_thin_client::ThinClient,
    state: &mut ReplState,
    token: Option<&str>,
) -> Result<(), String> {
    match cmd {
        "/history" => {
            if state.history.is_empty() {
                eprintln!("{}", "  No history yet".dim());
            } else if arg.starts_with("grep ") {
                // /history grep <query>
                let query = arg
                    .split_once(' ')
                    .map(|x| x.1)
                    .unwrap_or("")
                    .to_lowercase();
                if query.is_empty() {
                    eprintln!("{}", "  Usage: /history grep <query>".yellow());
                    return Ok(());
                }
                let mut found = 0;
                for (i, (user, asst)) in state.history.iter().enumerate() {
                    let turn_n = i + 1;
                    let matches_user = user.to_lowercase().contains(&query);
                    let matches_asst = asst.to_lowercase().contains(&query);
                    if matches_user || matches_asst {
                        found += 1;
                        eprintln!("  {}", format!("Turn {turn_n}").bold());
                        if matches_user {
                            let u = truncate_str(user, 120);
                            eprintln!("  {} {}", "❯".cyan(), u);
                        }
                        if matches_asst {
                            let a = truncate_str(asst, 120);
                            eprintln!("    {}", a.dim());
                        }
                        eprintln!();
                    }
                }
                if found == 0 {
                    eprintln!("{}", format!("  No matches for '{query}'").dim());
                } else {
                    eprintln!("{}", format!("  {found} turn(s) matched").dim());
                }
            } else {
                eprintln!(
                    "\n{}",
                    "─── Conversation History ─────────────────────────────────────"
                        .bold()
                        .cyan()
                );
                for (i, (user, asst)) in state.history.iter().enumerate() {
                    let turn_n = i + 1;
                    let u = truncate_str(user, 80);
                    let a = truncate_str(asst, 80);
                    eprintln!("  {}", format!("Turn {turn_n}").bold());
                    eprintln!("  {} {}", "❯".cyan(), u);
                    eprintln!("    {}", a.dim());
                    if i + 1 < state.history.len() {
                        eprintln!();
                    }
                }
                eprintln!();
            }
        }

        "/grep" => {
            let request = match parse_grep_request(arg) {
                Ok(request) => request,
                Err(usage) => {
                    eprintln!("{}", format!("  {usage}").yellow());
                    return Ok(());
                }
            };

            let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let executor = edge_tools::ToolExecutor::new(project_root);

            let (title, result) = match request {
                GrepRequest::Content(pattern) => (
                    format!("Workspace grep · {pattern}"),
                    executor.grep(&serde_json::json!({"pattern": pattern, "path": "."})),
                ),
                GrepRequest::Files(pattern) => (
                    format!("Workspace glob · {pattern}"),
                    executor.glob(&serde_json::json!({"pattern": pattern, "path": "."})),
                ),
                GrepRequest::Review(pattern) => {
                    let title = format!("Review grep · {pattern}");
                    (title, review_search(&executor, &pattern))
                }
            };

            eprintln!(
                "\n{}",
                format!("─── {title} ─────────────────────────────────────────────")
                    .bold()
                    .cyan()
            );
            for line in result.lines() {
                eprintln!("  {line}");
            }
            eprintln!();
        }

        "/lsp" => {
            let subcommand = arg.trim();
            if !subcommand.is_empty() && subcommand != "status" {
                eprintln!("{}", "  Usage: /lsp [status]".yellow());
                return Ok(());
            }

            let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let executor = edge_tools::ToolExecutor::new(project_root);
            let result = executor.lsp(&serde_json::json!({"operation": "diagnostics"}));
            let parsed: serde_json::Value = serde_json::from_str(&result)
                .unwrap_or_else(|_| serde_json::json!({ "error": "invalid lsp status response" }));

            if let Some(error) = parsed.get("error").and_then(serde_json::Value::as_str) {
                eprintln!(
                    "\n{}\n  {}\n",
                    "─── LSP Status ───────────────────────────────────────────────"
                        .bold()
                        .cyan(),
                    error.yellow()
                );
            } else {
                print_lsp_status_report(&parsed);
            }
        }

        "/review" => {
            let Some(tok) = token else {
                eprintln!("{}", "  Not logged in. Use /login.".yellow());
                return Ok(());
            };
            let project_root = std::env::current_dir().unwrap_or_default();
            let prompt = build_review_prompt(arg, &project_root);
            let review_label = if arg.trim().is_empty() {
                "HEAD".to_string()
            } else {
                arg.trim().to_string()
            };
            eprintln!(
                "\n{}",
                format!("─── Review · {review_label} ─────────────────────────────────────")
                    .bold()
                    .cyan()
            );
            let selector = crate::repl_runtime::create_tool_selector_quiet(api, None);
            let mut pm = PermissionManager::with_project(false, &project_root);
            let turn_start = std::time::Instant::now();
            let sr = stream_chat_sse(ChatTurnParams {
                api,
                token: tok,
                message: &prompt,
                session_id: state.session_id.as_deref(),
                model: state.model.as_deref(),
                explain: state.explain,
                render_md: true,
                history: &state.history,
                perm_manager: &mut pm,
                verbose_mode: state.verbose_mode,
                render_policy: crate::stream_render::RenderPolicy::Stream,
                selector: &*selector.0,
                recent_tools: &state.recent_tools,
                tool_health_entries: &state.tool_health_entries,
                unified_skill_registry: astra_runtime::skills::default_unified_registry(),
                plan_only_chat: false,
                is_plan_subtask: false,
                plan_subtask_id: None,
                delegation_engine: None,
                cancel_token: None,
                plan_assemble_line_release: None,
                stream_event_tx: None,
                approval_request_tx: None,
                mcp_manager: Some(state.mcp_manager.clone()),
                skill_search: &state.skill_search,
                skill_quality_tracker: &mut state.skill_quality_tracker,
                discovered_skills: None,
                messaging_metrics: state.messaging_metrics.clone(),
                agent_spawner: state.agent_spawner.clone(),
                root_agent_id: Some("main"),
                root_mailbox_slot: Some(&mut state.root_mailbox),
                observability_hub: state.observability_hub.clone(),
                observability_session: state.observability_session.clone(),
                file_journal: None,
                database_snapshot_journal: None,
                git_stash_journal: None,
                git_commit_journal: None,
                git_worktree_journal: None,
                session_state_journal: None,
                task_manager: None,
                turn_index: 0,
                evolution_service: state.evolution_service.clone(),
            })
            .await
            .map_err(|f| f.error)?;
            if let Some(session_id) = sr.session_id.as_deref() {
                crate::repl_turn::initialize_journal_pub(state, session_id);
                state.session_id = Some(session_id.to_string());
            }
            state.last_response = Some(sr.full_text.clone());
            let review_input = format!("/review {arg}").trim().to_string();
            state
                .history
                .push((review_input.clone(), sr.full_text.clone()));
            state.turn += 1;
            state.total_prompt_tokens += sr.prompt_tokens;
            state.total_completion_tokens += sr.completion_tokens;
            state.recent_tools = sr.tools_used.clone();

            // Write turn event to journal (same as normal chat turns).
            if let Some(journal) = state.journal.as_ref() {
                let turn_event = astra_services::session_journal::JournalEvent::turn(
                    state.session_id.as_deref(),
                    state.turn,
                    state.model.as_deref(),
                    &review_input,
                    &sr.full_text,
                    sr.tool_calls_count,
                    sr.prompt_tokens,
                    sr.completion_tokens,
                    turn_start.elapsed().as_millis() as u64,
                )
                .with_tool_calls(sr.tool_call_records)
                .with_budget_pressure(sr.budget_pressure)
                .with_tool_selection(
                    sr.tools_selected,
                    sr.selected_skills,
                    sr.tools_used.clone(),
                    sr.budget_used,
                )
                .with_ttft(sr.ttft_ms)
                .with_context_time(sr.context_ms)
                .with_selector_strategy(sr.selector_strategy)
                .with_selector_time(sr.selector_ms)
                .with_selector_tokens(sr.selector_tokens_in, sr.selector_tokens_out)
                .with_memoria_time(sr.memoria_ms);
                state.last_turn_event = Some(turn_event.clone());
                let _ = journal.append(&turn_event);
            }
        }

        "/copy" => match &state.last_response {
            Some(text) => {
                let text = text.clone();
                let n = text.chars().count();
                let preview: String = text.chars().take(60).collect();
                let preview_display = if text.chars().count() > 60 {
                    format!("{}…", preview)
                } else {
                    preview
                };
                if copy_to_clipboard(&text) {
                    eprintln!("{}", format!("  ✓ Copied ({n} chars)").green());
                    eprintln!("  {}", preview_display.dim());
                } else {
                    eprintln!(
                        "{}",
                        "  ✗ No clipboard tool found (install xclip or xsel)".yellow()
                    );
                }
            }
            None => eprintln!("{}", "  ✗ No response to copy yet".yellow()),
        },

        "/diagnostics" => {
            eprintln!(
                "\n{}",
                "─── Diagnostics ──────────────────────────────────────────────"
                    .bold()
                    .cyan()
            );

            // Accumulate rows: (ok: bool, label: &str, detail: String)
            let mut rows: Vec<(bool, &'static str, String)> = Vec::new();

            // Binary version
            rows.push((
                true,
                "binary",
                format!("astra v{}", env!("CARGO_PKG_VERSION")),
            ));

            // API base URL (same origin used for /health, /login, etc.)
            rows.push((true, "api url", api.api_origin()));

            // API health (+ embedded DB probe summary — note: health may use a separate short-lived pool)
            match api.get_health_text().await {
                Ok(body) => {
                    let parsed: serde_json::Value =
                        serde_json::from_str(&body).unwrap_or(serde_json::json!({}));
                    let status = parsed
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let database = parsed
                        .get("database")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let health_ok = status == "healthy" && database == "connected";
                    rows.push((
                        health_ok,
                        "api health",
                        format!("status={status}, database={database}"),
                    ));
                }
                Err(e) => {
                    rows.push((false, "api health", e.to_string()));
                }
            }

            // Auth status — absence of a token is normal, not a "failed check"
            if let Some(tok) = token {
                match api.get_auth_me_text(tok).await {
                    Ok(b) => {
                        let v: serde_json::Value = serde_json::from_str(&b).unwrap_or_default();
                        let un = v.get("username").and_then(|u| u.as_str()).unwrap_or("?");
                        rows.push((true, "auth", format!("logged in as {un}")));
                    }
                    Err(astra_thin_client::ThinClientError::Api { status, .. })
                        if status.as_u16() == 401 =>
                    {
                        rows.push((false, "auth", "token expired — run /login".to_string()));
                    }
                    Err(astra_thin_client::ThinClientError::Api { status, .. }) => {
                        rows.push((false, "auth", format!("HTTP {status}")));
                    }
                    Err(e) => {
                        rows.push((false, "auth", e.to_string()));
                    }
                }
            } else {
                rows.push((
                    true,
                    "auth",
                    "not logged in — use /login or /register".to_string(),
                ));
            }

            // Git repo
            match std::process::Command::new("git")
                .args(["rev-parse", "--show-toplevel"])
                .output()
            {
                Ok(out) if out.status.success() => {
                    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    rows.push((true, "git repo", path));
                }
                _ => {
                    rows.push((false, "git repo", "not a git repo".to_string()));
                }
            }

            // Memoria
            let memoria_key_set = std::env::var("MEMORIA_API_KEY")
                .or_else(|_| std::env::var("MEMORIA_MASTER_KEY"))
                .is_ok();
            if memoria_key_set {
                let memoria_base = std::env::var("MEMORIA_BASE_URL")
                    .unwrap_or_else(|_| astra_core::config::DEFAULT_MEMORIA_URL.to_string());
                let memoria_health = format!("{}/health", memoria_base.trim_end_matches('/'));
                match api.get_url(&memoria_health).await {
                    Ok(r) if r.status().is_success() => {
                        rows.push((true, "memoria", format!("reachable at {memoria_base}")));
                    }
                    Ok(r) => {
                        rows.push((
                            false,
                            "memoria",
                            format!("HTTP {} at {memoria_base}", r.status()),
                        ));
                    }
                    Err(_) => {
                        // When https fails, probe http to give an actionable hint
                        let hint = if memoria_base.starts_with("https://") {
                            let http_url = memoria_base.replacen("https://", "http://", 1);
                            let http_health = format!("{}/health", http_url.trim_end_matches('/'));
                            if api
                                .get_url(&http_health)
                                .await
                                .is_ok_and(|r| r.status().is_success())
                            {
                                format!(
                                    "reachable over http, not https — set MEMORIA_BASE_URL={http_url}"
                                )
                            } else {
                                format!("unreachable ({memoria_base})")
                            }
                        } else {
                            format!("unreachable ({memoria_base})")
                        };
                        rows.push((false, "memoria", hint));
                    }
                }
            } else {
                rows.push((false, "memoria", "MEMORIA_API_KEY not set".to_string()));
            }

            // Print table
            let label_w = rows.iter().map(|(_, l, _)| l.len()).max().unwrap_or(10);
            for (ok, label, detail) in &rows {
                let icon = if *ok {
                    "✓".green().to_string()
                } else {
                    "✗".red().to_string()
                };
                eprintln!("  {}  {:<label_w$}  {}", icon, label, detail.clone().dim());
            }

            let fail_count = rows.iter().filter(|(ok, _, _)| !ok).count();
            eprintln!();
            if fail_count == 0 {
                eprintln!("  {}", "All checks passed".green().bold());
            } else {
                eprintln!("  {}", format!("{fail_count} check(s) failed").red().bold());
                eprintln!(
                    "  {}",
                    "Hint: GET /health may pass while auth pool-times out — they use different connection paths."
                        .dim()
                );
            }
            eprintln!();
        }

        "/context" => {
            let (sub_cmd, _sub_arg) = match arg.find(char::is_whitespace) {
                Some(pos) => (arg[..pos].trim(), arg[pos..].trim()),
                None => (arg.trim(), ""),
            };

            // /context breakdown — show last turn's actual context assembly trace
            if sub_cmd == "breakdown" || sub_cmd == "trace" {
                let session = state.observability_session.as_ref();
                if let Some(session) = session {
                    let guard = session.read().unwrap_or_else(|e| e.into_inner());
                    if guard.context_traces.is_empty() {
                        eprintln!(
                            "{}",
                            "  No context assembly traces yet. Complete a turn first.".yellow()
                        );
                    } else {
                        // Show latest trace
                        let trace = guard.context_traces.last().unwrap();
                        print_context_breakdown(trace);
                    }
                } else {
                    eprintln!("{}", "  No observability session active.".yellow());
                }
                return Ok(());
            }

            let sep = "─".repeat(38);
            eprintln!("\n  {}", format!("─── Context Window {sep}").bold().cyan());

            // ── Identity ──
            let model_display = state.model.clone().unwrap_or_else(|| "default".to_string());
            eprintln!("  {:<12}  {}", "model".cyan(), model_display.dim());
            eprintln!("  {:<12}  {}", "turn".cyan(), state.turn.to_string().dim());

            // ── Token usage bar ──
            let est_messages: Vec<serde_json::Value> = state
                .history
                .iter()
                .flat_map(|(u, a)| {
                    let mut pair = Vec::with_capacity(2);
                    if !u.is_empty() {
                        pair.push(serde_json::json!({"role":"user","content":u}));
                    }
                    if !a.is_empty() {
                        pair.push(serde_json::json!({"role":"assistant","content":a}));
                    }
                    pair
                })
                .collect();
            let history_tokens = prompts::estimate_tokens(&est_messages);
            let budget = &state.context_budget;
            let limit = budget.model_limit;
            let usage_pct = if limit > 0 {
                (history_tokens as f64 / limit as f64 * 100.0).min(100.0)
            } else {
                0.0
            };

            // Visual bar: 30 chars wide
            let bar_width = 30usize;
            let filled = ((usage_pct / 100.0) * bar_width as f64) as usize;
            let empty = bar_width.saturating_sub(filled);
            let bar_color = if usage_pct < 60.0 {
                "green"
            } else if usage_pct < 85.0 {
                "yellow"
            } else {
                "red"
            };
            let bar_str = format!(
                "[{}{}] {:.0}%  (~{}k / {}k)",
                "█".repeat(filled),
                "░".repeat(empty),
                usage_pct,
                history_tokens / 1000,
                limit / 1000
            );
            let bar_display = match bar_color {
                "green" => bar_str.green(),
                "yellow" => bar_str.yellow(),
                _ => bar_str.red(),
            };
            let est_pressure = if budget.compact_trigger() > 0 {
                (history_tokens as f64 / budget.compact_trigger() as f64).min(1.0)
            } else {
                0.0
            };
            let (status_icon, status_label, status_hint) =
                describe_context_pressure(usage_pct, est_pressure);
            eprintln!(
                "  {:<12}  {} {}",
                "status".cyan(),
                status_icon,
                status_label.bold()
            );
            eprintln!("  {:<12}  {}", "what it means".cyan(), status_hint.dim());
            eprintln!("  {:<12}  {bar_display}", "usage".cyan());

            // ── Breakdown ──
            // System+tools estimate: typically ~5-15% of budget
            let free = limit.saturating_sub(history_tokens);
            eprintln!(
                "  {:<12}  {}",
                "history".cyan(),
                format!(
                    "~{}k tokens across {} messages; ~{}k tokens still free",
                    history_tokens / 1000,
                    state.history.len() * 2,
                    free / 1000
                )
                .dim()
            );

            // ── Compaction tier ──
            let compact_trigger_k = budget.compact_trigger() / 1000;
            let tier_emoji = if est_pressure < PRESSURE_HEALTHY_THRESHOLD {
                "🟢"
            } else if est_pressure < PRESSURE_WARNING_THRESHOLD {
                "🟡"
            } else {
                "🔴"
            };
            let tier_label = if est_pressure < PRESSURE_HEALTHY_THRESHOLD {
                "Normal"
            } else if est_pressure < PRESSURE_WARNING_THRESHOLD {
                "Approaching compact"
            } else {
                "Near compact trigger"
            };
            eprintln!(
                "  {:<12}  {}",
                "compaction".cyan(),
                format!(
                    "{tier_emoji} {tier_label}  (starts near ~{compact_trigger_k}k; keep {} recent turns)",
                    budget.keep_recent_turns
                )
                .dim()
            );

            // ── Cache status ──
            let total_cr = state.total_cache_read_tokens;
            let total_cw = state.total_cache_creation_tokens;
            let cache_emoji = if total_cr > 0 { "🟢" } else { "⚪" };
            eprintln!(
                "  {:<12}  {}",
                "cache".cyan(),
                format!(
                    "{cache_emoji} total: read {}k / write {}k",
                    total_cr / 1000,
                    total_cw / 1000
                )
                .dim()
            );

            // ── Attention ──
            if let Some(ref anchor) = state.continuation_anchor {
                let parsed = parse_continuation_anchor(anchor);
                if let Some(task) = parsed.task.as_deref() {
                    eprintln!("  {:<12}  {}", "task".cyan(), truncate_str(task, 80).dim());
                }
                if let Some(direction) = parsed.direction.as_deref() {
                    eprintln!(
                        "  {:<12}  {}",
                        "direction".cyan(),
                        truncate_str(direction, 80).dim()
                    );
                }
                if parsed.task.is_none() && parsed.direction.is_none() {
                    eprintln!(
                        "  {:<12}  {}",
                        "focus".cyan(),
                        truncate_str(anchor, 80).dim()
                    );
                }
            }
            if let Some(ref goal) = state.session_goal {
                eprintln!("  {:<12}  {}", "goal".cyan(), truncate_str(goal, 80).dim());
            }

            eprintln!("  {}", "─".repeat(56).cyan().dim());

            // Inline last turn's actual component breakdown if available
            if let Some(ref obs) = state.observability_session {
                let guard = obs.read().unwrap_or_else(|e| e.into_inner());
                if let Some(trace) = guard.context_traces.last() {
                    let tb = &trace.token_budget;
                    if tb.total_used > 0 {
                        eprintln!(
                            "\n  {}  ({})",
                            "Last turn actual allocation:".bold().cyan(),
                            trace.turn_id.as_str().dim()
                        );
                        let components: &[(&str, u32)] = &[
                            ("system_prompt", tb.system_prompt_tokens),
                            ("history", tb.history_tokens),
                            ("memory", tb.memory_tokens),
                            ("tool_schemas", tb.tool_schema_tokens),
                            ("user_message", tb.user_message_tokens),
                        ];
                        for (label, tokens) in components {
                            if *tokens > 0 {
                                let pct = (*tokens as f64 / tb.total_used as f64 * 100.0) as u32;
                                eprintln!(
                                    "    {:<16} {:>6} ({:>2}%)",
                                    format!("{label}:").dim(),
                                    tokens.to_string().cyan(),
                                    pct
                                );
                            }
                        }
                        let pressure_str = format!("{:.0}%", tb.budget_pressure * 100.0);
                        let pressure_colored = if tb.budget_pressure > 0.9 {
                            pressure_str.red().to_string()
                        } else if tb.budget_pressure > 0.7 {
                            pressure_str.yellow().to_string()
                        } else {
                            pressure_str.green().to_string()
                        };
                        eprintln!(
                            "    {:<16} {} / {} ({})",
                            "total:".dim(),
                            tb.total_used.to_string().cyan().bold(),
                            tb.max_tokens.to_string().dim(),
                            pressure_colored
                        );
                    }
                }
            }

            eprintln!("  {}", "Use /context breakdown for full details".dim());
            eprintln!();
        }

        "/turn" => {
            let pick = match parse_turn_pick(arg) {
                Ok(p) => p,
                Err(msg) => {
                    eprintln!("{}", format!("  {msg}").yellow());
                    return Ok(());
                }
            };

            if pick == TurnPick::List {
                let Some(sid) = state.session_id.as_deref() else {
                    eprintln!(
                        "{}",
                        "  No active session; cannot list journal turns.".yellow()
                    );
                    return Ok(());
                };
                match session_journal::read_journal(sid) {
                    Ok(events) => {
                        let turns = collect_journal_turns(events);
                        if turns.is_empty() {
                            eprintln!("{}", "  No Turn events in this session journal yet.".dim());
                        } else {
                            print_turn_journal_list(&turns);
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "{}",
                            format!("  Failed to read session journal: {e}").yellow()
                        );
                    }
                }
                return Ok(());
            }

            if pick == TurnPick::Last {
                if let Some(ev) = state.last_turn_event.as_ref() {
                    let seq = state
                        .session_id
                        .as_deref()
                        .and_then(|sid| {
                            session_journal::read_journal(sid).ok().map(|events| {
                                let turns = collect_journal_turns(events);
                                seq_for_event(&turns, ev)
                            })
                        })
                        .flatten();
                    print_turn_trace(ev, seq);
                } else if let Some(sid) = state.session_id.as_deref() {
                    match session_journal::read_journal(sid) {
                        Ok(events) => {
                            let turns = collect_journal_turns(events);
                            if let Some(ev) = turns.last() {
                                let seq = Some(turns.len() as u32);
                                print_turn_trace(ev, seq);
                            } else {
                                eprintln!(
                                    "{}",
                                    "  No Turn events in journal yet. Complete a turn first.".dim()
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "{}",
                                format!("  Failed to read session journal: {e}").yellow()
                            );
                        }
                    }
                } else {
                    eprintln!(
                        "{}",
                        "  No active session and no in-memory turn; cannot show trace.".yellow()
                    );
                }
                return Ok(());
            }

            let Some(sid) = state.session_id.as_deref() else {
                eprintln!(
                    "{}",
                    "  No active session; cannot load /turn from journal.".yellow()
                );
                return Ok(());
            };
            let turns = match session_journal::read_journal(sid) {
                Ok(events) => collect_journal_turns(events),
                Err(e) => {
                    eprintln!(
                        "{}",
                        format!("  Failed to read session journal: {e}").yellow()
                    );
                    return Ok(());
                }
            };
            match resolve_turn_pick(&turns, pick) {
                Ok(Some((ev, seq))) => print_turn_trace(&ev, seq),
                Ok(None) => {
                    eprintln!(
                        "{}",
                        "  No matching journal Turn for that selector.".yellow()
                    );
                }
                Err(e) => eprintln!("{}", format!("  {e}").yellow()),
            }
        }

        "/version" => {
            eprintln!("{}", "  astra version 0.1.0 (Rust)".bold());
        }

        "/rewind" => {
            if arg.is_empty() {
                // Show available turns
                if state.history.is_empty() {
                    eprintln!("{}", "  No history to rewind".dim());
                } else {
                    eprintln!("{}", "  Usage: /rewind <turn_number>".yellow());
                    eprintln!(
                        "{}",
                        format!(
                            "  Current: turn {} ({} exchanges)",
                            state.turn,
                            state.history.len()
                        )
                        .dim()
                    );
                    for (i, (user, _)) in state.history.iter().enumerate() {
                        let turn_n = i + 1;
                        let u = truncate_str(user, 60);
                        eprintln!(
                            "  {} {} {}",
                            format!("{turn_n}").bold(),
                            "❯".cyan(),
                            u.dim()
                        );
                    }
                }
            } else {
                let target: usize = match arg.parse() {
                    Ok(n) => n,
                    Err(_) => {
                        eprintln!(
                            "{}",
                            "  Usage: /rewind <turn_number> (e.g. /rewind 3)".yellow()
                        );
                        return Ok(());
                    }
                };
                if target == 0 {
                    // Rewind to start = clear history
                    let old_len = state.history.len();
                    state.history.clear();
                    state.turn = 0;
                    state.last_response = None;
                    if let Some(ref j) = state.journal {
                        let _ = j.append(&session_journal::JournalEvent::config_change(
                            state.session_id.as_deref(),
                            "rewind",
                            &format!("rewound to start, removed {old_len} turn(s)"),
                        ));
                    }
                    eprintln!(
                        "{}",
                        format!("  ✓ Rewound to start. Removed {old_len} turn(s).").green()
                    );
                } else if target > state.history.len() {
                    eprintln!(
                        "{}",
                        format!(
                            "  ✗ Turn {target} does not exist (max: {})",
                            state.history.len()
                        )
                        .yellow()
                    );
                } else {
                    let old_len = state.history.len();
                    let removed = old_len - target;
                    state.history.truncate(target);
                    state.turn = target as u32;
                    state.last_response = state.history.last().map(|(_, a)| a.clone());
                    if let Some(ref j) = state.journal {
                        let _ = j.append(&session_journal::JournalEvent::config_change(
                            state.session_id.as_deref(),
                            "rewind",
                            &format!(
                                "rewound from turn {old_len} to {target}, removed {removed} turn(s)"
                            ),
                        ));
                    }
                    eprintln!(
                        "{}",
                        format!("  ✓ Rewound to turn {target}. Removed {removed} turn(s).").green()
                    );
                }
            }
        }

        "/report" => {
            // Check active durable task state first, then fallback to last saved report
            let report = state
                .durable_task_state
                .as_ref()
                .and_then(|d| d.last_report.as_ref())
                .or(state.last_delivery_report.as_ref());

            if let Some(report) = report {
                super::durable_bridge::display_delivery_report(report);
                if arg.trim() == "save" || arg.trim() == "json" {
                    super::durable_bridge::save_delivery_report_json(report);
                }
            } else {
                eprintln!(
                    "{}",
                    "  No delivery report available. Complete a plan with /plan first.".dim()
                );
            }
        }

        _ => unreachable!("unexpected info command: {cmd}"),
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct ContinuationAnchorParts {
    task: Option<String>,
    direction: Option<String>,
}

fn parse_continuation_anchor(anchor: &str) -> ContinuationAnchorParts {
    let mut task = None;
    let mut direction = None;

    for line in anchor
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if let Some(rest) = line.strip_prefix("Latest user task: ") {
            task = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("Latest assistant direction: ") {
            direction = Some(rest.to_string());
        }
    }

    ContinuationAnchorParts { task, direction }
}

fn print_context_breakdown(
    trace: &astra_runtime::turn::context_assembly_trace::ContextAssemblyTrace,
) {
    eprintln!(
        "\n{}",
        format!(
            "─── Context Breakdown — {} ──────────────────",
            trace.turn_id
        )
        .bold()
        .cyan()
    );

    let tb = &trace.token_budget;
    let sp = &trace.system_prompt;
    let hist = &trace.history;
    let tools = &trace.tools;

    let components: &[(&str, u32)] = &[
        ("system_prompt", tb.system_prompt_tokens),
        ("history", tb.history_tokens),
        ("memory", tb.memory_tokens),
        ("tool_schemas", tb.tool_schema_tokens),
        ("user_message", tb.user_message_tokens),
    ];
    let max_tok = components.iter().map(|(_, t)| *t).max().unwrap_or(1).max(1);
    let bar_max = 30;

    eprintln!();
    for (label, tokens) in components {
        let bar_len = (*tokens as usize * bar_max) / max_tok as usize;
        let bar: String = "█".repeat(bar_len.max(if *tokens > 0 { 1 } else { 0 }));
        let pct = if tb.total_used > 0 {
            (*tokens as f64 / tb.total_used as f64 * 100.0) as u32
        } else {
            0
        };
        eprintln!(
            "  {:<16} {:>6} ({:>2}%) {}",
            format!("{label}:").dim(),
            tokens.to_string().cyan(),
            pct,
            bar.dim()
        );

        // Sub-components for system_prompt
        if *label == "system_prompt" && sp.total_tokens > 0 {
            let subs: &[(&str, u32)] = &[
                ("base_persona", sp.base_persona_tokens),
                ("environment", sp.environment_tokens),
                ("preferences", sp.user_preferences_tokens),
            ];
            for (sub, t) in subs {
                if *t > 0 {
                    eprintln!(
                        "    {:<14} {:>5}",
                        format!("└ {sub}").dim(),
                        t.to_string().dim()
                    );
                }
            }
            for sk in &sp.skills_injected {
                eprintln!(
                    "    {:<14} {:>5}",
                    format!("└ skill:{}", sk.skill_name).dim(),
                    sk.tokens.to_string().dim()
                );
            }
            for mem in &sp.repository_memories {
                let preview: String = mem.content_preview.chars().take(30).collect();
                eprintln!(
                    "    {:<14} {:>5}  {}",
                    "└ memory".to_string().dim(),
                    mem.tokens.to_string().dim(),
                    preview.dim()
                );
            }
        }

        // Sub-components for history
        if *label == "history" && !hist.turns_retained.is_empty() {
            for tr in &hist.turns_retained {
                let role_char = match tr.role.as_str() {
                    "user" => "U",
                    "assistant" => "A",
                    _ => "?",
                };
                let tc = if tr.has_tool_calls { " 🔧" } else { "" };
                eprintln!(
                    "    {:<14} {:>5}",
                    format!("└ t{} {role_char}{tc}", tr.turn_index).dim(),
                    tr.tokens.to_string().dim()
                );
            }
            if !hist.turns_dropped.is_empty() {
                eprintln!(
                    "    {}",
                    format!("└ {} turns dropped", hist.turns_dropped.len()).dim()
                );
            }
            if hist.compression_ratio > 0.0 && hist.compression_ratio < 1.0 {
                eprintln!(
                    "    {}",
                    format!(
                        "└ compressed {:.0}%",
                        (1.0 - hist.compression_ratio) * 100.0
                    )
                    .dim()
                );
            }
        }

        // Sub-components for tool_schemas
        if *label == "tool_schemas" && !tools.tools_selected.is_empty() {
            for ts in &tools.tools_selected {
                eprintln!(
                    "    {:<14} {:>5}",
                    format!("└ {}", ts.tool_name).dim(),
                    ts.tokens.to_string().dim()
                );
            }
        }
    }

    let pressure_str = format!("{:.0}%", tb.budget_pressure * 100.0);
    let pressure_colored = if tb.budget_pressure > 0.9 {
        pressure_str.red().to_string()
    } else if tb.budget_pressure > 0.7 {
        pressure_str.yellow().to_string()
    } else {
        pressure_str.green().to_string()
    };
    eprintln!(
        "\n  {:<16} {} / {} ({}{})",
        "total:".bold().dim(),
        tb.total_used.to_string().cyan().bold(),
        tb.max_tokens.to_string().dim(),
        pressure_colored,
        if tb.compression_triggered {
            " compressed"
        } else {
            ""
        }
    );

    // Tool selection summary
    if !tools.tools_selected.is_empty() {
        eprintln!(
            "\n  {} {} tools via {} (conf={:.0}%)",
            "Tools:".bold(),
            tools.tools_selected.len(),
            tools.selection_strategy.clone().dim(),
            tools.selection_confidence * 100.0
        );
    }
    if trace.memory.candidates_considered > 0 {
        eprintln!(
            "\n  {} {} considered → {} selected ({} tok, {}ms)",
            "Memory:".bold(),
            trace.memory.candidates_considered,
            trace.memory.memories_selected.len(),
            trace.memory.total_tokens,
            trace.memory.retrieval_latency_ms
        );
    }

    eprintln!();
}

fn describe_context_pressure(
    usage_pct: f64,
    est_pressure: f64,
) -> (&'static str, &'static str, &'static str) {
    if est_pressure < PRESSURE_HEALTHY_THRESHOLD && usage_pct < USAGE_HEALTHY_PCT {
        (
            "🟢",
            "Healthy",
            "Plenty of room left. Short follow-ups like '继续' should work well.",
        )
    } else if est_pressure < PRESSURE_WARNING_THRESHOLD && usage_pct < USAGE_WARNING_PCT {
        (
            "🟡",
            "Getting full",
            "Still usable, but older turns may compact soon if the thread keeps growing.",
        )
    } else {
        (
            "🔴",
            "Near compaction",
            "You can continue, but expect older context to be summarized or compressed soon.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_grep_request_defaults_to_content_search() {
        assert_eq!(
            parse_grep_request("tool timeout").unwrap(),
            GrepRequest::Content("tool timeout".to_string())
        );
    }

    #[test]
    fn parse_grep_request_supports_files_mode() {
        assert_eq!(
            parse_grep_request("files Cargo.toml").unwrap(),
            GrepRequest::Files("Cargo.toml".to_string())
        );
    }

    #[test]
    fn parse_grep_request_supports_review_mode() {
        assert_eq!(
            parse_grep_request("review timeout").unwrap(),
            GrepRequest::Review("timeout".to_string())
        );
    }

    #[test]
    fn parse_grep_request_rejects_empty_args() {
        assert!(parse_grep_request("").is_err());
    }

    #[test]
    fn collect_changed_files_deduplicates_and_skips_blanks() {
        let files =
            collect_changed_files("src/main.rs\n", "src/main.rs\nsrc/lib.rs\n", "\nnew.rs\n");
        assert_eq!(files, vec!["src/main.rs", "src/lib.rs", "new.rs"]);
    }

    #[test]
    fn parse_review_match_extracts_file_line_and_text() {
        assert_eq!(
            parse_review_match("src/main.rs:42:timeout exceeded"),
            Some(ReviewMatch {
                path: "src/main.rs",
                line: "42",
                text: "timeout exceeded",
            })
        );
    }

    #[test]
    fn format_review_search_result_summarizes_grouped_hits() {
        let files = vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "tests/review.rs".to_string(),
        ];
        let formatted = format_review_search_result(
            &files,
            "src/main.rs:12:tool timeout\nsrc/main.rs:18:retry timeout\nsrc/lib.rs:7:timeout budget",
        );
        assert!(formatted.contains("Scope: 3 changed files"));
        assert!(formatted.contains("Matches: 3 hit(s) across 2 file(s)"));
        assert!(formatted.contains("\nsrc/main.rs\n"));
        assert!(formatted.contains("  12: tool timeout"));
        assert!(formatted.contains("\nsrc/lib.rs\n"));
    }

    #[test]
    fn format_review_search_result_guides_when_no_matches_found() {
        let files = vec!["src/main.rs".to_string(), "tests/review.rs".to_string()];
        let formatted = format_review_search_result(&files, "");
        assert!(formatted.contains("Scope: 2 changed files"));
        assert!(formatted.contains("No matches found in changed files"));
        assert!(formatted.contains("Tip: use /grep <pattern>"));
    }

    #[test]
    fn build_review_prompt_defaults_to_head() {
        let tmp = std::env::temp_dir();
        let prompt = super::build_review_prompt("", &tmp);
        assert!(prompt.contains("Review target: HEAD"));
        assert!(prompt.contains("git_show"));
        assert!(prompt.contains("Do NOT read entire files"));
        assert!(prompt.contains("Do not narrate your process"));
        assert!(prompt.contains("Pre-fetched"));
        assert!(prompt.contains("```text"));
    }

    #[test]
    fn build_review_prompt_supports_working_tree() {
        let tmp = std::env::temp_dir();
        let prompt = super::build_review_prompt("working", &tmp);
        assert!(prompt.contains("Review target: WORKING_TREE"));
        assert!(prompt.contains("git_diff"));
        assert!(prompt.contains("stat_only:true"));
        assert!(prompt.contains("Prefer `read_file`/`grep`/`glob` over `bash`"));
    }

    #[test]
    fn build_review_prompt_local_changes_maps_to_working_tree() {
        let tmp = std::env::temp_dir();
        let prompt = super::build_review_prompt("local changes", &tmp);
        assert!(prompt.contains("Review target: WORKING_TREE"));
    }

    #[test]
    fn parse_review_git_target_accepts_common_aliases() {
        use super::{ReviewGitTarget, parse_review_git_target};
        assert_eq!(parse_review_git_target(""), ReviewGitTarget::Head);
        assert_eq!(parse_review_git_target("latest"), ReviewGitTarget::Head);
        assert_eq!(
            parse_review_git_target("latest commit"),
            ReviewGitTarget::Head
        );
        assert_eq!(
            parse_review_git_target("last commit"),
            ReviewGitTarget::Head
        );
        assert_eq!(
            parse_review_git_target("local changes"),
            ReviewGitTarget::WorkingTree
        );
        assert_eq!(
            parse_review_git_target("LOCAL"),
            ReviewGitTarget::WorkingTree
        );
        assert_eq!(
            parse_review_git_target("abc123"),
            ReviewGitTarget::Rev("abc123")
        );
    }

    #[test]
    fn parse_continuation_anchor_extracts_task_and_direction() {
        let parsed = parse_continuation_anchor(
            "Latest user task: debug Chinese input drops\nLatest assistant direction: inspect prompt redraw path",
        );
        assert_eq!(parsed.task.as_deref(), Some("debug Chinese input drops"));
        assert_eq!(
            parsed.direction.as_deref(),
            Some("inspect prompt redraw path")
        );
    }

    #[test]
    fn parse_continuation_anchor_handles_task_only() {
        let parsed = parse_continuation_anchor("Latest user task: fix auth");
        assert_eq!(parsed.task.as_deref(), Some("fix auth"));
        assert_eq!(parsed.direction, None);
    }

    #[test]
    fn describe_context_pressure_reports_healthy() {
        let (_, label, hint) = describe_context_pressure(5.0, 0.1);
        assert_eq!(label, "Healthy");
        assert!(hint.contains("Plenty of room"));
    }

    #[test]
    fn describe_context_pressure_reports_near_compaction() {
        let (_, label, hint) = describe_context_pressure(90.0, 0.95);
        assert_eq!(label, "Near compaction");
        assert!(hint.contains("summarized or compressed"));
    }
}
