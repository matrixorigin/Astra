use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use super::color::blend;
use super::terminal_palette::{default_bg, default_fg};

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Eagerly initialize the shared animation time origin. It is used only by
/// intentionally small status affordances, never as evidence of task work.
pub(crate) fn init_time_origin() {
    let _ = PROCESS_START.get_or_init(Instant::now);
}

/// Process time origin. Exposed for deterministic status-indicator tests.
pub(crate) fn process_start() -> Instant {
    *PROCESS_START.get_or_init(Instant::now)
}

pub(crate) fn elapsed_since_start() -> Duration {
    let start = PROCESS_START.get_or_init(Instant::now);
    start.elapsed()
}

/// Process-relative seconds for a specific `Instant`, used by the bounded
/// status indicator cadence.
pub(crate) fn time_at(i: Instant) -> f32 {
    let start = *PROCESS_START.get_or_init(Instant::now);
    i.saturating_duration_since(start).as_secs_f32()
}

pub(crate) fn shimmer_spans(text: &str) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }

    let padding = 10usize;
    let period = chars.len() + padding * 2;
    let sweep_seconds = 2.0f32;
    let pos_f =
        (elapsed_since_start().as_secs_f32() % sweep_seconds) / sweep_seconds * (period as f32);
    let pos = pos_f as usize;

    let has_true_color = supports_color::on_cached(supports_color::Stream::Stdout)
        .map(|level| level.has_16m)
        .unwrap_or(false);
    let band_half_width = 5.0;

    let base_color = default_fg().unwrap_or((128, 128, 128));
    let highlight_color = default_bg().unwrap_or((255, 255, 255));

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(chars.len());
    for (i, ch) in chars.iter().enumerate() {
        let i_pos = i as isize + padding as isize;
        let pos = pos as isize;
        let dist = (i_pos - pos).abs() as f32;

        let t = if dist <= band_half_width {
            let x = std::f32::consts::PI * (dist / band_half_width);
            0.5 * (1.0 + x.cos())
        } else {
            0.0
        };

        let style = if has_true_color {
            let highlight = t.clamp(0.0, 1.0);
            let (r, g, b) = blend(highlight_color, base_color, highlight * 0.9);
            Style::default()
                .fg(Color::Rgb(r, g, b))
                .add_modifier(Modifier::BOLD)
        } else if t < 0.2 {
            Style::default().add_modifier(Modifier::DIM)
        } else if t < 0.6 {
            Style::default()
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };

        spans.push(Span::styled(ch.to_string(), style));
    }
    spans
}
