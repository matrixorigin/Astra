//! Git diff presentation for the REPL — terminal renderer today, IDE sink later.
//!
//! Design: [`DiffSink`] is the extension point. [`TerminalDiffSink`] prints colored
//! unified diffs (claudecode-style). A future `IdeDiffSink` can reuse
//! [`summarize_unified_diff`] / parsed hunks for MCP / editor RPC.

use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use crossterm::style::Stylize;

/// Max bytes read from `git diff` / `git show` (avoid OOM on huge trees).
const MAX_DIFF_BYTES: usize = 2_000_000;
/// Stop rendering after this many lines (tail message points to narrowed `/diff -- <path>`).
const MAX_RENDER_LINES: usize = 8_000;

/// Compare working tree / index against commits (default matches common IDE “vs HEAD”).
#[derive(Clone, Debug)]
pub(crate) enum DiffScope {
    /// `git diff HEAD` — all local changes vs last commit (staged + unstaged).
    VsHead,
    /// `git diff --cached` — staged vs `HEAD`.
    Staged,
    /// `git diff` — unstaged only.
    Unstaged,
    /// `git show <rev>` — single commit patch.
    Show { rev: String },
    /// `git diff --stat HEAD` — file list + counts only.
    StatHead,
}

/// Cheap stats for a banner line (claudecode-style summary).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DiffStats {
    pub files: usize,
    pub lines_added: usize,
    pub lines_removed: usize,
}

/// Receives structured diff output — implement for terminal now, IDE extension later.
pub(crate) trait DiffSink {
    fn header(&mut self, line: &str);
    fn stats_line(&mut self, stats: DiffStats);
    fn meta(&mut self, line: &str);
    fn hunk_header(&mut self, line: &str);
    fn add_line(&mut self, line: &str);
    fn del_line(&mut self, line: &str);
    fn context_line(&mut self, line: &str);
}

/// Colored unified diff for the terminal.
pub(crate) struct TerminalDiffSink {
    pub width: usize,
    lines_out: usize,
    truncated: bool,
}

impl TerminalDiffSink {
    pub fn new(width: usize) -> Self {
        Self {
            width: width.max(40),
            lines_out: 0,
            truncated: false,
        }
    }

    fn clip(&self, s: &str) -> String {
        let w = self.width.saturating_sub(1);
        if s.chars().count() <= w {
            return s.to_string();
        }
        let mut out = String::new();
        let mut n = 0usize;
        for ch in s.chars() {
            if n + 1 >= w {
                out.push('…');
                break;
            }
            out.push(ch);
            n += 1;
        }
        out
    }

    fn bump(&mut self) -> bool {
        self.lines_out += 1;
        if self.lines_out > MAX_RENDER_LINES {
            self.truncated = true;
            return false;
        }
        true
    }
}

impl DiffSink for TerminalDiffSink {
    fn header(&mut self, line: &str) {
        if !self.bump() {
            return;
        }
        let _ = writeln!(io::stdout(), "{}", self.clip(line).cyan().bold());
    }

    fn stats_line(&mut self, stats: DiffStats) {
        if !self.bump() {
            return;
        }
        let s = format!(
            "  {} file(s)  +{}  -{}",
            stats.files, stats.lines_added, stats.lines_removed
        );
        let _ = writeln!(io::stdout(), "{}", s.green());
    }

    fn meta(&mut self, line: &str) {
        if !self.bump() {
            return;
        }
        let _ = writeln!(io::stdout(), "{}", self.clip(line).dim());
    }

    fn hunk_header(&mut self, line: &str) {
        if !self.bump() {
            return;
        }
        let _ = writeln!(io::stdout(), "{}", self.clip(line).magenta());
    }

    fn add_line(&mut self, line: &str) {
        if !self.bump() {
            return;
        }
        let _ = writeln!(io::stdout(), "{}", self.clip(line).green());
    }

    fn del_line(&mut self, line: &str) {
        if !self.bump() {
            return;
        }
        let _ = writeln!(io::stdout(), "{}", self.clip(line).red());
    }

    fn context_line(&mut self, line: &str) {
        if !self.bump() {
            return;
        }
        let _ = writeln!(io::stdout(), "{}", self.clip(line).dim());
    }
}

impl TerminalDiffSink {
    pub fn finish(&mut self) {
        if self.truncated {
            let _ = writeln!(
                io::stdout(),
                "{}",
                format!(
                    "  … output truncated (>{MAX_RENDER_LINES} lines). Narrow with: /diff <path>"
                )
                .yellow()
            );
        }
        let _ = io::stdout().flush();
    }
}

/// Scan unified diff text for `diff --git` and +/- line counts.
pub(crate) fn summarize_unified_diff(text: &str) -> DiffStats {
    let mut stats = DiffStats::default();
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            stats.files += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            if rest.starts_with('+') || rest.starts_with("++") {
                continue;
            }
            stats.lines_added += 1;
        } else if let Some(rest) = line.strip_prefix('-') {
            if rest.starts_with('-') || rest.starts_with("-- ") {
                continue;
            }
            stats.lines_removed += 1;
        }
    }
    stats
}

/// Classify and forward one line of unified diff.
pub(crate) fn render_diff_line(sink: &mut impl DiffSink, line: &str) {
    if line.starts_with("diff --git ") {
        sink.header(line);
        return;
    }
    if line.starts_with("index ")
        || line.starts_with("new file mode")
        || line.starts_with("deleted file mode")
        || line.starts_with("similarity index")
        || line.starts_with("rename from")
        || line.starts_with("rename to")
        || line.starts_with("Binary files ")
    {
        sink.meta(line);
        return;
    }
    if line.starts_with("--- ") || line.starts_with("+++ ") {
        sink.meta(line);
        return;
    }
    if line.starts_with("@@") {
        sink.hunk_header(line);
        return;
    }
    if let Some(body) = line.strip_prefix('+') {
        sink.add_line(&format!("+{body}"));
        return;
    }
    if let Some(body) = line.strip_prefix('-') {
        sink.del_line(&format!("-{body}"));
        return;
    }
    if line.starts_with('\\') {
        sink.meta(line);
        return;
    }
    sink.context_line(line);
}

fn is_git_repo(root: &Path) -> bool {
    Command::new("git")
        .args(["-C", root.to_str().unwrap_or("."), "rev-parse", "--git-dir"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run git in `repo_root`; returns stdout UTF-8 or error message.
fn git_output(repo_root: &Path, args: &[String]) -> Result<String, String> {
    let out = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .map_err(|e| format!("git failed to start: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = err.trim();
        if msg.is_empty() {
            return Err("git exited with an error".into());
        }
        return Err(msg.to_string());
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if stdout.len() > MAX_DIFF_BYTES {
        return Err(format!(
            "diff output exceeds {} MiB — narrow with a path",
            MAX_DIFF_BYTES / 1_000_000
        ));
    }
    Ok(stdout)
}

/// Fetch raw unified diff text for the given scope and path filters.
pub(crate) fn fetch_git_diff_text(
    repo_root: &Path,
    scope: DiffScope,
    paths: &[String],
) -> Result<String, String> {
    if !is_git_repo(repo_root) {
        return Err("not a git repository (or git not installed)".into());
    }

    let mut args: Vec<String> = vec![
        "-c".into(),
        "core.pager=".into(),
        "diff".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
    ];

    match &scope {
        DiffScope::VsHead => args.push("HEAD".into()),
        DiffScope::Staged => args.push("--cached".into()),
        DiffScope::Unstaged => {}
        DiffScope::Show { .. } | DiffScope::StatHead => {
            return Err("internal: use fetch_git_show_patch or fetch_git_diff_stat_head".into());
        }
    }

    if !paths.is_empty() {
        args.push("--".into());
        for p in paths {
            args.push(p.clone());
        }
    }

    git_output(repo_root, &args)
}

pub(crate) fn fetch_git_show_patch(repo_root: &Path, rev: &str) -> Result<String, String> {
    if !is_git_repo(repo_root) {
        return Err("not a git repository (or git not installed)".into());
    }
    let args = vec![
        "-c".into(),
        "core.pager=".into(),
        "show".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        "-p".into(),
        rev.to_string(),
    ];
    git_output(repo_root, &args)
}

pub(crate) fn fetch_git_diff_stat_head(repo_root: &Path, paths: &[String]) -> Result<String, String> {
    if !is_git_repo(repo_root) {
        return Err("not a git repository (or git not installed)".into());
    }
    let mut args = vec![
        "-c".into(),
        "core.pager=".into(),
        "diff".into(),
        "--no-color".into(),
        "HEAD".into(),
        "--stat".into(),
        "--stat-width=100".into(),
    ];
    if !paths.is_empty() {
        args.push("--".into());
        for p in paths {
            args.push(p.clone());
        }
    }
    git_output(repo_root, &args)
}

/// Parse `/diff` arguments: optional mode word then path list.
pub(crate) fn parse_diff_args(arg: &str) -> (DiffScope, Vec<String>) {
    let tokens: Vec<&str> = arg.split_whitespace().collect();
    if tokens.is_empty() {
        return (DiffScope::VsHead, vec![]);
    }
    match tokens[0] {
        "staged" => (DiffScope::Staged, tokens[1..].iter().map(|s| s.to_string()).collect()),
        "unstaged" | "patch" => (DiffScope::Unstaged, tokens[1..].iter().map(|s| s.to_string()).collect()),
        "stat" => (DiffScope::StatHead, tokens[1..].iter().map(|s| s.to_string()).collect()),
        "show" => {
            let rev = tokens.get(1).map(|s| s.to_string()).unwrap_or_default();
            let paths = if tokens.len() > 2 {
                tokens[2..].iter().map(|s| s.to_string()).collect()
            } else {
                vec![]
            };
            (DiffScope::Show { rev }, paths)
        }
        _ => (DiffScope::VsHead, tokens.iter().map(|s| s.to_string()).collect()),
    }
}

/// Entry: print diff for cwd. Uses `git` binary for true unified diffs (line-accurate review).
pub(crate) fn run_diff_command(repo_root: &Path, arg: &str, term_width: usize) {
    let t = arg.trim();
    if matches!(
        t.split_whitespace().next(),
        Some("help" | "-h" | "--help")
    ) {
        print_diff_usage();
        return;
    }

    let (scope, paths) = parse_diff_args(arg);

    eprintln!();
    match scope {
        DiffScope::Show { rev } if rev.is_empty() => {
            eprintln!("{}", "  Usage: /diff show <rev> [paths…]".yellow());
            return;
        }
        DiffScope::Show { rev } => {
            eprintln!(
                "{}",
                format!("  ─── git show {rev} ───").dim()
            );
            match fetch_git_show_patch(repo_root, &rev) {
                Ok(text) => {
                    let stats = summarize_unified_diff(&text);
                    let mut sink = TerminalDiffSink::new(term_width);
                    sink.stats_line(stats);
                    for line in text.lines() {
                        render_diff_line(&mut sink, line);
                    }
                    sink.finish();
                }
                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
            }
            return;
        }
        DiffScope::StatHead => {
            eprintln!("{}", "  ─── git diff HEAD --stat ───".dim());
            match fetch_git_diff_stat_head(repo_root, &paths) {
                Ok(text) => {
                    if text.trim().is_empty() {
                        eprintln!("{}", "  No changes.".dim());
                    } else {
                        for line in text.lines() {
                            let _ = writeln!(io::stdout(), "{}", line);
                        }
                        let _ = io::stdout().flush();
                    }
                }
                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
            }
            return;
        }
        _ => {
            let label = match scope {
                DiffScope::VsHead => "git diff HEAD",
                DiffScope::Staged => "git diff --cached",
                DiffScope::Unstaged => "git diff",
                _ => "git diff",
            };
            eprintln!("{}", format!("  ─── {label} ───").dim());
            match fetch_git_diff_text(repo_root, scope, &paths) {
                Ok(text) => {
                    let trimmed = text.trim_end();
                    if trimmed.is_empty()
                        || trimmed == "No changes"
                        || trimmed == "No staged changes"
                    {
                        eprintln!("{}", "  No changes.".dim());
                        return;
                    }
                    let stats = summarize_unified_diff(&text);
                    let mut sink = TerminalDiffSink::new(term_width);
                    sink.stats_line(stats);
                    for line in text.lines() {
                        render_diff_line(&mut sink, line);
                    }
                    sink.finish();
                }
                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
            }
        }
    }
}

fn print_diff_usage() {
    eprintln!();
    eprintln!("{}", "  /diff — review changes (unified diff, colored)".cyan().bold());
    eprintln!(
        "{}",
        "  /diff              all local changes vs HEAD (staged + unstaged)".dim()
    );
    eprintln!("{}", "  /diff staged       staged vs HEAD".dim());
    eprintln!("{}", "  /diff unstaged     unstaged only".dim());
    eprintln!("{}", "  /diff stat         short file list + line counts".dim());
    eprintln!("{}", "  /diff show <rev>   single-commit patch".dim());
    eprintln!(
        "{}",
        "  /diff <path> …     limit to paths (any mode: put after keywords)".dim()
    );
    eprintln!(
        "{}",
        "  IDE: diff rendering is pluggable (see diff_presenter::DiffSink).".dim()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CollectSink(Vec<String>);

    impl DiffSink for CollectSink {
        fn header(&mut self, line: &str) {
            self.0.push(format!("H:{line}"));
        }
        fn stats_line(&mut self, stats: DiffStats) {
            self.0.push(format!("S:{}+{}-{}", stats.files, stats.lines_added, stats.lines_removed));
        }
        fn meta(&mut self, line: &str) {
            self.0.push(format!("M:{line}"));
        }
        fn hunk_header(&mut self, line: &str) {
            self.0.push(format!("@:{line}"));
        }
        fn add_line(&mut self, line: &str) {
            self.0.push(format!("A:{line}"));
        }
        fn del_line(&mut self, line: &str) {
            self.0.push(format!("D:{line}"));
        }
        fn context_line(&mut self, line: &str) {
            self.0.push(format!("C:{line}"));
        }
    }

    #[test]
    fn summarize_counts_files_and_lines() {
        let u = r"diff --git a/x b/x
--- a/x
+++ b/x
@@ -1 +1 @@
-old
+new
diff --git a/y b/y
--- a/y
+++ b/y
+a
+b
";
        let s = summarize_unified_diff(u);
        assert_eq!(s.files, 2);
        assert_eq!(s.lines_added, 3);
        assert_eq!(s.lines_removed, 1);
    }

    #[test]
    fn parse_diff_args_modes() {
        let (sc, p) = parse_diff_args("");
        assert!(matches!(sc, DiffScope::VsHead));
        assert!(p.is_empty());

        let (sc, p) = parse_diff_args("staged foo.rs");
        assert!(matches!(sc, DiffScope::Staged));
        assert_eq!(p, vec!["foo.rs"]);

        let (sc, p) = parse_diff_args("show abc123");
        assert!(matches!(sc, DiffScope::Show { rev } if rev == "abc123"));
        assert!(p.is_empty());
    }

    #[test]
    fn render_diff_line_routing() {
        let mut c = CollectSink(vec![]);
        render_diff_line(&mut c, "diff --git a/f b/f");
        render_diff_line(&mut c, "--- a/f");
        render_diff_line(&mut c, "+++ b/f");
        render_diff_line(&mut c, "@@ -1 +1 @@");
        render_diff_line(&mut c, "-x");
        render_diff_line(&mut c, "+y");
        render_diff_line(&mut c, " context");
        assert!(c.0.iter().any(|l| l.starts_with("H:")));
        assert!(c.0.iter().any(|l| l.starts_with("A:+y")));
        assert!(c.0.iter().any(|l| l.starts_with("D:-x")));
    }
}
