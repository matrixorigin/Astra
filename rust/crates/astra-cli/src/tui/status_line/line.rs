//! Pure status-line composition.

#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

/// Permission mode expressed as an enum rather than a string so the
/// status line can't silently render typo'd values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PermissionMode {
    #[default]
    Ask,
    Auto,
    Deny,
    Bypass,
}

impl PermissionMode {
    pub fn chip_text(self) -> &'static str {
        match self {
            Self::Ask => "",
            Self::Auto => "⚡auto",
            Self::Deny => "⚡deny",
            Self::Bypass => "⚡bypass",
        }
    }
}

/// Inputs that feed the status line. Owned so the struct is `Clone`
/// and easy to fixture.
#[derive(Debug, Clone, Default)]
pub(crate) struct StatusContext {
    pub model: Option<String>,
    pub cwd: Option<String>,
    /// Tuple `(used, limit)` in tokens.
    pub token_budget: Option<(u64, u64)>,
    pub permission_mode: PermissionMode,
    pub turn_active: bool,
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
    pub git_branch: Option<String>,
    /// Number of approvals currently awaiting a user decision.
    pub pending_approvals: usize,
}

/// A styled text fragment that appears on either side of the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment {
    pub text: String,
    pub style: Style,
}

impl Segment {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// Composed status line. `left` / `right` hold the segments; a caller
/// renders them in their preferred layout (or uses [`StatusLine::plain`]
/// for a single left-joined-right-joined string).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StatusLine {
    pub left: Vec<Segment>,
    pub right: Vec<Segment>,
}

/// Threshold below which the budget chip is dim, above which it warns.
const BUDGET_WARN_PERCENT: f32 = 75.0;
const BUDGET_ERROR_PERCENT: f32 = 90.0;
/// Max cwd segment width before the middle is elided.
const CWD_MAX_WIDTH: usize = 28;

impl StatusLine {
    /// Build a status line from context.
    pub fn from_context(ctx: &StatusContext) -> Self {
        let dim = Style::default().fg(Color::DarkGray);
        let mut out = Self::default();

        // ── Left: short hint, then permission chip if non-default ─
        let hint_text = if ctx.turn_active {
            "Ctrl+C interrupt".to_string()
        } else {
            "/ @ $ · Ctrl+O transcript".to_string()
        };
        out.left.push(Segment::styled(hint_text, dim));

        match ctx.permission_mode {
            PermissionMode::Ask => {}
            PermissionMode::Auto => {
                out.left.push(Segment::styled(
                    PermissionMode::Auto.chip_text(),
                    Style::default().fg(Color::Yellow),
                ));
            }
            PermissionMode::Deny => {
                out.left.push(Segment::styled(
                    PermissionMode::Deny.chip_text(),
                    Style::default().fg(Color::Red),
                ));
            }
            PermissionMode::Bypass => {
                out.left.push(Segment::styled(
                    PermissionMode::Bypass.chip_text(),
                    Style::default().fg(Color::Red),
                ));
            }
        }

        if ctx.pending_approvals > 0 {
            let text = if ctx.pending_approvals == 1 {
                "⏸ 1 pending".to_string()
            } else {
                format!("⏸ {} pending", ctx.pending_approvals)
            };
            out.left.push(Segment::styled(
                text,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ));
        }

        // ── Right: model · branch · cwd · tokens · cost ───────────
        if let Some(model) = ctx.model.as_deref() {
            out.right.push(Segment::styled(model.to_string(), dim));
        }

        if let Some(branch) = ctx.git_branch.as_deref() {
            out.right
                .push(Segment::styled(format!("⎇ {branch}"), dim));
        }

        if let Some(cwd) = ctx.cwd.as_deref() {
            out.right
                .push(Segment::styled(truncate_cwd(cwd, CWD_MAX_WIDTH), dim));
        }

        if let Some((used, limit)) = ctx.token_budget {
            if limit > 0 {
                let pct = (used as f32 / limit as f32) * 100.0;
                let style = if pct >= BUDGET_ERROR_PERCENT {
                    Style::default().fg(Color::Red)
                } else if pct >= BUDGET_WARN_PERCENT {
                    Style::default().fg(Color::Yellow)
                } else {
                    dim
                };
                out.right.push(Segment::styled(
                    format!("{pct:.0}% ({})", format_tokens_compact(used)),
                    style,
                ));
            }
        }

        if let Some(cost) = ctx.cost_usd {
            out.right
                .push(Segment::styled(format!("${cost:.2}"), dim));
        }

        out
    }

    /// Render to a simple string for testing: left joined by ' · ',
    /// two-space gap, right joined by ' · '.
    pub fn plain(&self) -> String {
        let left: Vec<&str> = self.left.iter().map(|s| s.text.as_str()).collect();
        let right: Vec<&str> = self.right.iter().map(|s| s.text.as_str()).collect();
        let l = left.join(" · ");
        let r = right.join(" · ");
        if l.is_empty() {
            r
        } else if r.is_empty() {
            l
        } else {
            format!("{l}  {r}")
        }
    }

    /// Draw into `area` of `buf`. Left side sticks to the left edge; right
    /// side is right-aligned. When the terminal is too narrow for both,
    /// right-side segments are dropped one at a time (tail first) until
    /// the line fits.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let sep = Span::styled(" · ", Style::default().fg(Color::DarkGray));

        let left_spans = join_segments(&self.left, &sep, 2 /* leading indent */);
        let mut right_segments: &[Segment] = &self.right;
        let mut right_spans;
        let left_w: usize = left_spans.iter().map(|s| s.content.width()).sum();

        loop {
            right_spans = join_segments(right_segments, &sep, 0);
            let right_w: usize = right_spans.iter().map(|s| s.content.width()).sum();
            let total = left_w + right_w + 2; // 2-char trailing margin
            if total <= area.width as usize || right_segments.is_empty() {
                break;
            }
            // Drop the trailing right segment and retry.
            right_segments = &right_segments[..right_segments.len() - 1];
        }

        let right_w: usize = right_spans.iter().map(|s| s.content.width()).sum();
        let padding = (area.width as usize)
            .saturating_sub(left_w + right_w + 2);

        let mut all = left_spans;
        all.push(Span::raw(" ".repeat(padding)));
        all.extend(right_spans);

        Widget::render(Line::from(all), area, buf);
    }
}

fn join_segments(segments: &[Segment], sep: &Span<'_>, leading_spaces: usize) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(segments.len() * 2 + 1);
    if leading_spaces > 0 {
        spans.push(Span::raw(" ".repeat(leading_spaces)));
    }
    for (i, seg) in segments.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                sep.content.to_string(),
                sep.style,
            ));
        }
        spans.push(Span::styled(seg.text.clone(), seg.style));
    }
    spans
}

/// Shorten `cwd` to at most `max_width` characters by replacing the
/// middle with an ellipsis, keeping the meaningful tail visible.
fn truncate_cwd(cwd: &str, max_width: usize) -> String {
    let count = cwd.chars().count();
    if count <= max_width {
        return cwd.to_string();
    }
    // Keep the last `max_width - 1` characters, prefixed with "…".
    let tail: String = cwd.chars().skip(count - (max_width - 1)).collect();
    format!("…{tail}")
}

/// "25000" → "25k"; preserves exact count under 1k.
fn format_tokens_compact(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{}k", n / 1_000)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}
