//! Canonical validation for persisted conversation session identifiers.
//!
//! Session ids cross database, journal, workspace, and diagnostic boundaries.
//! Keep their admission contract in one place so callers never reflect an
//! arbitrary request body value as a durable or diagnostic session identity.

/// `agent_sessions.session_id` and every session-scoped durable table use
/// `VARCHAR(64)`. This is a byte limit because SQL column limits are measured
/// in bytes for the configured character set.
pub const MAX_PERSISTED_SESSION_ID_BYTES: usize = 64;

/// Validate an id before it is used to address a persisted session.
///
/// The journal validator owns the safe path-component grammar. This adds the
/// durable storage limit; callers must reject rather than normalize input.
pub fn validate_persisted_session_id(session_id: &str) -> Result<(), String> {
    if session_id.len() > MAX_PERSISTED_SESSION_ID_BYTES {
        return Err(format!(
            "session id exceeds the persisted {} byte limit",
            MAX_PERSISTED_SESSION_ID_BYTES
        ));
    }
    // Do not propagate the journal validator's detail: some invalid-id errors
    // include the rejected value, while this function is used at request and
    // diagnostic boundaries.
    crate::session_journal::validate_session_id(session_id)
        .map_err(|_| "session id does not satisfy the safe identifier grammar".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_session_id_requires_safe_storage_sized_identity() {
        assert!(validate_persisted_session_id(&"a".repeat(MAX_PERSISTED_SESSION_ID_BYTES)).is_ok());
        assert!(
            validate_persisted_session_id(&"a".repeat(MAX_PERSISTED_SESSION_ID_BYTES + 1)).is_err()
        );
        assert!(validate_persisted_session_id("unsafe/session").is_err());
    }
}
