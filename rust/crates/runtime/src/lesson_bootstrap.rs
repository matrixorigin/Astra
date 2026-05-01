//! Session-bootstrap constants for cross-session lesson loading.
//!
//! Bootstrap now goes entirely through Memoria `/v1/memories/retrieve`.
//! The DAO-based `attach_session_lessons` was removed — Memoria is the
//! single source of truth for lessons (Session Memory Protocol L3).

/// Default number of lessons to retrieve from Memoria at session start.
pub const DEFAULT_SESSION_BOOTSTRAP_LIMIT: u32 = 6;
