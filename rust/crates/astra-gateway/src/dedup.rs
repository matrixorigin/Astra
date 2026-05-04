use std::collections::VecDeque;
use std::time::{Duration, Instant};

const DEFAULT_MAX_SIZE: usize = 1000;
const DEFAULT_TTL: Duration = Duration::from_secs(300);

/// Message deduplicator: tracks recently seen message IDs to prevent
/// double-processing from platform duplicate delivery.
pub struct MessageDeduplicator {
    seen: VecDeque<(String, Instant)>,
    max_size: usize,
    ttl: Duration,
}

impl MessageDeduplicator {
    pub fn new() -> Self {
        Self {
            seen: VecDeque::new(),
            max_size: DEFAULT_MAX_SIZE,
            ttl: DEFAULT_TTL,
        }
    }

    /// Returns true if this message ID is new (not a duplicate).
    /// Returns false if it was already seen (duplicate — should skip).
    pub fn check(&mut self, msg_id: &str) -> bool {
        self.evict_expired();

        if self.seen.iter().any(|(id, _)| id == msg_id) {
            return false;
        }

        if self.seen.len() >= self.max_size {
            self.seen.pop_front();
        }
        self.seen.push_back((msg_id.to_string(), Instant::now()));
        true
    }

    fn evict_expired(&mut self) {
        while let Some((_, ts)) = self.seen.front() {
            if ts.elapsed() > self.ttl {
                self.seen.pop_front();
            } else {
                break;
            }
        }
    }
}

impl Default for MessageDeduplicator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_message_returns_true() {
        let mut dedup = MessageDeduplicator::new();
        assert!(dedup.check("msg-1"));
    }

    #[test]
    fn duplicate_returns_false() {
        let mut dedup = MessageDeduplicator::new();
        assert!(dedup.check("msg-1"));
        assert!(!dedup.check("msg-1"));
    }

    #[test]
    fn different_ids_both_new() {
        let mut dedup = MessageDeduplicator::new();
        assert!(dedup.check("msg-1"));
        assert!(dedup.check("msg-2"));
    }

    #[test]
    fn bounded_size() {
        let mut dedup = MessageDeduplicator {
            seen: VecDeque::new(),
            max_size: 3,
            ttl: DEFAULT_TTL,
        };
        dedup.check("a");
        dedup.check("b");
        dedup.check("c");
        dedup.check("d"); // evicts "a"
        assert!(dedup.check("a")); // "a" is new again
        assert!(!dedup.check("d")); // "d" is still seen
    }

    #[test]
    fn empty_id_is_new_every_time() {
        let mut dedup = MessageDeduplicator::new();
        // Empty IDs should be rejected by caller, not dedup
        assert!(dedup.check(""));
        // But dedup stores it, so second call returns false
        assert!(!dedup.check(""));
    }

    #[test]
    fn ttl_expiry() {
        let mut dedup = MessageDeduplicator {
            seen: VecDeque::new(),
            max_size: DEFAULT_MAX_SIZE,
            ttl: Duration::from_millis(1), // very short TTL
        };
        dedup.check("msg-1");
        std::thread::sleep(Duration::from_millis(10));
        // After TTL, message should be treated as new
        assert!(dedup.check("msg-1"), "expired message should be new");
    }

    #[test]
    fn many_messages_bounded() {
        let mut dedup = MessageDeduplicator::new(); // max 1000
        for i in 0..1500 {
            dedup.check(&format!("msg-{i}"));
        }
        assert!(dedup.seen.len() <= DEFAULT_MAX_SIZE);
    }
}
