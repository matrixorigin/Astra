use std::time::{Duration, Instant};

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
    paste_guard: Option<(Instant, GuardedInput)>,
}

pub(crate) enum BurstDecision {
    Normal,
    Buffered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardedInput {
    Insert,
    Discard,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PasteTail {
    last_input: Instant,
    guard_started: Instant,
}

impl PasteBurstDetector {
    pub fn new() -> Self {
        Self {
            last_char_time: None,
            consecutive_fast_chars: 0,
            buffer: String::new(),
            active: false,
            paste_guard: None,
        }
    }

    pub fn on_char(&mut self, c: char, now: Instant) -> BurstDecision {
        let is_fast = self.last_char_time.is_some_and(|prev| {
            now.checked_duration_since(prev)
                .is_some_and(|elapsed| elapsed <= Duration::from_millis(BURST_CHAR_INTERVAL_MS))
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

    pub fn guarded_input(&mut self, now: Instant) -> Option<GuardedInput> {
        if self.active {
            return Some(GuardedInput::Insert);
        }
        if let Some((at, action)) = self.paste_guard
            && now
                .checked_duration_since(at)
                .is_some_and(|elapsed| elapsed <= Duration::from_millis(ENTER_SUPPRESS_WINDOW_MS))
        {
            return Some(action);
        }
        if self.last_char_time.is_some_and(|last| {
            now.checked_duration_since(last)
                .is_some_and(|elapsed| elapsed <= Duration::from_millis(BURST_CHAR_INTERVAL_MS))
        }) {
            self.paste_guard = Some((now, GuardedInput::Insert));
            return Some(GuardedInput::Insert);
        }
        self.paste_guard = None;
        None
    }

    /// Printable bytes belong to a burst only while they keep arriving at the
    /// burst rate. The longer Enter/Tab grace must not steal a new editor's
    /// normal input after focus changes.
    pub fn plain_input(&mut self, now: Instant) -> Option<GuardedInput> {
        let fast = self.last_char_time.is_some_and(|last| {
            now.checked_duration_since(last)
                .is_some_and(|elapsed| elapsed <= Duration::from_millis(BURST_CHAR_INTERVAL_MS))
        });
        match (fast, self.paste_guard) {
            (true, Some((_, GuardedInput::Discard))) => {
                self.note_guarded_control(now, GuardedInput::Discard);
                Some(GuardedInput::Discard)
            }
            (true, _) => Some(GuardedInput::Insert),
            (false, Some((_, GuardedInput::Discard))) => {
                self.paste_guard = None;
                None
            }
            (false, _) => None,
        }
    }

    pub fn note_guarded_control(&mut self, now: Instant, action: GuardedInput) {
        if self.last_char_time.is_some_and(|last| {
            now.checked_duration_since(last)
                .is_some_and(|elapsed| elapsed <= Duration::from_millis(BURST_CHAR_INTERVAL_MS))
        }) {
            self.last_char_time = Some(now);
            self.paste_guard = Some((now, action));
        }
    }

    pub fn pending_tail(&mut self, now: Instant) -> Option<PasteTail> {
        self.guarded_input(now)?;
        let last_input = self.last_char_time.or(self.paste_guard.map(|(at, _)| at))?;
        Some(PasteTail {
            last_input,
            guard_started: self.paste_guard.map_or(last_input, |(at, _)| at),
        })
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
            self.flush(now)
        } else {
            None
        }
    }

    pub fn flush(&mut self, now: Instant) -> Option<String> {
        if !self.active {
            return None;
        }
        let text = std::mem::take(&mut self.buffer);
        self.active = false;
        self.consecutive_fast_chars = 0;
        self.paste_guard = Some((now, GuardedInput::Insert));
        (!text.is_empty()).then_some(text)
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn release_focus(&mut self) {
        debug_assert!(!self.active);
        self.last_char_time = None;
        self.consecutive_fast_chars = 0;
        self.paste_guard = None;
    }

    pub(super) fn shrink_buffer(&mut self) {
        self.buffer.shrink_to_fit();
    }

    /// Replacing or clearing text must still reject a trailing pasted newline.
    pub fn reset_preserving_enter_guard(&mut self) {
        if self.active {
            self.paste_guard = Some((Instant::now(), GuardedInput::Insert));
        }
        if !matches!(self.paste_guard, Some((_, GuardedInput::Discard))) {
            self.last_char_time = None;
        }
        self.consecutive_fast_chars = 0;
        self.buffer.clear();
        self.active = false;
    }

    /// A canceled editor must not let a trailing pasted Enter modify the
    /// restored draft or submit it.
    pub fn cancel(&mut self, now: Instant) {
        let pending = self.pending_tail(now);
        self.buffer.clear();
        self.active = false;
        self.last_char_time = None;
        self.consecutive_fast_chars = 0;
        self.paste_guard = None;
        if let Some(tail) = pending {
            self.discard_tail(tail);
        }
    }

    pub fn discard_tail(&mut self, tail: PasteTail) {
        self.last_char_time = Some(tail.last_input);
        self.paste_guard = Some((tail.guard_started, GuardedInput::Discard));
    }

    #[cfg(test)]
    pub fn force_due_buffer_for_test(&mut self, text: &str, now: Instant) {
        self.last_char_time = Some(now - std::time::Duration::from_millis(FLUSH_IDLE_MS + 1));
        self.consecutive_fast_chars = BURST_MIN_CHARS;
        self.buffer = text.to_string();
        self.active = true;
        self.paste_guard = None;
    }

    pub fn recommended_tick_ms() -> u64 {
        FLUSH_IDLE_MS + 1
    }
}

#[cfg(test)]
mod tests {
    use super::{BurstDecision, GuardedInput, PasteBurstDetector};
    use std::time::{Duration, Instant};

    #[test]
    fn text_replacement_preserves_the_flush_enter_guard() {
        let now = Instant::now();
        let mut detector = PasteBurstDetector::new();
        detector.force_due_buffer_for_test("pasted", now);
        assert_eq!(detector.flush_if_due(now).as_deref(), Some("pasted"));
        detector.reset_preserving_enter_guard();
        assert!(!detector.is_active());
        assert!(
            detector
                .guarded_input(now + Duration::from_millis(100))
                .is_some()
        );
        assert!(
            detector
                .guarded_input(now + Duration::from_millis(150))
                .is_none()
        );
    }

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
    fn submillisecond_and_short_line_pastes_guard_enter() {
        let now = Instant::now();
        let mut burst = PasteBurstDetector::new();
        for ch in ['a', 'b', 'c'] {
            burst.on_char(ch, now);
        }
        assert!(burst.is_active());
        assert!(burst.guarded_input(now).is_some());

        let mut short = PasteBurstDetector::new();
        short.on_char('x', now);
        assert!(
            short
                .guarded_input(now + Duration::from_millis(1))
                .is_some()
        );
        short.on_char('y', now + Duration::from_millis(2));
        assert!(
            short
                .guarded_input(now + Duration::from_millis(3))
                .is_some()
        );
    }

    #[test]
    fn ordinary_typing_does_not_delay_enter() {
        let now = Instant::now();
        let mut detector = PasteBurstDetector::new();
        detector.on_char('x', now);
        assert!(
            detector
                .guarded_input(now + Duration::from_millis(20))
                .is_none()
        );
    }

    #[test]
    fn canceled_paste_discards_its_trailing_enter() {
        let now = Instant::now();
        let mut detector = PasteBurstDetector::new();
        detector.on_char('x', now);
        detector.cancel(now + Duration::from_millis(1));
        assert_eq!(
            detector.guarded_input(now + Duration::from_millis(2)),
            Some(GuardedInput::Discard)
        );
        assert_eq!(
            detector.guarded_input(now + Duration::from_millis(150)),
            None
        );
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
        assert!(d.guarded_input(t0 + Duration::from_millis(5)).is_some());
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
        assert!(d.guarded_input(t0 + Duration::from_millis(100)).is_some());
        // After 120ms
        assert!(d.guarded_input(t0 + Duration::from_millis(200)).is_none());
    }

    #[test]
    fn reset_drops_active_buffer_but_keeps_newline_protection() {
        let mut d = PasteBurstDetector::new();
        let t0 = Instant::now();
        d.on_char('a', t0);
        d.on_char('b', t0 + Duration::from_millis(2));
        d.on_char('c', t0 + Duration::from_millis(4));
        assert!(d.is_active());

        d.reset_preserving_enter_guard();
        assert!(d.guarded_input(Instant::now()).is_some());

        assert!(!d.is_active());
        assert!(d.flush_if_due(t0 + Duration::from_millis(20)).is_none());
        assert!(matches!(
            d.on_char('/', t0 + Duration::from_millis(30)),
            BurstDecision::Normal
        ));
    }
}
