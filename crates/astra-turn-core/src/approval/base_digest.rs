//! Issue #326 P5f / R2 Major 3 / scenarios #5 / #35 / #49:
//! host-generated file digest for stale-revalidation.
//!
//! ## Why "host generated"
//!
//! When the LLM proposes `edit_file(path, patch)`, the patch
//! applies to whatever's on disk **at execute time**. If a
//! second tool (or the user, or a sibling agent) edits the same
//! file between approval and execution, the patch silently lands
//! on top of the wrong baseline — the user approved one diff and
//! got another.
//!
//! The mitigation is "approval is bound to a base digest": if
//! the file changed since approval, re-prompt with a fresh diff.
//! Plan v3 §P5f spells this out.
//!
//! Crucially the digest must be computed by the **host**, not the
//! LLM. R2 Major 3 calls this out: an `expected_base_sha` arg
//! supplied by the model is trivial to spoof — pass an old hash,
//! get a stale-but-approved-looking patch through. The only
//! trustworthy moment to read the file is at host enqueue time,
//! and that's what this module helps with.
//!
//! ## What this module does
//!
//! - [`BaseDigest`] — wrapper around `[u8; 32]` (SHA-256) that
//!   serializes as hex so it can ride in JSON envelopes.
//! - [`compute_file_digest`] — read a file and produce a
//!   `BaseDigest`. Returns `None` for "file doesn't exist yet"
//!   (a brand-new write tool call) which is a valid state, not
//!   an error.
//! - [`stale_check`] — the actual contract: compare the
//!   digest at enqueue against the digest at execute time and
//!   return one of `[Fresh, Stale, FileGone]`.
//!
//! The wiring into the approval queue + executor is staged for a
//! follow-up commit in this PR; this commit lands the type and
//! contract so the queue layer (P4) can store `BaseDigest` on
//! every PendingApproval.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// SHA-256 digest of a file's bytes, kept hex-encoded for
/// readability and JSON envelope compatibility.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BaseDigest(pub String);

impl BaseDigest {
    /// Construct from raw 32-byte SHA-256 output.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        let mut hex = String::with_capacity(64);
        for b in bytes {
            use std::fmt::Write;
            let _ = write!(hex, "{b:02x}");
        }
        Self(hex)
    }

    /// Length-prefixed display for traces / UI: `sha256:abc…def`.
    #[must_use]
    pub fn short_display(&self) -> String {
        if self.0.len() <= 16 {
            format!("sha256:{}", self.0)
        } else {
            format!("sha256:{}…{}", &self.0[..8], &self.0[self.0.len() - 8..])
        }
    }
}

/// Result of comparing the enqueue-time digest to a fresh read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StaleCheck {
    /// File hasn't changed since approval. Safe to apply.
    Fresh,
    /// File contents changed (different bytes). Approval must be
    /// re-prompted with the new baseline.
    Stale {
        previous: BaseDigest,
        current: BaseDigest,
    },
    /// File existed at enqueue but is now gone. Approval should
    /// be re-prompted (or the tool call rejected outright since
    /// "delete and re-apply" is a different decision).
    FileGone { previous: BaseDigest },
    /// File didn't exist at enqueue and still doesn't — fine for
    /// a brand-new write tool. Identical to `Fresh` semantically.
    StillAbsent,
    /// File didn't exist at enqueue but now does. Almost
    /// certainly stale — re-prompt.
    AppearedSinceEnqueue { current: BaseDigest },
}

impl StaleCheck {
    /// Quick yes/no for "is the original approval still valid?".
    #[must_use]
    pub fn is_fresh(&self) -> bool {
        matches!(self, Self::Fresh | Self::StillAbsent)
    }
}

/// Compute the SHA-256 digest of a file. Returns `None` when the
/// file doesn't exist (legitimate state for a brand-new write).
/// Returns `Err` for actual I/O failures (permission denied,
/// not-a-file, etc.) so the caller can surface a real error.
pub fn compute_file_digest(path: &Path) -> std::io::Result<Option<BaseDigest>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest: [u8; 32] = hasher.finalize().into();
    Ok(Some(BaseDigest::from_bytes(digest)))
}

/// Compare the enqueue-time digest against the file's current
/// state and return a [`StaleCheck`].
///
/// `previous` is whatever was stored on the [`PendingApproval`]
/// at enqueue time. `path` is the file as the executor is about
/// to read it.
pub fn stale_check(path: &Path, previous: Option<BaseDigest>) -> std::io::Result<StaleCheck> {
    let current = compute_file_digest(path)?;
    Ok(match (previous, current) {
        (None, None) => StaleCheck::StillAbsent,
        (Some(prev), None) => StaleCheck::FileGone { previous: prev },
        (None, Some(cur)) => StaleCheck::AppearedSinceEnqueue { current: cur },
        (Some(prev), Some(cur)) => {
            if prev == cur {
                StaleCheck::Fresh
            } else {
                StaleCheck::Stale {
                    previous: prev,
                    current: cur,
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_hex_64_chars() {
        let d = BaseDigest::from_bytes([0xab; 32]);
        assert_eq!(d.0.len(), 64);
        assert_eq!(d.0, "ab".repeat(32));
    }

    #[test]
    fn short_display_truncates() {
        let d = BaseDigest::from_bytes([0xab; 32]);
        let s = d.short_display();
        assert!(s.starts_with("sha256:"));
        assert!(s.contains('…'));
    }

    #[test]
    fn compute_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-here");
        assert!(compute_file_digest(&path).unwrap().is_none());
    }

    #[test]
    fn compute_matches_for_same_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"hello").unwrap();
        let d1 = compute_file_digest(&path).unwrap().unwrap();
        let d2 = compute_file_digest(&path).unwrap().unwrap();
        assert_eq!(d1, d2);
    }

    #[test]
    fn compute_changes_for_different_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"hello").unwrap();
        let d1 = compute_file_digest(&path).unwrap().unwrap();
        std::fs::write(&path, b"hello world").unwrap();
        let d2 = compute_file_digest(&path).unwrap().unwrap();
        assert_ne!(d1, d2);
    }

    #[test]
    fn stale_check_fresh_when_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"hello").unwrap();
        let snap = compute_file_digest(&path).unwrap();
        let result = stale_check(&path, snap).unwrap();
        assert_eq!(result, StaleCheck::Fresh);
        assert!(result.is_fresh());
    }

    #[test]
    fn stale_check_stale_when_modified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"hello").unwrap();
        let snap = compute_file_digest(&path).unwrap();

        std::fs::write(&path, b"hello changed").unwrap();
        let result = stale_check(&path, snap).unwrap();
        match result {
            StaleCheck::Stale { previous, current } => {
                assert_ne!(previous, current);
            }
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn stale_check_file_gone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"hello").unwrap();
        let snap = compute_file_digest(&path).unwrap();

        std::fs::remove_file(&path).unwrap();
        let result = stale_check(&path, snap).unwrap();
        assert!(matches!(result, StaleCheck::FileGone { .. }));
        assert!(!result.is_fresh());
    }

    #[test]
    fn stale_check_still_absent_for_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-yet");
        let result = stale_check(&path, None).unwrap();
        assert_eq!(result, StaleCheck::StillAbsent);
        assert!(result.is_fresh());
    }

    #[test]
    fn stale_check_appeared_since_enqueue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        // No file at enqueue time.
        let snap = None;
        // File appeared between enqueue and execute.
        std::fs::write(&path, b"hello").unwrap();
        let result = stale_check(&path, snap).unwrap();
        assert!(matches!(result, StaleCheck::AppearedSinceEnqueue { .. }));
        assert!(!result.is_fresh());
    }
}
