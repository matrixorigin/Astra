use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use super::color::blend;
use super::terminal_palette::{default_bg, default_fg};

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

pub(crate) fn elapsed_since_start() -> Duration {
    let start = PROCESS_START.get_or_init(Instant::now);
    start.elapsed()
}

/// RGB color for a given "position along a border" (0..len) at the
/// current moment in time. Produces a flowing rainbow-ish gradient
/// that cycles around the frame — warm pinks → cool blues → back.
/// Hue advances with time and along the border, giving a "wave
/// travelling around the frame" effect.
///
/// Used by `LiveFramedCell` to color each border character while the
/// active cell is still streaming.
pub(crate) fn gradient_color_at(pos: usize, len: usize, period_seconds: f32) -> (u8, u8, u8) {
    let t = elapsed_since_start().as_secs_f32();
    // Normalize position to [0, 1) along the border, then add a
    // time-varying phase so the hue slides along the frame.
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
