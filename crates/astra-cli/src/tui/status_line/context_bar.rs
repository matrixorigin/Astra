//! Context-window capacity rail rendered directly above the composer.
//!
//! The rail is deliberately a separate visual channel from the status line:
//! capacity is continuous data, so position and colour communicate it more
//! efficiently than another textual chip in an already crowded footer.

use astra_turn_types::{ContextWindowUsage, ContextWindowUsageSource};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

const SIDE_PADDING: u16 = 2;
const LABEL_GAP: u16 = 1;
const WARN_PERCENT: u64 = 75;
const ERROR_PERCENT: u64 = 90;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ContextBar {
    usage: Option<ContextWindowUsage>,
    is_previous: bool,
}

impl ContextBar {
    pub(crate) fn new(usage: Option<ContextWindowUsage>, is_previous: bool) -> Self {
        Self { usage, is_previous }
    }

    pub(crate) fn render(self, area: Rect, buf: &mut Buffer) {
        let Some(usage) = self.usage.filter(|usage| usage.limit_tokens > 0) else {
            return;
        };
        if area.height == 0 || area.width <= SIDE_PADDING.saturating_mul(2) {
            return;
        }

        let theme = crate::tui::theme::current();
        let inner_x = area.x.saturating_add(SIDE_PADDING);
        let inner_width = area.width.saturating_sub(SIDE_PADDING.saturating_mul(2));
        let label = context_label(usage);
        let label_width = u16::try_from(label.width()).unwrap_or(u16::MAX);

        // On genuinely narrow terminals, capacity is still useful as text.
        // Giving the label the row is clearer than squeezing in a decorative
        // one- or two-cell rail that cannot communicate a meaningful ratio.
        if label_width.saturating_add(LABEL_GAP) >= inner_width {
            let compact = compact_label(usage, usize::from(inner_width));
            let compact_width = u16::try_from(compact.width()).unwrap_or(inner_width);
            let x = inner_x.saturating_add(inner_width.saturating_sub(compact_width));
            buf.set_string(x, area.y, compact, label_style(usage, self.is_previous));
            return;
        }

        let rail_width = inner_width.saturating_sub(label_width + LABEL_GAP);
        let filled = filled_cells(usage.used_tokens, usage.limit_tokens, rail_width);
        let fill_style = evidence_style(
            Style::default().fg(capacity_color(usage)),
            usage,
            self.is_previous,
        );
        let empty_style = Style::default().fg(theme.dim).add_modifier(Modifier::DIM);
        if filled > 0 {
            buf.set_string(inner_x, area.y, "─".repeat(usize::from(filled)), fill_style);
        }
        if rail_width > filled {
            buf.set_string(
                inner_x.saturating_add(filled),
                area.y,
                "─".repeat(usize::from(rail_width - filled)),
                empty_style,
            );
        }
        buf.set_string(
            inner_x.saturating_add(rail_width + LABEL_GAP),
            area.y,
            label,
            label_style(usage, self.is_previous),
        );
    }
}

fn filled_cells(used: u64, limit: u64, width: u16) -> u16 {
    if used == 0 || limit == 0 || width == 0 {
        return 0;
    }
    let used = u128::from(used.min(limit));
    let limit = u128::from(limit);
    let width = u128::from(width);
    // Round to the nearest cell. A non-zero value gets one visible cell so a
    // large window does not make early usage look exactly empty.
    let rounded = ((used * width) + (limit / 2)) / limit;
    u16::try_from(rounded.max(1).min(width)).unwrap_or(u16::MAX)
}

fn capacity_color(usage: ContextWindowUsage) -> Color {
    let theme = crate::tui::theme::current();
    let used = u128::from(usage.used_tokens);
    let limit = u128::from(usage.limit_tokens.max(1));
    if used.saturating_mul(100) >= limit.saturating_mul(u128::from(ERROR_PERCENT)) {
        theme.error
    } else if used.saturating_mul(100) >= limit.saturating_mul(u128::from(WARN_PERCENT)) {
        theme.warn
    } else {
        // A calm teal rail reads as ambient capacity, not primary action.
        // Bright accent remains reserved for focus and navigation.
        theme.quote
    }
}

fn label_style(usage: ContextWindowUsage, is_previous: bool) -> Style {
    let theme = crate::tui::theme::current();
    let style = if capacity_color(usage) == theme.quote {
        Style::default().fg(theme.dim)
    } else {
        Style::default().fg(capacity_color(usage))
    };
    evidence_style(style, usage, is_previous)
}

fn context_label(usage: ContextWindowUsage) -> String {
    format!(
        "{} / {}",
        format_tokens_compact(usage.used_tokens),
        format_tokens_compact(usage.limit_tokens)
    )
}

fn evidence_style(mut style: Style, usage: ContextWindowUsage, is_previous: bool) -> Style {
    if is_previous || matches!(usage.source, ContextWindowUsageSource::Estimated) {
        style = style.add_modifier(Modifier::DIM);
    }
    style
}

fn compact_label(usage: ContextWindowUsage, max_width: usize) -> String {
    let label = context_label(usage);
    if label.width() <= max_width {
        return label;
    }
    let compact = label.replace(" / ", "/");
    if compact.width() <= max_width {
        return compact;
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let mut out = compact.chars().take(max_width - 1).collect::<String>();
    out.push('…');
    out
}

/// "25000" → "25k"; preserves exact counts below 1k.
fn format_tokens_compact(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        let rounded_k = (tokens + 500) / 1_000;
        if rounded_k >= 1_000 {
            "1.0M".to_string()
        } else {
            format!("{rounded_k}k")
        }
    } else {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(usage: ContextWindowUsage, width: u16) -> Buffer {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        ContextBar::new(Some(usage), false).render(area, &mut buf);
        buf
    }

    fn text(buf: &Buffer) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol())
            .collect::<String>()
    }

    #[test]
    fn rail_uses_the_actionable_input_limit_without_internal_breakdown() {
        let buf = render(ContextWindowUsage::provider_reported(95_000, 800_000), 64);
        let rendered = text(&buf);
        assert!(rendered.contains("95k / 800k"), "{rendered:?}");
        assert!(!rendered.contains("Ctx"), "{rendered:?}");
        assert!(!rendered.contains("usable"), "{rendered:?}");
    }

    #[test]
    fn fill_is_proportional_and_clamped_to_the_rail() {
        assert_eq!(filled_cells(25, 100, 40), 10);
        assert_eq!(filled_cells(0, 100, 40), 0);
        assert_eq!(filled_cells(150, 100, 40), 40);
    }

    #[test]
    fn capacity_thresholds_use_semantic_theme_colours() {
        let theme = crate::tui::theme::current();
        assert_eq!(
            capacity_color(ContextWindowUsage::provider_reported(74, 100)),
            theme.quote
        );
        assert_eq!(
            capacity_color(ContextWindowUsage::provider_reported(75, 100)),
            theme.warn
        );
        assert_eq!(
            capacity_color(ContextWindowUsage::provider_reported(90, 100)),
            theme.error
        );
    }

    #[test]
    fn estimated_usage_avoids_cryptic_punctuation_in_the_compact_readout() {
        let buf = render(ContextWindowUsage::estimated(95_000, 800_000), 64);
        let rendered = text(&buf);
        assert!(rendered.contains("95k / 800k"));
        assert!(!rendered.contains('~'));
    }

    #[test]
    fn narrow_width_keeps_a_compact_readout_and_never_overflows() {
        let buf = render(ContextWindowUsage::provider_reported(95_000, 800_000), 14);
        let rendered = text(&buf);
        assert_eq!(rendered.width(), 14);
        assert!(
            rendered.contains("95k / 800k") || rendered.contains("95k/800k"),
            "{rendered:?}"
        );
    }
}
