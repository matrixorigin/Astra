//! Session-stable latches for cache break prevention.
//!
//! A latch starts as `None`, becomes `Some(value)` on first trigger,
//! and NEVER changes again for the session lifetime. This prevents
//! mid-session mode toggles from invalidating the KV cache prefix.

use serde::{Deserialize, Serialize};

use crate::section_types::CacheScope;

/// A beta header that was latched (sent once, must be sent every subsequent turn).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatchedHeader {
    pub name: String,
    pub value: String,
    pub latched_at_turn: u32,
}

/// A provider-specific feature that was latched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatchedFeature {
    pub key: String,
    pub latched_at_turn: u32,
}

/// Session-stable latches. Each field starts as None, becomes Some(value)
/// on first trigger, and NEVER changes again for the session lifetime.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionLatches {
    /// Beta headers that, once sent, must be sent on every subsequent turn.
    pub beta_headers: Vec<LatchedHeader>,
    /// Cache scope eligibility (e.g., 1h TTL, global scope).
    /// Evaluated once, then frozen to prevent mid-session scope flips.
    pub cache_scope: Option<CacheScope>,
    /// Turn where cache scope first latched.
    pub cache_scope_latched_at_turn: Option<u32>,
    /// Provider-specific feature gates that affect serialization.
    pub provider_features: Vec<LatchedFeature>,
}

impl SessionLatches {
    /// Attempt to latch a beta header. If a header with the same name is
    /// already latched, this is a no-op (the original value is preserved).
    /// Returns `true` if the header was newly latched.
    pub fn latch_header(
        &mut self,
        name: impl Into<String>,
        value: impl Into<String>,
        turn: u32,
    ) -> bool {
        let name = name.into();
        if self.beta_headers.iter().any(|h| h.name == name) {
            return false;
        }
        self.beta_headers.push(LatchedHeader {
            name,
            value: value.into(),
            latched_at_turn: turn,
        });
        true
    }

    /// Attempt to latch the cache scope. If already latched, this is a no-op.
    /// Returns `true` if the scope was newly latched.
    pub fn latch_cache_scope(&mut self, scope: CacheScope, turn: u32) -> bool {
        if self.cache_scope.is_some() {
            return false;
        }
        self.cache_scope = Some(scope);
        self.cache_scope_latched_at_turn = Some(turn);
        true
    }

    /// Attempt to latch a provider feature. If already latched, this is a no-op.
    /// Returns `true` if the feature was newly latched.
    pub fn latch_feature(&mut self, key: impl Into<String>, turn: u32) -> bool {
        let key = key.into();
        if self.provider_features.iter().any(|f| f.key == key) {
            return false;
        }
        self.provider_features.push(LatchedFeature {
            key,
            latched_at_turn: turn,
        });
        true
    }

    /// Check if a specific beta header is latched.
    #[must_use]
    pub fn has_header(&self, name: &str) -> bool {
        self.beta_headers.iter().any(|h| h.name == name)
    }

    /// Returns true if any latch was triggered on the given turn.
    /// Used by the optimizer to suppress cache markers near volatile content
    /// that was just introduced this turn (and may change next turn).
    #[must_use]
    pub fn any_flipped_this_turn(&self, current_turn: u32) -> bool {
        self.beta_headers
            .iter()
            .any(|h| h.latched_at_turn == current_turn)
            || self.cache_scope_latched_at_turn == Some(current_turn)
            || self
                .provider_features
                .iter()
                .any(|f| f.latched_at_turn == current_turn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_none_by_default() {
        let l = SessionLatches::default();
        assert!(l.beta_headers.is_empty());
        assert!(l.cache_scope.is_none());
        assert!(l.cache_scope_latched_at_turn.is_none());
        assert!(l.provider_features.is_empty());
    }

    #[test]
    fn latch_once_set_never_changes() {
        let mut l = SessionLatches::default();
        assert!(l.latch_header("X-Mode", "auto", 1));
        assert!(!l.latch_header("X-Mode", "manual", 5));
        assert_eq!(l.beta_headers.len(), 1);
        assert_eq!(l.beta_headers[0].value, "auto");
        assert_eq!(l.beta_headers[0].latched_at_turn, 1);
    }

    #[test]
    fn cache_scope_frozen_after_first() {
        let mut l = SessionLatches::default();
        assert!(l.latch_cache_scope(CacheScope::Global, 4));
        assert!(!l.latch_cache_scope(CacheScope::Session, 5));
        assert_eq!(l.cache_scope, Some(CacheScope::Global));
        assert_eq!(l.cache_scope_latched_at_turn, Some(4));
        assert!(l.any_flipped_this_turn(4));
        assert!(!l.any_flipped_this_turn(5));
    }

    #[test]
    fn feature_latch_idempotent() {
        let mut l = SessionLatches::default();
        assert!(l.latch_feature("thinking_clear", 3));
        assert!(!l.latch_feature("thinking_clear", 7));
        assert_eq!(l.provider_features.len(), 1);
        assert_eq!(l.provider_features[0].latched_at_turn, 3);
    }

    #[test]
    fn has_header_checks_name() {
        let mut l = SessionLatches::default();
        assert!(!l.has_header("X-Mode"));
        l.latch_header("X-Mode", "auto", 1);
        assert!(l.has_header("X-Mode"));
        assert!(!l.has_header("X-Other"));
    }
}
