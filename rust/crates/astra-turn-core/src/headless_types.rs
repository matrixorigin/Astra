//! Shared headless tool types extracted from `agentic_headless_round`.
//!
//! These live in turn-core so that downstream modules (`headless_tool_body_preview`,
//! `headless_tool_pipeline`, etc.) can reference them without circular dependencies.

/// Styled terminal output categories for headless tool stderr rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessStderrStyle {
    Dim,
    Red,
    Green,
    Yellow,
    /// File / `diff --git` headers (terminal preview).
    CyanBold,
    Magenta,
    /// Unified diff `+` line (not `+++`).
    DiffAdd,
    /// Unified diff `-` line (not `---`).
    DiffRemove,
    /// Unified diff context (` `) and `\ No newline…` meta lines.
    DiffContext,
    /// Read file body / neutral code line.
    Normal,
}

/// Host sink for headless tool round stderr (noop when CLI passes [`NoopHeadlessTerminal`]).
pub trait HeadlessRoundTerminal: Send {
    fn emit_line(&mut self, style: HeadlessStderrStyle, line: String);
}

/// No-op implementation (e.g. `--quiet`).
pub struct NoopHeadlessTerminal;

impl HeadlessRoundTerminal for NoopHeadlessTerminal {
    fn emit_line(&mut self, _: HeadlessStderrStyle, _: String) {}
}
