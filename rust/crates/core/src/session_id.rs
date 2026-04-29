//! Session ID validation — single source of truth for all crates.

use std::path::Path;

const MAX_SESSION_ID_LEN: usize = 200;

/// Validate that a session ID is safe for use as a filesystem path component.
///
/// Rejects:
/// - empty or whitespace-only IDs
/// - non-ASCII characters (blocks Unicode invisibles, RTL overrides, homoglyphs)
/// - ASCII control characters (including NUL and DEL)
/// - path separators (`/`, `\`), `..` anywhere (guards against traversal)
/// - `.` as a standalone ID (maps to current directory)
/// - IDs longer than 200 bytes (filesystem NAME_MAX safety)
/// - multi-component paths after OS normalization
pub fn validate(session_id: &str) -> Result<(), String> {
    if session_id.is_empty() || session_id.trim().is_empty() {
        return Err("session ID cannot be empty".to_string());
    }
    if !session_id.is_ascii() {
        return Err(format!(
            "invalid session ID {:?}: must contain only ASCII characters",
            session_id
        ));
    }
    if session_id.len() > MAX_SESSION_ID_LEN {
        return Err(format!(
            "invalid session ID (len={}): must be at most {} bytes",
            session_id.len(),
            MAX_SESSION_ID_LEN,
        ));
    }
    if session_id == "."
        || session_id.contains('/')
        || session_id.contains('\\')
        || session_id.contains("..")
        || session_id.bytes().any(|b| b.is_ascii_control())
    {
        return Err(format!(
            "invalid session ID {:?}: must not contain path separators, '..', or control characters",
            session_id
        ));
    }
    if Path::new(session_id).components().count() != 1 {
        return Err(format!(
            "invalid session ID {:?}: must be a single path component",
            session_id
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid() {
        let long_id = "a".repeat(MAX_SESSION_ID_LEN + 1);
        let cases: Vec<&str> = vec![
            "",
            "   ",
            ".",
            "../etc/passwd",
            "foo/bar",
            "a\\b",
            "..",
            "has\0nul",
            "has\nnewline",
            "has\ttab",
            "has\x7Fdel",
            "café",
            "abc\u{200B}def",
            "\u{202E}secret",
            &long_id,
        ];
        for bad in cases {
            assert!(validate(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn accepts_valid() {
        for good in [
            "abc123",
            "550e8400-e29b-41d4-a716-446655440000",
            "session_with-dashes.and.dots",
            &"a".repeat(MAX_SESSION_ID_LEN),
        ] {
            assert!(validate(good).is_ok(), "{good:?} should be accepted");
        }
    }
}
