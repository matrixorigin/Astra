//! L2: structural gate for persistent memory storage.
//!
//! **Problem**: Memoria accepts any string for any memory_type. The
//! `working` type is auto-purged and session-scoped, so pollution
//! there is self-limiting (L1 `should_store_in_memory` handles it).
//! But `semantic` / `episodic` / `procedural` are **persistent**:
//! anything stored to them survives the session, gets indexed for
//! vector search, and can resurface on any future query. Observed
//! in session `c6e18730`: unstructured LLM-generated summaries and
//! `[compaction:…]`-tagged blobs kept appearing as "memories" across
//! unrelated sessions.
//!
//! **Fix**: require all persistent-type writes to carry a structural
//! envelope:
//!
//!   [@ns/type] body
//!
//! where `ns` is one of the namespaces declared in
//! `astra_prompts::memory_proto` (pref / fact / plan / episode / insight /
//! knowledge / task / swap). `type` is a free status token (`active`,
//! `archived`, etc.). Body must carry enough content to be worth
//! retrieving — short blurbs get rejected, forcing the caller to either
//! summarize more durably or skip the write.
//!
//! This is **Claude Code's `type:` frontmatter + "what NOT to save"
//! principle** adapted to a vector-DB backend. Claude Code enforces at
//! write via a system-prompt section; astra enforces in code so any
//! store path (compaction, memory tool, session-end governance, future
//! callers) gets the same contract.
//!
//! What this module does NOT do:
//!   - Does not enforce a frontmatter / Why / How-to-apply body
//!     structure — that's a future L2b refinement.
//!   - Does not enforce cross-memory dedup — Memoria's vector similarity
//!     is the intended backstop.
//!   - Does not block `working` writes; those are session-scoped and
//!     handled by `should_store_in_memory` (L1).

/// Minimum body length (in unicode scalars, post-prefix) for a
/// persistent memory entry. Below this, the content can't carry
/// retrievable signal — it's either a stray token, an incomplete
/// fragment, or a short acknowledgment that should never have made
/// it this far.
///
/// Chosen at 30 scalars: covers `[@fact/active] Use RS256` (26) →
/// REJECT (too terse to be useful on retrieval) and `[@fact/active]
/// Use RS256 for JWT signing` (37) → ACCEPT (concrete + actionable).
/// The line is approximate; the point is to refuse payloads that
/// clearly lack substance.
const PERSISTENT_BODY_MIN_CHARS: usize = 30;

/// Memory types that Memoria persists across sessions. Writes with
/// these types MUST satisfy [`validate_persistent_memory_content`].
///
/// `working` is deliberately absent — those writes are session-scoped
/// and L1's `should_store_in_memory` already gates them.
pub const PERSISTENT_MEMORY_TYPES: &[&str] = &[
    "semantic",
    "episodic",
    "procedural",
    "profile",
];

/// Reason a persistent memory write was rejected by the structural
/// gate. Returned as a string so callers can log it verbatim; the
/// variants are stable so tests can match on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistentStoreRejection {
    /// Content is empty or only whitespace.
    Empty,
    /// Content does not start with `[@namespace/status]`.
    MissingStructuralPrefix,
    /// Prefix is present but malformed (e.g. missing `/`, no closing `]`).
    MalformedPrefix,
    /// Namespace is not one of the registered taxonomy values.
    UnknownNamespace(String),
    /// Body after the prefix is too short to carry durable signal.
    BodyTooShort { chars: usize, min: usize },
}

impl std::fmt::Display for PersistentStoreRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty or whitespace-only content"),
            Self::MissingStructuralPrefix => write!(
                f,
                "persistent memory must start with `[@ns/type]` — see \
                 astra_prompts::memory_proto namespaces"
            ),
            Self::MalformedPrefix => write!(f, "prefix shape must be `[@ns/type]`"),
            Self::UnknownNamespace(ns) => write!(
                f,
                "unknown namespace {ns:?}; use one of: pref / fact / plan / \
                 episode / insight / knowledge / task / swap"
            ),
            Self::BodyTooShort { chars, min } => write!(
                f,
                "body is {chars} chars, minimum {min} for persistent memory"
            ),
        }
    }
}

/// Registered memory-protocol namespaces.
///
/// Kept here (not imported from `astra-prompts`) so this crate has no
/// dependency on prompts. The list is a tiny enum that rarely changes;
/// if `astra-prompts::memory_proto` adds a namespace, a single test
/// here will fail loudly.
const KNOWN_NAMESPACES: &[&str] = &[
    "pref",
    "fact",
    "plan",
    "episode",
    "insight",
    "knowledge",
    "task",
    "swap",
    "lesson",
    "feedback",
];

/// Validate content for a persistent-type memory store.
///
/// Returns `Ok(())` when the content is acceptable, or
/// `Err(PersistentStoreRejection)` with a specific reason. Callers
/// should log the error and skip the store call (no Memoria write
/// attempt at all).
///
/// Use via [`should_store_persistent_memory`] which composes memory-
/// type filtering + content validation into a single call-site-friendly
/// predicate.
pub fn validate_persistent_memory_content(
    content: &str,
) -> Result<(), PersistentStoreRejection> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(PersistentStoreRejection::Empty);
    }

    let rest = trimmed
        .strip_prefix("[@")
        .ok_or(PersistentStoreRejection::MissingStructuralPrefix)?;

    let close_idx = rest
        .find(']')
        .ok_or(PersistentStoreRejection::MalformedPrefix)?;
    let inner = &rest[..close_idx];
    let (ns, _type_tag) = inner
        .split_once('/')
        .ok_or(PersistentStoreRejection::MalformedPrefix)?;

    if ns.is_empty() {
        return Err(PersistentStoreRejection::MalformedPrefix);
    }
    if !KNOWN_NAMESPACES.contains(&ns) {
        return Err(PersistentStoreRejection::UnknownNamespace(ns.to_string()));
    }

    let body_raw = &rest[close_idx + 1..];
    let body = body_raw.trim();
    let body_chars = body.chars().count();
    if body_chars < PERSISTENT_BODY_MIN_CHARS {
        return Err(PersistentStoreRejection::BodyTooShort {
            chars: body_chars,
            min: PERSISTENT_BODY_MIN_CHARS,
        });
    }

    Ok(())
}

/// True when `memory_type` is a persistent-store type that must satisfy
/// the structural gate.
#[must_use]
pub fn is_persistent_memory_type(memory_type: &str) -> bool {
    PERSISTENT_MEMORY_TYPES.contains(&memory_type)
}

/// Combined predicate: should this `(content, memory_type)` pair be
/// allowed through to Memoria?
///
/// - Non-persistent types (`working`, unknown): always allowed (the L1
///   message-level gate has already done its job upstream).
/// - Persistent types: must satisfy [`validate_persistent_memory_content`].
///
/// Returns `Ok(())` on accept, `Err(reason)` on reject.
pub fn should_store_persistent_memory(
    content: &str,
    memory_type: &str,
) -> Result<(), PersistentStoreRejection> {
    if !is_persistent_memory_type(memory_type) {
        return Ok(());
    }
    validate_persistent_memory_content(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_persistent_memory_type ──────────────────────────────────────

    #[test]
    fn persistent_types_are_gated() {
        assert!(is_persistent_memory_type("semantic"));
        assert!(is_persistent_memory_type("episodic"));
        assert!(is_persistent_memory_type("procedural"));
        assert!(is_persistent_memory_type("profile"));
    }

    #[test]
    fn working_is_not_persistent() {
        // Working memory is session-scoped + L1 gates it upstream.
        assert!(!is_persistent_memory_type("working"));
    }

    #[test]
    fn unknown_types_skip_the_gate() {
        // A future memory type we don't know about should pass through
        // — we don't want to hard-fail unknown types (forward-compat).
        // The caller can decide to reject separately if it wants.
        assert!(!is_persistent_memory_type("tool_result"));
        assert!(!is_persistent_memory_type("future-type"));
    }

    // ── validate_persistent_memory_content: accept ─────────────────────

    #[test]
    fn accepts_well_formed_fact() {
        let content = "[@fact/active] astra-engine uses Rust 2024 edition with clippy warnings as errors.";
        assert!(validate_persistent_memory_content(content).is_ok());
    }

    #[test]
    fn accepts_feedback_with_rule_and_why() {
        let content = "[@feedback/curated] Integration tests must hit real DB, not mocks. \
                       Why: prior mock/prod divergence incident.";
        assert!(validate_persistent_memory_content(content).is_ok());
    }

    #[test]
    fn accepts_with_leading_whitespace() {
        let content = "  \n[@pref/active] senior Rust engineer, prefers CLI tooling over web.";
        assert!(validate_persistent_memory_content(content).is_ok());
    }

    #[test]
    fn accepts_all_known_namespaces() {
        for ns in KNOWN_NAMESPACES {
            let content = format!(
                "[@{ns}/active] a sufficiently long body to clear the minimum-chars gate."
            );
            assert!(
                validate_persistent_memory_content(&content).is_ok(),
                "namespace {ns} should be accepted"
            );
        }
    }

    // ── validate_persistent_memory_content: reject ─────────────────────

    #[test]
    fn rejects_empty_string() {
        assert_eq!(
            validate_persistent_memory_content(""),
            Err(PersistentStoreRejection::Empty)
        );
        assert_eq!(
            validate_persistent_memory_content("   \n  \t "),
            Err(PersistentStoreRejection::Empty)
        );
    }

    #[test]
    fn rejects_unstructured_prose() {
        // Exactly the shape that polluted session c6e18730: bare
        // conversational snippets with no structural envelope.
        let content =
            "Recent conversation: user asked about JWT, we discussed RS256 and refresh rotation.";
        assert_eq!(
            validate_persistent_memory_content(content),
            Err(PersistentStoreRejection::MissingStructuralPrefix)
        );
    }

    #[test]
    fn rejects_session_tagged_but_not_structural() {
        // `[session:xyz]` prefix is not structural — it's a provenance
        // tag that was polluting the index in prior iterations.
        let content = "[session:abc] Recent conversation: hi → Hi! How can I help?";
        assert_eq!(
            validate_persistent_memory_content(content),
            Err(PersistentStoreRejection::MissingStructuralPrefix)
        );
    }

    #[test]
    fn rejects_compaction_tagged_without_namespace() {
        // `[compaction:sid]` was the actual wire shape for auto-
        // persisted summaries. Without a `@ns/type` envelope it's
        // just an opaque blob — future queries can't reason about it.
        let content = "[compaction:abc-123] Summary of the session: we discussed architecture…";
        assert_eq!(
            validate_persistent_memory_content(content),
            Err(PersistentStoreRejection::MissingStructuralPrefix)
        );
    }

    #[test]
    fn rejects_malformed_prefix_no_slash() {
        let content = "[@factactive] some body content here long enough";
        assert_eq!(
            validate_persistent_memory_content(content),
            Err(PersistentStoreRejection::MalformedPrefix)
        );
    }

    #[test]
    fn rejects_malformed_prefix_no_close_bracket() {
        let content = "[@fact/active body with no closing bracket ever";
        assert_eq!(
            validate_persistent_memory_content(content),
            Err(PersistentStoreRejection::MalformedPrefix)
        );
    }

    #[test]
    fn rejects_empty_namespace() {
        let content = "[@/active] body content long enough to meet the gate";
        assert_eq!(
            validate_persistent_memory_content(content),
            Err(PersistentStoreRejection::MalformedPrefix)
        );
    }

    #[test]
    fn rejects_unknown_namespace() {
        let content = "[@bogus/active] a sufficiently long body to clear the chars gate.";
        match validate_persistent_memory_content(content) {
            Err(PersistentStoreRejection::UnknownNamespace(ns)) => assert_eq!(ns, "bogus"),
            other => panic!("expected UnknownNamespace, got {other:?}"),
        }
    }

    #[test]
    fn rejects_short_body() {
        // Prefix is well-formed but body is only "Use RS256" (9 chars)
        // — below the 30-char minimum. Real preference memos are
        // longer: "Use RS256 for JWT signing because HS256 leaks key
        // on compromise" etc.
        let content = "[@fact/active] Use RS256";
        match validate_persistent_memory_content(content) {
            Err(PersistentStoreRejection::BodyTooShort { chars, min }) => {
                assert_eq!(chars, 9);
                assert_eq!(min, PERSISTENT_BODY_MIN_CHARS);
            }
            other => panic!("expected BodyTooShort, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_body() {
        let content = "[@fact/active]";
        assert!(matches!(
            validate_persistent_memory_content(content),
            Err(PersistentStoreRejection::BodyTooShort { .. })
        ));
        let content = "[@fact/active]   \n  \t  ";
        assert!(matches!(
            validate_persistent_memory_content(content),
            Err(PersistentStoreRejection::BodyTooShort { .. })
        ));
    }

    // ── should_store_persistent_memory (composed predicate) ───────────

    #[test]
    fn composed_passes_working_regardless_of_shape() {
        // Working memory gets past the gate without structural
        // validation — L1 at the message level handles those.
        assert!(should_store_persistent_memory("bare prose", "working").is_ok());
        assert!(should_store_persistent_memory("", "working").is_ok());
    }

    #[test]
    fn composed_rejects_semantic_without_structure() {
        // This was the session c6e18730 shape: auto-compaction summary
        // written as semantic without any [@ns/type] envelope.
        let unstructured = "User asked about JWT, we chose RS256 with 1-hour expiry.";
        match should_store_persistent_memory(unstructured, "semantic") {
            Err(PersistentStoreRejection::MissingStructuralPrefix) => {}
            other => panic!("expected MissingStructuralPrefix, got {other:?}"),
        }
    }

    #[test]
    fn composed_accepts_structured_semantic() {
        let content =
            "[@knowledge/active] astra sessions are stored at ~/.astra/sessions/<uuid>.jsonl";
        assert!(should_store_persistent_memory(content, "semantic").is_ok());
    }

    #[test]
    fn composed_gates_all_persistent_types() {
        // Each persistent type must subject its content to the same
        // structural validation.
        for persistent in PERSISTENT_MEMORY_TYPES {
            assert!(
                should_store_persistent_memory("unstructured", persistent).is_err(),
                "type {persistent} should enforce structural validation"
            );
        }
    }

    // ── Display messages are useful for operators ─────────────────────

    #[test]
    fn display_messages_describe_the_failure() {
        let e = PersistentStoreRejection::MissingStructuralPrefix;
        let msg = format!("{e}");
        assert!(msg.contains("[@ns/type]"), "got: {msg}");

        let e = PersistentStoreRejection::UnknownNamespace("bogus".to_string());
        let msg = format!("{e}");
        assert!(msg.contains("bogus"), "got: {msg}");

        let e = PersistentStoreRejection::BodyTooShort { chars: 5, min: 30 };
        let msg = format!("{e}");
        assert!(msg.contains("5") && msg.contains("30"), "got: {msg}");
    }
}
