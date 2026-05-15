//! Issue #326 P3 / R1 Major 6 / scenario #12: split compound shell
//! commands so the approval card can show each subcommand on its
//! own line.
//!
//! When the LLM proposes
//!
//! ```text
//! cd src && npm test && rm -rf dist
//! ```
//!
//! the approval card should render each step separately so the
//! user can see "wait, this also runs `rm -rf dist` after the
//! tests" instead of glossing over it. The risk classification
//! is the **maximum** across all subcommands — so a benign
//! `cd src` chained with `rm -rf` reads as catastrophic, not
//! as `cd`.
//!
//! ## Why an argv tokenizer, not a real shell parser
//!
//! Plan v3 P3 calls this an "argv tokenizer". A real shell
//! parser (POSIX bash) is huge and ambiguous; we don't need
//! that fidelity. We only need to:
//!
//! 1. Split on `;`, `&&`, `||`, `|` while respecting quoted
//!    strings. Process substitution / heredocs / subshells go
//!    into the catch-all "remainder" so they don't silently
//!    drop.
//! 2. Detect `$(...)` / backticks anywhere — those force the
//!    UI to mark the whole command as `Compound + dynamic
//!    eval` and disable the Always button (`make_allow_rule`
//!    refuses to persist a rule from a dynamic-eval command).
//!
//! The tokenizer is intentionally conservative: when it can't
//! parse cleanly, it returns the original command as a single
//! step. The UI gracefully degrades to "single line, no
//! split" rather than emitting a wrong split.

/// One step of a compound shell command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompoundStep {
    /// The text of this step, trimmed of leading whitespace.
    pub command: String,
    /// The separator that **follows** this step in the original
    /// command (`;`, `&&`, `||`, `|`). `None` for the final step.
    pub trailing_separator: Option<CompoundSeparator>,
}

/// Shell separator class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompoundSeparator {
    /// `;` — sequential, run regardless of previous result.
    Sequential,
    /// `&&` — run only if previous succeeded.
    AndThen,
    /// `||` — run only if previous failed.
    OrElse,
    /// `|` — pipe stdout of previous to stdin of this.
    Pipe,
}

/// Result of a tokenization attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompoundCommand {
    pub steps: Vec<CompoundStep>,
    /// True iff the command contains `$(...)` or backticks.
    /// The UI uses this to disable Always (we won't persist a
    /// rule from a dynamic-eval command — too dangerous).
    pub has_dynamic_eval: bool,
    /// True iff parsing was conservative (returned the whole
    /// command as one step instead of splitting).
    pub is_atomic_fallback: bool,
}

/// Split a shell command into compound steps.
///
/// Returns a single-step `CompoundCommand` for atomic commands
/// (no separators) and for inputs the tokenizer can't parse
/// cleanly. The latter case is signalled by
/// `is_atomic_fallback = true`.
#[must_use]
pub fn tokenize_compound_command(input: &str) -> CompoundCommand {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return CompoundCommand {
            steps: vec![CompoundStep {
                command: String::new(),
                trailing_separator: None,
            }],
            has_dynamic_eval: false,
            is_atomic_fallback: true,
        };
    }

    let has_dynamic_eval = detect_dynamic_eval(trimmed);

    // Walk the byte-string; collect splits at top-level separators.
    let bytes = trimmed.as_bytes();
    let mut splits: Vec<(usize, CompoundSeparator, usize)> = Vec::new();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut paren_depth: u32 = 0;
    let mut brace_depth: u32 = 0;

    while i < bytes.len() {
        let b = bytes[i];

        // Backslash escapes the next char (outside quotes).
        if b == b'\\' && i + 1 < bytes.len() && !in_single {
            i += 2;
            continue;
        }
        if !in_double && !in_backtick && b == b'\'' {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single && !in_backtick && b == b'"' {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single && !in_double && b == b'`' {
            in_backtick = !in_backtick;
            i += 1;
            continue;
        }
        if in_single || in_double || in_backtick {
            i += 1;
            continue;
        }
        if b == b'(' {
            paren_depth = paren_depth.saturating_add(1);
            i += 1;
            continue;
        }
        if b == b')' {
            paren_depth = paren_depth.saturating_sub(1);
            i += 1;
            continue;
        }
        if b == b'{' {
            brace_depth = brace_depth.saturating_add(1);
            i += 1;
            continue;
        }
        if b == b'}' {
            brace_depth = brace_depth.saturating_sub(1);
            i += 1;
            continue;
        }
        if paren_depth > 0 || brace_depth > 0 {
            i += 1;
            continue;
        }

        // Top-level: detect operator.
        if b == b'&' && i + 1 < bytes.len() && bytes[i + 1] == b'&' {
            splits.push((i, CompoundSeparator::AndThen, i + 2));
            i += 2;
            continue;
        }
        if b == b'|' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            splits.push((i, CompoundSeparator::OrElse, i + 2));
            i += 2;
            continue;
        }
        if b == b'|' {
            // Single | is a pipe; ignore "|>", "|&" etc.
            splits.push((i, CompoundSeparator::Pipe, i + 1));
            i += 1;
            continue;
        }
        if b == b';' {
            splits.push((i, CompoundSeparator::Sequential, i + 1));
            i += 1;
            continue;
        }
        i += 1;
    }

    // Quote / paren mismatch → atomic fallback.
    if in_single || in_double || in_backtick || paren_depth != 0 || brace_depth != 0 {
        return CompoundCommand {
            steps: vec![CompoundStep {
                command: trimmed.to_string(),
                trailing_separator: None,
            }],
            has_dynamic_eval,
            is_atomic_fallback: true,
        };
    }

    if splits.is_empty() {
        return CompoundCommand {
            steps: vec![CompoundStep {
                command: trimmed.to_string(),
                trailing_separator: None,
            }],
            has_dynamic_eval,
            is_atomic_fallback: false,
        };
    }

    // Materialize the steps.
    let mut steps = Vec::with_capacity(splits.len() + 1);
    let mut start = 0usize;
    for (op_start, sep, op_end) in &splits {
        let cmd = trimmed[start..*op_start].trim().to_string();
        steps.push(CompoundStep {
            command: cmd,
            trailing_separator: Some(*sep),
        });
        start = *op_end;
    }
    let last = trimmed[start..].trim().to_string();
    steps.push(CompoundStep {
        command: last,
        trailing_separator: None,
    });
    // Filter out fully-empty steps (e.g. trailing `;`).
    steps.retain(|s| !s.command.is_empty());
    if steps.is_empty() {
        steps.push(CompoundStep {
            command: trimmed.to_string(),
            trailing_separator: None,
        });
    }

    CompoundCommand {
        steps,
        has_dynamic_eval,
        is_atomic_fallback: false,
    }
}

fn detect_dynamic_eval(input: &str) -> bool {
    // Outside-of-quotes detection of $(…) and `…`. We accept some
    // false-positives — single quote tracking in shell is itself
    // subtle. Conservative is fine: the consequence of "false
    // positive" is "Always button is disabled", which the user can
    // override by saving a rule manually.
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() && !in_single {
            i += 2;
            continue;
        }
        if b == b'\'' {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single {
            if b == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'(' {
                return true;
            }
            if b == b'`' {
                return true;
            }
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmds(c: &CompoundCommand) -> Vec<String> {
        c.steps.iter().map(|s| s.command.clone()).collect()
    }

    // ── Atomic / no-split cases ────────────────────────────────

    #[test]
    fn atomic_command_returns_single_step() {
        let c = tokenize_compound_command("npm test");
        assert_eq!(cmds(&c), vec!["npm test"]);
        assert!(!c.is_atomic_fallback);
        assert!(!c.has_dynamic_eval);
    }

    #[test]
    fn empty_input_returns_empty_step() {
        let c = tokenize_compound_command("");
        assert_eq!(c.steps.len(), 1);
        assert_eq!(c.steps[0].command, "");
        assert!(c.is_atomic_fallback);
    }

    // ── Compound splits (scenario #12) ─────────────────────────

    #[test]
    fn split_on_and_then() {
        let c = tokenize_compound_command("cd src && npm test && rm -rf dist");
        assert_eq!(cmds(&c), vec!["cd src", "npm test", "rm -rf dist"]);
        assert_eq!(
            c.steps[0].trailing_separator,
            Some(CompoundSeparator::AndThen)
        );
        assert_eq!(
            c.steps[1].trailing_separator,
            Some(CompoundSeparator::AndThen)
        );
        assert_eq!(c.steps[2].trailing_separator, None);
    }

    #[test]
    fn split_on_sequential() {
        let c = tokenize_compound_command("ls; pwd; whoami");
        assert_eq!(cmds(&c), vec!["ls", "pwd", "whoami"]);
    }

    #[test]
    fn split_on_or_else() {
        let c = tokenize_compound_command("test -f file || echo missing");
        assert_eq!(cmds(&c), vec!["test -f file", "echo missing"]);
        assert_eq!(
            c.steps[0].trailing_separator,
            Some(CompoundSeparator::OrElse)
        );
    }

    #[test]
    fn split_on_pipe() {
        let c = tokenize_compound_command("cat log | grep ERROR | head");
        assert_eq!(cmds(&c), vec!["cat log", "grep ERROR", "head"]);
    }

    #[test]
    fn mixed_separators() {
        let c = tokenize_compound_command("ls && pwd ; whoami | head");
        assert_eq!(cmds(&c), vec!["ls", "pwd", "whoami", "head"]);
    }

    // ── Quote-aware: separators inside quotes don't split ──────

    #[test]
    fn separator_inside_double_quotes_is_not_split() {
        let c = tokenize_compound_command(r#"echo "a && b" && pwd"#);
        assert_eq!(cmds(&c), vec![r#"echo "a && b""#, "pwd"]);
    }

    #[test]
    fn separator_inside_single_quotes_is_not_split() {
        let c = tokenize_compound_command(r#"echo 'a; b' ; pwd"#);
        assert_eq!(cmds(&c), vec!["echo 'a; b'", "pwd"]);
    }

    #[test]
    fn escaped_separator_is_not_split() {
        let c = tokenize_compound_command(r#"echo a\&\&b && pwd"#);
        assert_eq!(cmds(&c), vec![r#"echo a\&\&b"#, "pwd"]);
    }

    // ── Subshell / brace / paren grouping is preserved ─────────

    #[test]
    fn subshell_is_atomic() {
        let c = tokenize_compound_command("(cd /tmp && ls) && pwd");
        assert_eq!(cmds(&c), vec!["(cd /tmp && ls)", "pwd"]);
    }

    // ── Dynamic-eval detection ─────────────────────────────────

    #[test]
    fn detects_command_substitution() {
        let c = tokenize_compound_command("export FOO=$(git rev-parse HEAD)");
        assert!(c.has_dynamic_eval);
    }

    #[test]
    fn detects_backtick_substitution() {
        let c = tokenize_compound_command("echo `pwd`");
        assert!(c.has_dynamic_eval);
    }

    #[test]
    fn dollar_paren_inside_single_quotes_is_not_dynamic_eval() {
        let c = tokenize_compound_command(r#"echo 'literal $(stuff)' "#);
        assert!(!c.has_dynamic_eval);
    }

    // ── Fallback path ──────────────────────────────────────────

    #[test]
    fn unbalanced_quote_falls_back_to_atomic() {
        let c = tokenize_compound_command("echo 'broken && pwd");
        assert!(c.is_atomic_fallback);
        assert_eq!(c.steps.len(), 1);
    }

    #[test]
    fn unbalanced_paren_falls_back_to_atomic() {
        let c = tokenize_compound_command("(cd /tmp && pwd");
        assert!(c.is_atomic_fallback);
    }

    // ── Trailing separator handling ───────────────────────────

    #[test]
    fn trailing_separator_is_dropped() {
        let c = tokenize_compound_command("ls;");
        assert_eq!(cmds(&c), vec!["ls"]);
    }
}
