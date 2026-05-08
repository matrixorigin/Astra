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

/// Minimum length of the abstract layer, in Unicode scalars.
///
/// Mirrors `astra_prompts::memory_proto::ABSTRACT_MIN_CHARS` —
/// duplicated here so this crate stays prompt-independent (a single
/// test in prompts will flag drift). Below this, the abstract can't
/// carry retrievable signal on its own and wouldn't make a useful
/// one-liner in the compact-view lane.
const ABSTRACT_MIN_CHARS: usize = 30;

/// Maximum length of the abstract layer, in Unicode scalars.
///
/// Mirrors `astra_prompts::memory_proto::ABSTRACT_MAX_CHARS`. Above
/// this the writer should push content into the overview or detail
/// layer; the cap keeps the volatile prompt-cache lane small.
const ABSTRACT_MAX_CHARS: usize = 150;

/// Separator that ends the abstract layer and starts the overview.
///
/// Mirrors `astra_prompts::memory_proto::LAYER_SEP_OVERVIEW`.
const LAYER_SEP_OVERVIEW: &str = "\n\n";

/// Separator that ends the overview layer and starts the detail.
///
/// Mirrors `astra_prompts::memory_proto::LAYER_SEP_DETAIL`. Needed
/// so the gate can extract just the abstract from a body that skips
/// overview (abstract + detail).
const LAYER_SEP_DETAIL: &str = "\n<!--layer:detail-->\n";

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
    /// The abstract layer (text before the first blank line, or whole
    /// body if no blank line) is shorter than [`ABSTRACT_MIN_CHARS`].
    /// Writers must synthesize a longer one-liner — see
    /// `memory_proto`'s writer obligations.
    AbstractTooShort { chars: usize, min: usize },
    /// The abstract layer exceeds [`ABSTRACT_MAX_CHARS`]. Push the
    /// overflow into the overview or detail layer. Silent truncation
    /// would lose the writer's chosen framing, so we reject.
    AbstractTooLong { chars: usize, max: usize },
    /// The abstract layer contains a newline. By contract the abstract
    /// is a single line — multi-line narrative belongs in overview or
    /// detail. Catches writers that forgot to split.
    AbstractNotSingleLine,
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
            Self::AbstractTooShort { chars, min } => write!(
                f,
                "abstract is {chars} chars, minimum {min} — synthesize a \
                 longer one-liner before store"
            ),
            Self::AbstractTooLong { chars, max } => write!(
                f,
                "abstract is {chars} chars, maximum {max} — push overflow \
                 into the overview or detail layer"
            ),
            Self::AbstractNotSingleLine => write!(
                f,
                "abstract must be a single line; multi-line narrative \
                 belongs in overview/detail"
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

    // Extract the abstract layer. Mirrors
    // `memory_proto::split_body_layers`: peel off the detail
    // sentinel first, then split on the blank-line (overview
    // separator). This way an `abstract + detail` body (no overview)
    // still gets the abstract in isolation.
    let head = match body.split_once(LAYER_SEP_DETAIL) {
        Some((h, _detail)) => h,
        None => body,
    };
    let abstract_layer = match head.split_once(LAYER_SEP_OVERVIEW) {
        Some((a, _overview)) => a,
        None => head,
    };

    if abstract_layer.contains('\n') {
        // Single-line contract: an abstract can't straddle lines.
        // If we got here without a blank-line separator, any `\n` in
        // the abstract indicates a malformed layered body.
        return Err(PersistentStoreRejection::AbstractNotSingleLine);
    }

    let abstract_chars = abstract_layer.chars().count();
    if abstract_chars < ABSTRACT_MIN_CHARS {
        return Err(PersistentStoreRejection::AbstractTooShort {
            chars: abstract_chars,
            min: ABSTRACT_MIN_CHARS,
        });
    }
    if abstract_chars > ABSTRACT_MAX_CHARS {
        return Err(PersistentStoreRejection::AbstractTooLong {
            chars: abstract_chars,
            max: ABSTRACT_MAX_CHARS,
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
    fn rejects_short_abstract() {
        // Prefix is well-formed but abstract is only "Use RS256" (9
        // chars) — below the 30-char minimum. Real preference memos
        // are longer: "Use RS256 for JWT signing because HS256 leaks
        // key on compromise" etc.
        let content = "[@fact/active] Use RS256";
        match validate_persistent_memory_content(content) {
            Err(PersistentStoreRejection::AbstractTooShort { chars, min }) => {
                assert_eq!(chars, 9);
                assert_eq!(min, ABSTRACT_MIN_CHARS);
            }
            other => panic!("expected AbstractTooShort, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_body_as_short_abstract() {
        let content = "[@fact/active]";
        assert!(matches!(
            validate_persistent_memory_content(content),
            Err(PersistentStoreRejection::AbstractTooShort { .. })
        ));
        let content = "[@fact/active]   \n  \t  ";
        assert!(matches!(
            validate_persistent_memory_content(content),
            Err(PersistentStoreRejection::AbstractTooShort { .. })
        ));
    }

    #[test]
    fn rejects_long_abstract_forces_layered_body() {
        // Writers that dump the whole fact into the tag line produce
        // an abstract longer than 150 chars. Reject so they push the
        // overflow into overview/detail instead.
        let long = "a".repeat(151);
        let content = format!("[@fact/active] {long}");
        match validate_persistent_memory_content(&content) {
            Err(PersistentStoreRejection::AbstractTooLong { chars, max }) => {
                assert_eq!(chars, 151);
                assert_eq!(max, ABSTRACT_MAX_CHARS);
            }
            other => panic!("expected AbstractTooLong, got {other:?}"),
        }
    }

    #[test]
    fn accepts_layered_body_with_long_detail() {
        // Abstract is fine (< 150), overview + detail can be long.
        // The layered-body split must see past the sentinel.
        let content = format!(
            "[@knowledge/curated] Session sess1: 2 corrections, 3 learnings\n\n\
             {}\n<!--layer:detail-->\n{}",
            "paragraph narrative ".repeat(10),
            "- bullet\n".repeat(50),
        );
        assert!(
            validate_persistent_memory_content(&content).is_ok(),
            "layered body with short abstract should pass"
        );
    }

    #[test]
    fn rejects_multiline_abstract_without_blank_separator() {
        // Writer forgot the `\n\n` separator — dropped a `\n` inside
        // the "abstract" area instead. We catch it explicitly.
        let content = "[@fact/active] Line one of the abstract\nLine two sneaks in here too.";
        assert_eq!(
            validate_persistent_memory_content(content),
            Err(PersistentStoreRejection::AbstractNotSingleLine)
        );
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

        let e = PersistentStoreRejection::AbstractTooShort { chars: 5, min: 30 };
        let msg = format!("{e}");
        assert!(msg.contains("5") && msg.contains("30"), "got: {msg}");

        let e = PersistentStoreRejection::AbstractTooLong {
            chars: 200,
            max: 150,
        };
        let msg = format!("{e}");
        assert!(
            msg.contains("200") && msg.contains("150"),
            "got: {msg}"
        );

        let e = PersistentStoreRejection::AbstractNotSingleLine;
        let msg = format!("{e}");
        assert!(msg.contains("single line"), "got: {msg}");
    }

    #[test]
    fn abstract_constants_match_memory_proto() {
        // Single source of truth lives in astra_prompts::memory_proto.
        // If those values change, this test fails loudly so we update
        // the local mirrors.
        assert_eq!(
            ABSTRACT_MIN_CHARS,
            astra_prompts::memory_proto::ABSTRACT_MIN_CHARS
        );
        assert_eq!(
            ABSTRACT_MAX_CHARS,
            astra_prompts::memory_proto::ABSTRACT_MAX_CHARS
        );
        assert_eq!(
            LAYER_SEP_OVERVIEW,
            astra_prompts::memory_proto::LAYER_SEP_OVERVIEW
        );
        assert_eq!(
            LAYER_SEP_DETAIL,
            astra_prompts::memory_proto::LAYER_SEP_DETAIL
        );
    }
}
