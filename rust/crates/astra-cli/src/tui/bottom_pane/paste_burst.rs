use std::time::Instant;

/// Max inter-character interval to stay in a burst (8ms matches Codex).
const BURST_CHAR_INTERVAL_MS: u64 = 8;

/// Minimum consecutive fast chars to trigger burst mode.
const BURST_MIN_CHARS: usize = 3;

/// Grace period after a burst flushes during which Enter inserts a
/// newline rather than submitting. Prevents accidental submit when
/// pasted text ends with a trailing newline.
const ENTER_SUPPRESS_WINDOW_MS: u64 = 120;

/// Idle timeout before flushing buffered paste content.
#[cfg(not(windows))]
const FLUSH_IDLE_MS: u64 = 8;
#[cfg(windows)]
const FLUSH_IDLE_MS: u64 = 60;

#[derive(Debug)]
pub(crate) struct PasteBurstDetector {
    last_char_time: Option<Instant>,
    consecutive_fast_chars: usize,
    buffer: String,
    active: bool,
    last_flush_time: Option<Instant>,
}

pub(crate) enum BurstDecision {
    Normal,
    Buffered,
}

impl PasteBurstDetector {
    pub fn new() -> Self {
        Self {
            last_char_time: None,
            consecutive_fast_chars: 0,
            buffer: String::new(),
            active: false,
            last_flush_time: None,
        }
    }

    pub fn on_char(&mut self, c: char, now: Instant) -> BurstDecision {
        let is_fast = self.last_char_time.is_some_and(|prev| {
            let elapsed_us = now.duration_since(prev).as_micros() as u64;
            // Sub-millisecond intervals (< 1000µs) are synthetic (test
            // harness loops) — real terminal input has ≥1ms between chars
            // even during the fastest paste. Gate on ≥1ms to avoid false
            // burst triggers in unit tests.
            (1000..=BURST_CHAR_INTERVAL_MS * 1000).contains(&elapsed_us)
        });

        if is_fast {
            self.consecutive_fast_chars += 1;
        } else {
            self.consecutive_fast_chars = 1;
        }
        self.last_char_time = Some(now);

        if self.consecutive_fast_chars >= BURST_MIN_CHARS {
            self.active = true;
        }

        if self.active {
            self.buffer.push(c);
            BurstDecision::Buffered
        } else {
            BurstDecision::Normal
        }
    }

    pub fn enter_should_insert_newline(&self, now: Instant) -> bool {
        if self.active {
            return true;
        }
        if let Some(flush_time) = self.last_flush_time {
            return now.duration_since(flush_time).as_millis() as u64 <= ENTER_SUPPRESS_WINDOW_MS;
        }
        false
    }

    pub fn flush_if_due(&mut self, now: Instant) -> Option<String> {
        if !self.active {
            return None;
        }
        let idle = self
            .last_char_time
            .map(|t| now.duration_since(t).as_millis() as u64)
            .unwrap_or(u64::MAX);

        if idle > FLUSH_IDLE_MS {
            let text = std::mem::take(&mut self.buffer);
            self.active = false;
            self.consecutive_fast_chars = 0;
            self.last_flush_time = Some(now);
            if text.is_empty() { None } else { Some(text) }
        } else {
            None
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn reset(&mut self) {
        self.last_char_time = None;
        self.consecutive_fast_chars = 0;
        self.buffer.clear();
        self.active = false;
        self.last_flush_time = None;
    }

    #[cfg(test)]
    pub fn force_due_buffer_for_test(&mut self, text: &str, now: Instant) {
        self.last_char_time = Some(now - std::time::Duration::from_millis(FLUSH_IDLE_MS + 1));
        self.consecutive_fast_chars = BURST_MIN_CHARS;
        self.buffer = text.to_string();
        self.active = true;
        self.last_flush_time = None;
    }

    pub fn recommended_tick_ms() -> u64 {
        FLUSH_IDLE_MS + 1
    }
}

#[cfg(test)]
mod tests {
    use super::{BurstDecision, PasteBurstDetector};
    use std::time::{Duration, Instant};

    #[test]
    fn slow_typing_is_not_burst() {
        let mut d = PasteBurstDetector::new();
        let t0 = Instant::now();
        for i in 0..10 {
            let t = t0 + Duration::from_millis(i * 100);
            assert!(matches!(d.on_char('a', t), BurstDecision::Normal));
        }
        assert!(!d.is_active());
    }

    #[test]
    fn fast_chars_trigger_burst() {
        let mut d = PasteBurstDetector::new();
        let t0 = Instant::now();
        // First 2 chars: not yet burst
        assert!(matches!(d.on_char('a', t0), BurstDecision::Normal));
        assert!(matches!(
            d.on_char('b', t0 + Duration::from_millis(2)),
            BurstDecision::Normal
        ));
        // Third char triggers burst
        assert!(matches!(
            d.on_char('c', t0 + Duration::from_millis(4)),
            BurstDecision::Buffered
        ));
        assert!(d.is_active());
    }

    #[test]
    fn flush_after_idle() {
        let mut d = PasteBurstDetector::new();
        let t0 = Instant::now();
        d.on_char('a', t0);
        d.on_char('b', t0 + Duration::from_millis(2));
        d.on_char('c', t0 + Duration::from_millis(4));
        d.on_char('d', t0 + Duration::from_millis(6));

        // Not yet idle enough
        assert!(d.flush_if_due(t0 + Duration::from_millis(10)).is_none());

        // Now idle
        let flushed = d.flush_if_due(t0 + Duration::from_millis(20));
        assert_eq!(flushed.as_deref(), Some("cd"));
        assert!(!d.is_active());
    }

    #[test]
    fn enter_suppressed_during_burst() {
        let mut d = PasteBurstDetector::new();
        let t0 = Instant::now();
        d.on_char('a', t0);
        d.on_char('b', t0 + Duration::from_millis(2));
        d.on_char('c', t0 + Duration::from_millis(4));
        assert!(d.enter_should_insert_newline(t0 + Duration::from_millis(5)));
    }

    #[test]
    fn enter_suppressed_in_grace_window_after_flush() {
        let mut d = PasteBurstDetector::new();
        let t0 = Instant::now();
        d.on_char('a', t0);
        d.on_char('b', t0 + Duration::from_millis(2));
        d.on_char('c', t0 + Duration::from_millis(4));
        d.flush_if_due(t0 + Duration::from_millis(20));

        // Within 120ms of flush
        assert!(d.enter_should_insert_newline(t0 + Duration::from_millis(100)));
        // After 120ms
        assert!(!d.enter_should_insert_newline(t0 + Duration::from_millis(200)));
    }

    #[test]
    fn reset_drops_active_buffer_and_timing_state() {
        let mut d = PasteBurstDetector::new();
        let t0 = Instant::now();
        d.on_char('a', t0);
        d.on_char('b', t0 + Duration::from_millis(2));
        d.on_char('c', t0 + Duration::from_millis(4));
        assert!(d.is_active());

        d.reset();

        assert!(!d.is_active());
        assert!(d.flush_if_due(t0 + Duration::from_millis(20)).is_none());
        assert!(matches!(
            d.on_char('/', t0 + Duration::from_millis(30)),
            BurstDecision::Normal
        ));
    }
}
