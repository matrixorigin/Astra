use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use super::color::blend;
use super::terminal_palette::{default_bg, default_fg};

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Eagerly initialize the shimmer time origin. Call once near the
/// top of `run_tui_repl` so any `Instant` captured later by a cell's
/// `finalize()` is guaranteed to be `>= PROCESS_START`. Without this,
/// the first cell to finalize before any `elapsed_since_start()` /
/// `gradient_color_at` call would saturate `time_at` to 0 and the
/// gutter colour would jump on freeze.
pub(crate) fn init_time_origin() {
    let _ = PROCESS_START.get_or_init(Instant::now);
}

pub(crate) fn elapsed_since_start() -> Duration {
    let start = PROCESS_START.get_or_init(Instant::now);
    start.elapsed()
}

/// Process-relative seconds for a specific `Instant` — same time
/// basis as [`elapsed_since_start`]. Lets cells stamp a "freeze
/// moment" at finalize and feed it back into [`gradient_color_at_t`]
/// so the gradient locks at the exact phase it had on the final
/// live frame.
pub(crate) fn time_at(i: Instant) -> f32 {
    let start = *PROCESS_START.get_or_init(Instant::now);
    i.saturating_duration_since(start).as_secs_f32()
}

/// RGB color for a given "position along a border" (0..len) at the
/// current moment in time. Produces a flowing rainbow-ish gradient
/// — warm pinks → cool blues → back. Hue advances with time and
/// along the border, giving a "wave travelling along the bar" effect.
pub(crate) fn gradient_color_at(pos: usize, len: usize, period_seconds: f32) -> (u8, u8, u8) {
    gradient_color_at_t(
        pos,
        len,
        period_seconds,
        elapsed_since_start().as_secs_f32(),
    )
}

/// Same as [`gradient_color_at`] but takes the time component
/// explicitly, so callers can lock the phase at a snapshot moment
/// (e.g. when a streaming cell finalizes — pin `t = freeze_phase` so
/// the gradient stops in place instead of jumping back to t=0).
pub(crate) fn gradient_color_at_t(
    pos: usize,
    len: usize,
    period_seconds: f32,
    t: f32,
) -> (u8, u8, u8) {
    let len = len.max(1) as f32;
    let phase = (t / period_seconds).fract();
    let u = ((pos as f32 / len) + phase).fract();
    hue_to_rgb(u)
}

/// Simple HSV-like hue sweep (S=0.55, V=1.0). Output is a soft pastel
/// wheel so the animation reads as flowing, not strobing.
fn hue_to_rgb(h: f32) -> (u8, u8, u8) {
    let h6 = (h.rem_euclid(1.0)) * 6.0;
    let c = 0.55_f32;
    let x = c * (1.0 - ((h6 % 2.0) - 1.0).abs());
    let (r, g, b) = if h6 < 1.0 {
        (c, x, 0.0)
    } else if h6 < 2.0 {
        (x, c, 0.0)
    } else if h6 < 3.0 {
        (0.0, c, x)
    } else if h6 < 4.0 {
        (0.0, x, c)
    } else if h6 < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = 1.0 - c;
    let to_u8 = |v: f32| (((v + m).clamp(0.0, 1.0)) * 255.0) as u8;
    (to_u8(r), to_u8(g), to_u8(b))
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
