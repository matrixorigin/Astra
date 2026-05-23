//! Structured memory entry protocol.
//!
//! # Wire format
//!
//! Every memory stored by astra follows this tagged format:
//!
//! ```text
//! [@<namespace>/<status>] <body>
//! ```
//!
//! Examples:
//! - `[@task/pending] Review PR #42`
//! - `[@plan/active] Finish API integration by Friday`
//! - `[@fact/semantic] User prefers Rust for CLI tools.`
//! - `[@episode/summary] ### Goals\nUser wants to fix auth...`
//! - `[@pref/active] Focus project: matrixorigin/memoria`
//! - `[@swap/archived] Swapped context from turn 5-12`
//! - `[@insight/active] User frequently works on Rust CLI + memory systems`
//!
//! The tag line is machine-parseable via regex. The body carries the
//! semantic content that Memoria indexes for similarity search. The
//! `memory_type` field sent to Memoria is derived from the namespace
//! via [`memory_ns::to_memory_type`].
//!
//! # Layered body
//!
//! The body is split into up to **three layers** by well-known
//! separators so the read path can slice just what it needs at
//! injection time, without a round-trip to the server:
//!
//! ```text
//! [@<namespace>/<status>] <abstract>
//!
//! <overview>
//! <!--layer:detail-->
//! <detail>
//! ```
//!
//! Layers:
//! | Layer      | Required | Size target        | Purpose                                               |
//! | ---------- | -------- | ------------------ | ----------------------------------------------------- |
//! | `abstract` | yes      | 30–150 scalars     | One-line gist. Always injected. Ranking signal.       |
//! | `overview` | optional | ~300–600 scalars   | Short paragraph for list views / `overview` view.     |
//! | `detail`   | optional | up to Memoria cap  | Full content: bullets, code, trace IDs, rationale.    |
//!
//! Separators are bytes, not regex — exact-match to keep splitting cheap
//! and reversible:
//! - `LAYER_SEP_OVERVIEW = "\n\n"` (ASCII blank line) separates abstract
//!   from overview. Overview ends at the first detail sentinel or EOF.
//! - `LAYER_SEP_DETAIL = "\n<!--layer:detail-->\n"` separates overview
//!   from detail. An HTML comment sentinel (instead of Markdown's
//!   `---`) so session-end learnings and compacted blocks can contain
//!   thematic breaks without ambiguity. The sentinel is invisible in
//!   most Markdown renderers, so layered bodies still read cleanly
//!   when inspected raw.
//!
//! **Hard-break from the single-body format:** entries must carry an
//! `abstract` layer that satisfies [`ABSTRACT_MIN_CHARS`] /
//! [`ABSTRACT_MAX_CHARS`]. The L2 write gate rejects entries without
//! a valid abstract — there is no fallback to "treat the whole body
//! as the abstract". This is deliberate: silent fallback would let
//! unsynthesized dumps land in the compact view and poison the
//! volatile cache lane.
//!
//! # Read-path slicing (simulates Memoria v2 views)
//!
//! Memoria v2 (unreleased, `open-memoria` branch commit `5eed8ef`,
//! crates: `memoria-api/src/v2/{router,models}.rs`) exposes three
//! recall views: `compact`, `overview`, `full`. We simulate those views
//! on the v1 `/memory/retrieve` endpoint by slicing the layered body
//! client-side:
//!
//! | v2 view     | v1.1 slice                         | Used by                                |
//! | ----------- | ---------------------------------- | -------------------------------------- |
//! | `compact`   | abstract only                      | Volatile system-prompt lane, ranking   |
//! | `overview`  | abstract + overview                | `introspect`, list views, UI summaries |
//! | `full`      | abstract + overview + detail       | Lazy `memory_expand`, debug traces     |
//!
//! When Memoria v2 ships, the server-side views will replace the
//! client-side slicing — the protocol stays the same, only the
//! transport changes. Callers that store layered bodies today will
//! light up v2's compact view automatically.
//!
//! # Writer obligations (abstract synthesis)
//!
//! Every writer MUST emit a valid abstract. "Valid" means:
//! - Length in `[ABSTRACT_MIN_CHARS, ABSTRACT_MAX_CHARS]` scalars.
//! - Single line (no `\n` — use overview/detail for structure).
//! - Self-contained (readable without the rest of the body, because
//!   that's what the compact view ships to the LLM).
//! - Specific (names the entity, action, or decision — not "notes
//!   about the session").
//!
//! Per-writer synthesis rules:
//!
//! | Writer                          | Layers emitted          | Abstract source                                                                                                       |
//! | ------------------------------- | ----------------------- | --------------------------------------------------------------------------------------------------------------------- |
//! | `memoria_compact.rs`            | abstract + detail       | LLM-synthesized in the same call that produces the compacted block. Prompt asks for "one-line topic < 150 chars".     |
//! | `session_end_governance.rs`     | abstract + overview + detail | Deterministic from section counts, e.g. `"Session <sid>: N corrections, M learnings, K decisions"`. No extra LLM call. |
//! | Short structural facts (pref, task, plan, insight) | abstract only | The fact *is* the abstract. Writer rejects its own input if it would exceed `ABSTRACT_MAX_CHARS`.             |
//! | External/unstructured           | abstract only           | Writer synthesizes via simple heuristic (first sentence, trimmed, padded if < MIN). If even that fails, the write is refused at L2 — we do not ship unsynthesized content into the compact view. |
//!
//! When an LLM is already in the loop (compact path), synthesis is
//! free — reuse the same call. When there is none (session-end),
//! prefer deterministic synthesis over adding latency; the overview
//! layer carries the readable narrative for anyone who expands it.
//!
//! # Future: corpus-level re-synthesis (`memory_reindex`)
//!
//! Abstracts are frozen at write time. Over many sessions, local
//! abstracts drift from the global narrative (e.g. ten session-end
//! entries each say "N corrections, M learnings" — fine individually,
//! noisy as a set). A future `memory_reindex` job re-synthesizes
//! abstracts in neighbor groups (clustered by retrieval) so the
//! compact view stays coherent corpus-wide. Out of scope for v1.1.
//!
//! # Constants at a glance
//!
//! - [`VERSION`] — `"v1.1"`. No back-compat with single-body v1.
//! - [`LAYER_SEP_OVERVIEW`], [`LAYER_SEP_DETAIL`] — the splitters.
//! - [`ABSTRACT_MIN_CHARS`], [`ABSTRACT_MAX_CHARS`] — enforced by the L2
//!   structural gate on write; violations are rejected, not silently
//!   truncated, so the layered promise holds.
use crate::memory_ns;

/// Protocol version tag.
///
/// Bumped from `"v1"` → `"v1.1"` when the layered body became
/// mandatory. No back-compat for single-body entries — the store is
/// still in early development, rewriting the few live writers is
/// cheaper than dragging a compatibility shim forward.
pub const VERSION: &str = "v1.1";

/// Separator between the `abstract` layer and the `overview` layer.
///
/// A single ASCII blank line (`\n\n`). Must appear *outside* code
/// fences in the abstract — the abstract layer is plain text by
/// contract, so this is safe.
pub const LAYER_SEP_OVERVIEW: &str = "\n\n";

/// Separator between the `overview` layer and the `detail` layer.
///
/// An HTML comment sentinel (`\n<!--layer:detail-->\n`). Chosen over
/// Markdown's `---` thematic break because detail often contains
/// Markdown (session-end learnings, compacted trace blocks) that
/// legitimately uses thematic breaks — a `---` splitter would
/// misfire. The comment is invisible in most Markdown renderers, so
/// layered bodies still read cleanly when inspected raw.
pub const LAYER_SEP_DETAIL: &str = "\n<!--layer:detail-->\n";

/// Minimum length of the `abstract` layer, in Unicode scalars.
///
/// Below this, the entry is too terse to be useful as a ranking
/// signal or compact-view one-liner. Enforced by the L2 write gate.
pub const ABSTRACT_MIN_CHARS: usize = 30;

/// Maximum length of the `abstract` layer, in Unicode scalars.
///
/// Above this, writers must push the extra content into the
/// `overview` or `detail` layer. The cap keeps the volatile system-
/// prompt lane small and cache-friendly. Enforced by the L2 write
/// gate; over-long abstracts are rejected so the layered promise
/// holds (silent truncation would lose the writer's chosen framing).
pub const ABSTRACT_MAX_CHARS: usize = 150;

// ── Namespace short names (used in tags) ─────────────────────────
pub const NS_TASK: &str = "task";
pub const NS_PLAN: &str = "plan";
pub const NS_FACT: &str = "fact";
pub const NS_EPISODE: &str = "episode";
pub const NS_PREF: &str = "pref";
pub const NS_SWAP: &str = "swap";
pub const NS_INSIGHT: &str = "insight";
pub const NS_KNOWLEDGE: &str = "knowledge";
pub const NS_LESSON: &str = "lesson";
pub const NS_FEEDBACK: &str = "feedback";
pub const NS_SESSION: &str = "session";

// ── Status values ────────────────────────────────────────────────
pub const ST_PENDING: &str = "pending";
pub const ST_ACTIVE: &str = "active";
pub const ST_DONE: &str = "done";
pub const ST_ARCHIVED: &str = "archived";
pub const ST_SUMMARY: &str = "summary";
pub const ST_AUTO: &str = "auto";

// ── Namespace → Memoria memory_type mapping ──────────────────────
/// Map a protocol namespace to a Memoria `memory_type`.
pub fn ns_to_memory_type(ns: &str) -> &'static str {
    match ns {
        NS_TASK => memory_ns::to_memory_type(memory_ns::TASK),
        NS_PLAN => memory_ns::to_memory_type(memory_ns::PLAN),
        NS_FACT => "semantic",
        NS_EPISODE => memory_ns::to_memory_type(memory_ns::EPISODIC),
        NS_PREF => memory_ns::to_memory_type(memory_ns::PREFERENCE),
        NS_SWAP => "working",
        NS_INSIGHT => "semantic",
        NS_KNOWLEDGE => memory_ns::to_memory_type(memory_ns::KNOWLEDGE),
        NS_LESSON => "semantic",
        NS_FEEDBACK => "semantic",
        NS_SESSION => "working",
        _ => "semantic",
    }
}

// ── Source identifiers for provenance tracking ─────────────
pub const SRC_USER: &str = "user";
pub const SRC_COMPACT: &str = "compact";
pub const SRC_AUTO_COMPACT: &str = "auto_compact";
pub const SRC_EXTRACTED: &str = "extracted";
pub const SRC_SYNTHESIS: &str = "synthesis";
pub const SRC_SYSTEM: &str = "system";

// ── Memoria trust tiers ──────────────────────────────────────
/// Verified: direct user-stated/confirmed facts (365-day half-life).
pub const TIER_VERIFIED: &str = "T1";
/// Curated: manually curated/corrected memories (180-day half-life).
pub const TIER_CURATED: &str = "T2";
/// Inferred: extracted summaries, soft conclusions (60-day half-life).
pub const TIER_INFERRED: &str = "T3";
/// Unverified: speculative/reflective hypotheses (30-day half-life).
pub const TIER_UNVERIFIED: &str = "T4";

/// Provenance metadata attached to memory entries.
///
/// Tracks when, where, and how a memory was created so that sessions
/// can be audited and entries traced back to their origin.
#[derive(Debug, Clone, Default)]
pub struct EntryMeta {
    /// Session that created the entry.
    pub session_id: Option<String>,
    /// Turn number within the session (1-based).
    pub turn: Option<u32>,
    /// How the entry was created (user, compact, extracted, synthesis, system).
    pub source: Option<String>,
    /// ISO 8601 timestamp.
    pub created_at: Option<String>,
    /// Memoria trust tier (T1–T4). Controls confidence decay half-life.
    pub trust_tier: Option<String>,
}

impl EntryMeta {
    /// Build metadata for a given session context.
    pub fn from_session(session_id: Option<&str>, turn: u32, source: &str) -> Self {
        Self {
            session_id: session_id.map(|s| s.to_string()),
            turn: Some(turn),
            source: Some(source.to_string()),
            created_at: Some(chrono::Utc::now().to_rfc3339()),
            trust_tier: None,
        }
    }

    /// Build metadata with an explicit trust tier.
    pub fn from_session_with_tier(
        session_id: Option<&str>,
        turn: u32,
        source: &str,
        trust_tier: &str,
    ) -> Self {
        Self {
            session_id: session_id.map(|s| s.to_string()),
            turn: Some(turn),
            source: Some(source.to_string()),
            created_at: Some(chrono::Utc::now().to_rfc3339()),
            trust_tier: Some(trust_tier.to_string()),
        }
    }

    /// Convert to JSON object for embedding in store payload.
    pub fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        if let Some(sid) = &self.session_id {
            obj.insert("session_id".into(), serde_json::Value::String(sid.clone()));
        }
        if let Some(t) = self.turn {
            obj.insert("turn".into(), serde_json::Value::Number(t.into()));
        }
        if let Some(src) = &self.source {
            obj.insert("source".into(), serde_json::Value::String(src.clone()));
        }
        if let Some(ts) = &self.created_at {
            obj.insert("created_at".into(), serde_json::Value::String(ts.clone()));
        }
        serde_json::Value::Object(obj)
    }
}

/// A parsed memory entry.
///
/// `body` is the raw layered body as stored — callers use
/// [`Self::abstract_layer`] / [`Self::overview_layer`] /
/// [`Self::detail_layer`] to slice it, or [`Self::compact_view`] /
/// [`Self::overview_view`] / [`Self::full_view`] to render the v2-
/// equivalent views.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEntry {
    /// Namespace short name (e.g. "task", "plan", "fact").
    pub ns: String,
    /// Status (e.g. "pending", "active", "done", "summary").
    pub status: String,
    /// The raw layered body: `abstract [\n\n overview] [\n<!--layer:detail-->\n detail]`.
    pub body: String,
}

/// Layers split out of a [`MemoryEntry`]'s body. Borrowed from the
/// entry so no allocation happens during slicing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyLayers<'a> {
    /// One-line gist. Always present in a well-formed entry.
    pub abstract_: &'a str,
    /// Short paragraph. `None` if the writer didn't emit one.
    pub overview: Option<&'a str>,
    /// Full content. `None` if the writer didn't emit one.
    pub detail: Option<&'a str>,
}

/// Split a raw layered body string into its three layers.
///
/// Grammar: `abstract [LAYER_SEP_OVERVIEW overview] [LAYER_SEP_DETAIL detail]`.
/// All four combinations are supported:
/// - abstract only
/// - abstract + overview
/// - abstract + detail (skips overview)
/// - abstract + overview + detail
///
/// First occurrence of each separator wins — writers must keep the
/// abstract free of `\n\n` and the overview free of the detail
/// sentinel. The L2 write gate enforces the abstract side; overview
/// hygiene is left to the writer contract.
pub fn split_body_layers(body: &str) -> BodyLayers<'_> {
    // Peel off detail first so a detail sentinel inside overview
    // can't happen (overview ends at the first sentinel by definition).
    let (head, detail) = match body.split_once(LAYER_SEP_DETAIL) {
        Some((h, d)) => (h, Some(d)),
        None => (body, None),
    };
    let (abstract_, overview) = match head.split_once(LAYER_SEP_OVERVIEW) {
        Some((a, o)) => (a, Some(o)),
        None => (head, None),
    };
    BodyLayers {
        abstract_,
        overview,
        detail,
    }
}

/// Assemble a layered body from its components.
///
/// Omitting `overview` and `detail` produces an abstract-only body.
/// The caller is responsible for abstract validity; the L2 gate
/// re-checks on write.
pub fn encode_body_layers(abstract_: &str, overview: Option<&str>, detail: Option<&str>) -> String {
    let mut out = String::with_capacity(
        abstract_.len()
            + overview.map_or(0, |o| o.len() + LAYER_SEP_OVERVIEW.len())
            + detail.map_or(0, |d| d.len() + LAYER_SEP_DETAIL.len()),
    );
    out.push_str(abstract_);
    if let Some(o) = overview {
        out.push_str(LAYER_SEP_OVERVIEW);
        out.push_str(o);
    }
    if let Some(d) = detail {
        out.push_str(LAYER_SEP_DETAIL);
        out.push_str(d);
    }
    out
}

impl MemoryEntry {
    /// Create an entry with a pre-built layered body.
    pub fn new(ns: &str, status: &str, body: &str) -> Self {
        Self {
            ns: ns.to_string(),
            status: status.to_string(),
            body: body.to_string(),
        }
    }

    /// Create an entry from discrete layers. Layers are assembled via
    /// [`encode_body_layers`] — prefer this over `new()` when you have
    /// the pieces separately, so you can't accidentally emit an
    /// unseparated body.
    pub fn new_layered(
        ns: &str,
        status: &str,
        abstract_: &str,
        overview: Option<&str>,
        detail: Option<&str>,
    ) -> Self {
        Self {
            ns: ns.to_string(),
            status: status.to_string(),
            body: encode_body_layers(abstract_, overview, detail),
        }
    }

    /// Split the body into its layers. Cheap — returns borrows.
    pub fn layers(&self) -> BodyLayers<'_> {
        split_body_layers(&self.body)
    }

    /// The abstract layer — always present in a well-formed entry.
    pub fn abstract_layer(&self) -> &str {
        self.layers().abstract_
    }

    /// The overview layer, if emitted.
    pub fn overview_layer(&self) -> Option<&str> {
        self.layers().overview
    }

    /// The detail layer, if emitted.
    pub fn detail_layer(&self) -> Option<&str> {
        self.layers().detail
    }

    /// Compact view: abstract only. The slice injected into the
    /// volatile system-prompt lane. Simulates Memoria v2's `compact`
    /// recall view.
    pub fn compact_view(&self) -> &str {
        self.abstract_layer()
    }

    /// Overview view: abstract + overview (if any). Simulates Memoria
    /// v2's `overview` recall view — used by `introspect`, list UIs,
    /// and the `overview_details` rendering in CLI flows.
    pub fn overview_view(&self) -> String {
        let layers = self.layers();
        match layers.overview {
            Some(o) => format!("{}{}{}", layers.abstract_, LAYER_SEP_OVERVIEW, o),
            None => layers.abstract_.to_string(),
        }
    }

    /// Full view: the entire body. Simulates Memoria v2's `full`
    /// recall view — used by lazy `memory_expand` and debug traces.
    pub fn full_view(&self) -> &str {
        &self.body
    }

    /// Encode to wire format: `[@ns/status] body`
    pub fn encode(&self) -> String {
        format!("[@{}/{}] {}", self.ns, self.status, self.body)
    }

    /// Get the Memoria `memory_type` for this entry.
    pub fn memory_type(&self) -> &'static str {
        ns_to_memory_type(&self.ns)
    }

    /// Build a JSON payload suitable for `POST /memory/store`.
    ///
    /// Without provenance metadata — use `to_store_payload_with_meta()` when
    /// session context is available.
    pub fn to_store_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "content": self.encode(),
            "memory_type": self.memory_type(),
        })
    }

    /// Build a JSON payload with provenance metadata.
    ///
    /// The `metadata` field carries session_id, turn, source, and timestamp
    /// so entries can be traced back to their origin for auditing.
    /// If the metadata includes a trust_tier, it is emitted as a top-level
    /// field for Memoria's confidence decay system.
    pub fn to_store_payload_with_meta(&self, meta: &EntryMeta) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "content": self.encode(),
            "memory_type": self.memory_type(),
            "metadata": meta.to_json(),
        });
        if let Some(ref tier) = meta.trust_tier {
            payload["trust_tier"] = serde_json::Value::String(tier.clone());
        }
        if let Some(ref sid) = meta.session_id {
            payload["session_id"] = serde_json::Value::String(sid.clone());
        }
        payload
    }

    /// Build a JSON payload for purging entries by namespace tag.
    pub fn purge_payload(ns: &str) -> serde_json::Value {
        serde_json::json!({
            "topic": format!("[@{}/", ns),
            "reason": format!("purge all {} entries", ns),
        })
    }

    /// Build a JSON payload for purging entries by namespace + status.
    pub fn purge_ns_status_payload(ns: &str, status: &str) -> serde_json::Value {
        serde_json::json!({
            "topic": format!("[@{}/{}]", ns, status),
            "reason": format!("purge {}/{} entries", ns, status),
        })
    }

    /// Build a search query optimized for finding entries in a namespace.
    pub fn search_query(ns: &str, extra_terms: &str) -> serde_json::Value {
        let query = if extra_terms.is_empty() {
            format!("[@{}/", ns)
        } else {
            format!("[@{}/] {}", ns, extra_terms)
        };
        serde_json::json!({
            "query": query,
            "top_k": 20,
        })
    }

    /// Parse from wire format. Returns None if the content doesn't match.
    ///
    /// Accepted format: `[@ns/status] body`
    pub fn parse(content: &str) -> Option<Self> {
        let trimmed = content.trim();

        if trimmed.starts_with("[@")
            && let Some(close) = trimmed.find(']')
        {
            let tag = &trimmed[2..close]; // "ns/status"
            let body = trimmed[close + 1..].trim().to_string();
            if let Some((ns, status)) = tag.split_once('/') {
                return Some(Self {
                    ns: ns.to_string(),
                    status: status.to_string(),
                    body,
                });
            }
        }

        None // unstructured memory — no tag
    }

    /// Check if this entry belongs to a given namespace.
    pub fn is_ns(&self, ns: &str) -> bool {
        self.ns == ns
    }

    /// Check if this entry has the given status.
    pub fn is_status(&self, status: &str) -> bool {
        self.status == status
    }

    /// Format for display in CLI (one-line summary).
    pub fn display_line(&self) -> String {
        let body_preview: String = self
            .body
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        format!("[{}/{}] {}", self.ns, self.status, body_preview)
    }

    /// Format for display grouped by status (for tasks).
    pub fn display_task_line(&self) -> String {
        let icon = match self.status.as_str() {
            ST_DONE => "✓",
            ST_PENDING => "○",
            _ => "·",
        };
        let body_preview: String = self
            .body
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(70)
            .collect();
        format!("{icon} {body_preview}")
    }
}

/// Filter a list of memory content strings to entries in a given namespace.
pub fn filter_ns(contents: &[&str], ns: &str) -> Vec<MemoryEntry> {
    contents
        .iter()
        .filter_map(|c| MemoryEntry::parse(c))
        .filter(|e| e.is_ns(ns))
        .collect()
}

/// Filter entries by namespace and status.
pub fn filter_ns_status(contents: &[&str], ns: &str, status: &str) -> Vec<MemoryEntry> {
    contents
        .iter()
        .filter_map(|c| MemoryEntry::parse(c))
        .filter(|e| e.is_ns(ns) && e.is_status(status))
        .collect()
}

/// Group entries from raw content strings by namespace.
///
/// Returns `(structured_entries, unstructured_texts)`.
/// Unstructured texts are memory strings that don't match any protocol format.
pub fn partition_memories(contents: &[&str]) -> (Vec<MemoryEntry>, Vec<String>) {
    let mut structured = Vec::new();
    let mut unstructured = Vec::new();
    for c in contents {
        if let Some(entry) = MemoryEntry::parse(c) {
            structured.push(entry);
        } else if !c.trim().is_empty() {
            unstructured.push(c.to_string());
        }
    }
    (structured, unstructured)
}

/// Format memory entries for injection into the LLM system prompt.
///
/// Groups entries by namespace and formats them readably.
pub fn format_for_llm(contents: &[&str]) -> String {
    let (entries, unstructured) = partition_memories(contents);
    let mut sections: Vec<String> = Vec::new();

    // Group by namespace
    let namespaces = [
        (NS_PREF, "Preferences"),
        (NS_FACT, "Knowledge"),
        (NS_KNOWLEDGE, "Knowledge"),
        (NS_PLAN, "Active Plan"),
        (NS_TASK, "Tasks"),
        (NS_INSIGHT, "Insights"),
        (NS_EPISODE, "Recent Context"),
        (NS_SESSION, "Session State"),
        (NS_SWAP, "Archived Context"),
    ];

    for (ns, label) in &namespaces {
        let ns_entries: Vec<_> = entries.iter().filter(|e| e.is_ns(ns)).collect();
        if ns_entries.is_empty() {
            continue;
        }
        if *ns == NS_TASK {
            let lines: Vec<String> = ns_entries.iter().map(|e| e.display_task_line()).collect();
            sections.push(format!("**{label}:** {}", lines.join(" | ")));
        } else {
            let bodies: Vec<String> = ns_entries
                .iter()
                .map(|e| {
                    let preview: String = e
                        .body
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(100)
                        .collect();
                    preview
                })
                .collect();
            sections.push(format!("**{label}:** {}", bodies.join(" | ")));
        }
    }

    if !unstructured.is_empty() {
        let previews: Vec<String> = unstructured
            .iter()
            .map(|s| s.chars().take(100).collect::<String>())
            .collect();
        sections.push(format!("**Context:** {}", previews.join(" | ")));
    }

    sections.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ──────────────────────────────────────────────────────────
    // ns_to_memory_type
    // ──────────────────────────────────────────────────────────

    #[test]
    fn ns_to_memory_type_known_namespaces() {
        assert_eq!(ns_to_memory_type(NS_TASK), "working");
        assert_eq!(ns_to_memory_type(NS_PLAN), "procedural");
        assert_eq!(ns_to_memory_type(NS_FACT), "semantic");
        assert_eq!(ns_to_memory_type(NS_EPISODE), "episodic");
        assert_eq!(ns_to_memory_type(NS_PREF), "profile");
        assert_eq!(ns_to_memory_type(NS_SWAP), "working");
        assert_eq!(ns_to_memory_type(NS_INSIGHT), "semantic");
        assert_eq!(ns_to_memory_type(NS_KNOWLEDGE), "semantic");
        assert_eq!(ns_to_memory_type(NS_LESSON), "semantic");
        assert_eq!(ns_to_memory_type(NS_FEEDBACK), "semantic");
        assert_eq!(ns_to_memory_type(NS_SESSION), "working");
    }

    #[test]
    fn ns_to_memory_type_unknown_fallback() {
        assert_eq!(ns_to_memory_type("bogus"), "semantic");
        assert_eq!(ns_to_memory_type(""), "semantic");
    }

    // ──────────────────────────────────────────────────────────
    // EntryMeta
    // ──────────────────────────────────────────────────────────

    #[test]
    fn entry_meta_from_session_populates_fields() {
        let m = EntryMeta::from_session(Some("s1"), 3, SRC_USER);
        assert_eq!(m.session_id, Some("s1".into()));
        assert_eq!(m.turn, Some(3));
        assert_eq!(m.source, Some(SRC_USER.into()));
        assert!(m.created_at.is_some());
    }

    #[test]
    fn entry_meta_from_session_none_session() {
        let m = EntryMeta::from_session(None, 1, SRC_COMPACT);
        assert_eq!(m.session_id, None);
        assert_eq!(m.turn, Some(1));
    }

    #[test]
    fn entry_meta_to_json_all_fields() {
        let m = EntryMeta {
            session_id: Some("s1".into()),
            turn: Some(2),
            source: Some("user".into()),
            created_at: Some("2025-01-01T00:00:00Z".into()),
            trust_tier: None,
        };
        let j = m.to_json();
        assert_eq!(j["session_id"], "s1");
        assert_eq!(j["turn"], 2);
        assert_eq!(j["source"], "user");
        assert_eq!(j["created_at"], "2025-01-01T00:00:00Z");
    }

    #[test]
    fn entry_meta_to_json_empty() {
        let m = EntryMeta::default();
        let j = m.to_json();
        let obj = j.as_object().unwrap();
        assert!(obj.is_empty());
    }

    // ──────────────────────────────────────────────────────────
    // MemoryEntry::new / encode / parse roundtrip
    // ──────────────────────────────────────────────────────────

    #[test]
    fn memory_entry_encode_format() {
        let e = MemoryEntry::new("task", "pending", "Review PR #42");
        assert_eq!(e.encode(), "[@task/pending] Review PR #42");
    }

    #[test]
    fn memory_entry_parse_roundtrip() {
        let original = MemoryEntry::new("plan", "active", "Finish API integration");
        let encoded = original.encode();
        let parsed = MemoryEntry::parse(&encoded).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn memory_entry_parse_with_leading_whitespace() {
        let e = MemoryEntry::parse("  [@fact/semantic] User prefers Rust  ").unwrap();
        assert_eq!(e.ns, "fact");
        assert_eq!(e.status, "semantic");
        assert_eq!(e.body, "User prefers Rust");
    }

    #[test]
    fn memory_entry_parse_no_tag() {
        assert!(MemoryEntry::parse("just plain text").is_none());
    }

    #[test]
    fn memory_entry_parse_malformed_tag_no_slash() {
        assert!(MemoryEntry::parse("[@noslash] body").is_none());
    }

    #[test]
    fn memory_entry_parse_empty_body() {
        let e = MemoryEntry::parse("[@task/done]").unwrap();
        assert_eq!(e.ns, "task");
        assert_eq!(e.status, "done");
        assert_eq!(e.body, "");
    }

    #[test]
    fn memory_entry_parse_empty_string() {
        assert!(MemoryEntry::parse("").is_none());
    }

    // ──────────────────────────────────────────────────────────
    // MemoryEntry methods
    // ──────────────────────────────────────────────────────────

    #[test]
    fn memory_entry_memory_type() {
        let e = MemoryEntry::new("task", "pending", "x");
        assert_eq!(e.memory_type(), "working");
    }

    #[test]
    fn memory_entry_is_ns() {
        let e = MemoryEntry::new("plan", "active", "x");
        assert!(e.is_ns("plan"));
        assert!(!e.is_ns("task"));
    }

    #[test]
    fn memory_entry_is_status() {
        let e = MemoryEntry::new("task", "done", "x");
        assert!(e.is_status("done"));
        assert!(!e.is_status("pending"));
    }

    // ──────────────────────────────────────────────────────────
    // to_store_payload / to_store_payload_with_meta
    // ──────────────────────────────────────────────────────────

    #[test]
    fn to_store_payload_shape() {
        let e = MemoryEntry::new("fact", "semantic", "Rust is fast");
        let p = e.to_store_payload();
        assert!(p["content"].as_str().unwrap().contains("[@fact/semantic]"));
        assert_eq!(p["memory_type"], "semantic");
    }

    #[test]
    fn to_store_payload_with_meta_includes_metadata() {
        let e = MemoryEntry::new("task", "pending", "do thing");
        let meta = EntryMeta {
            session_id: Some("s1".into()),
            turn: Some(1),
            source: Some("user".into()),
            created_at: None,
            trust_tier: None,
        };
        let p = e.to_store_payload_with_meta(&meta);
        assert_eq!(p["metadata"]["session_id"], "s1");
        assert_eq!(p["metadata"]["turn"], 1);
    }

    #[test]
    fn to_store_payload_with_meta_includes_trust_tier() {
        let e = MemoryEntry::new("fact", "semantic", "Rust is fast");
        let meta = EntryMeta::from_session_with_tier(Some("s1"), 3, SRC_USER, TIER_VERIFIED);
        let p = e.to_store_payload_with_meta(&meta);
        assert_eq!(p["trust_tier"], "T1");
        assert_eq!(p["session_id"], "s1");
        assert_eq!(p["metadata"]["source"], "user");
    }

    #[test]
    fn to_store_payload_with_meta_omits_trust_tier_when_none() {
        let e = MemoryEntry::new("task", "pending", "do thing");
        let meta = EntryMeta::from_session(Some("s1"), 1, SRC_COMPACT);
        let p = e.to_store_payload_with_meta(&meta);
        assert!(p.get("trust_tier").is_none());
        // session_id still emitted from meta
        assert_eq!(p["session_id"], "s1");
    }

    #[test]
    fn tier_constants_are_valid() {
        assert_eq!(TIER_VERIFIED, "T1");
        assert_eq!(TIER_CURATED, "T2");
        assert_eq!(TIER_INFERRED, "T3");
        assert_eq!(TIER_UNVERIFIED, "T4");
    }

    // ──────────────────────────────────────────────────────────
    // purge_payload / purge_ns_status_payload / search_query
    // ──────────────────────────────────────────────────────────

    #[test]
    fn purge_payload_format() {
        let p = MemoryEntry::purge_payload("task");
        assert_eq!(p["topic"], "[@task/");
        assert!(p["reason"].as_str().unwrap().contains("task"));
    }

    #[test]
    fn purge_ns_status_payload_format() {
        let p = MemoryEntry::purge_ns_status_payload("plan", "done");
        assert_eq!(p["topic"], "[@plan/done]");
    }

    #[test]
    fn search_query_no_extra() {
        let q = MemoryEntry::search_query("fact", "");
        assert_eq!(q["query"], "[@fact/");
        assert_eq!(q["top_k"], 20);
    }

    #[test]
    fn search_query_with_extra() {
        let q = MemoryEntry::search_query("task", "auth module");
        assert_eq!(q["query"], "[@task/] auth module");
    }

    // ──────────────────────────────────────────────────────────
    // display_line / display_task_line
    // ──────────────────────────────────────────────────────────

    #[test]
    fn display_line_truncates_long_body() {
        let body = "a".repeat(200);
        let e = MemoryEntry::new("fact", "semantic", &body);
        let line = e.display_line();
        assert!(line.len() < 200); // truncated to 80 chars + prefix
        assert!(line.starts_with("[fact/semantic]"));
    }

    #[test]
    fn display_task_line_done_icon() {
        let e = MemoryEntry::new("task", "done", "Fix bug");
        assert!(e.display_task_line().starts_with("✓"));
    }

    #[test]
    fn display_task_line_pending_icon() {
        let e = MemoryEntry::new("task", "pending", "Review code");
        assert!(e.display_task_line().starts_with("○"));
    }

    #[test]
    fn display_task_line_other_icon() {
        let e = MemoryEntry::new("task", "active", "In progress");
        assert!(e.display_task_line().starts_with("·"));
    }

    // ──────────────────────────────────────────────────────────
    // filter_ns / filter_ns_status
    // ──────────────────────────────────────────────────────────

    #[test]
    fn filter_ns_returns_matching() {
        let contents = vec![
            "[@task/pending] Task A",
            "[@fact/semantic] Fact B",
            "[@task/done] Task C",
            "plain text",
        ];
        let result = filter_ns(&contents, "task");
        assert_eq!(result.len(), 2);
        assert!(result[0].body.contains("Task A"));
        assert!(result[1].body.contains("Task C"));
    }

    #[test]
    fn filter_ns_empty_input() {
        let result = filter_ns(&[], "task");
        assert!(result.is_empty());
    }

    #[test]
    fn filter_ns_status_matches() {
        let contents = vec!["[@task/pending] A", "[@task/done] B", "[@task/pending] C"];
        let result = filter_ns_status(&contents, "task", "pending");
        assert_eq!(result.len(), 2);
    }

    // ──────────────────────────────────────────────────────────
    // partition_memories
    // ──────────────────────────────────────────────────────────

    #[test]
    fn partition_memories_splits_correctly() {
        let contents = vec![
            "[@task/pending] do thing",
            "unstructured note",
            "[@fact/semantic] know stuff",
            "   ", // whitespace-only → dropped
        ];
        let (structured, unstructured) = partition_memories(&contents);
        assert_eq!(structured.len(), 2);
        assert_eq!(unstructured.len(), 1);
        assert_eq!(unstructured[0], "unstructured note");
    }

    #[test]
    fn partition_memories_empty() {
        let (s, u) = partition_memories(&[]);
        assert!(s.is_empty());
        assert!(u.is_empty());
    }

    // ──────────────────────────────────────────────────────────
    // format_for_llm
    // ──────────────────────────────────────────────────────────

    #[test]
    fn format_for_llm_groups_by_namespace() {
        let contents = vec![
            "[@task/pending] Review PR",
            "[@task/done] Write tests",
            "[@fact/semantic] User prefers Rust",
        ];
        let result = format_for_llm(&contents);
        assert!(result.contains("**Knowledge:**"));
        assert!(result.contains("**Tasks:**"));
    }

    #[test]
    fn format_for_llm_includes_unstructured() {
        let contents = vec!["plain note"];
        let result = format_for_llm(&contents);
        assert!(result.contains("**Context:**"));
        assert!(result.contains("plain note"));
    }

    #[test]
    fn format_for_llm_empty() {
        assert!(format_for_llm(&[]).is_empty());
    }

    #[test]
    fn format_for_llm_tasks_use_icons() {
        let contents = vec!["[@task/done] Fixed it", "[@task/pending] Todo"];
        let result = format_for_llm(&contents);
        assert!(result.contains("✓"));
        assert!(result.contains("○"));
    }

    // ──────────────────────────────────────────────────────────
    // Layered body: split / encode / views
    // ──────────────────────────────────────────────────────────

    #[test]
    fn split_abstract_only() {
        let layers = split_body_layers("just the abstract");
        assert_eq!(layers.abstract_, "just the abstract");
        assert_eq!(layers.overview, None);
        assert_eq!(layers.detail, None);
    }

    #[test]
    fn split_abstract_and_overview() {
        let body = "the abstract\n\nthe overview continues here";
        let layers = split_body_layers(body);
        assert_eq!(layers.abstract_, "the abstract");
        assert_eq!(layers.overview, Some("the overview continues here"));
        assert_eq!(layers.detail, None);
    }

    #[test]
    fn split_abstract_and_detail_no_overview() {
        // Skipping overview is legal (compact writers do this).
        let body = "the abstract\n<!--layer:detail-->\nthe detail body";
        let layers = split_body_layers(body);
        assert_eq!(layers.abstract_, "the abstract");
        assert_eq!(layers.overview, None);
        assert_eq!(layers.detail, Some("the detail body"));
    }

    #[test]
    fn split_all_three_layers() {
        let body = "abs\n\novr\n<!--layer:detail-->\ndet";
        let layers = split_body_layers(body);
        assert_eq!(layers.abstract_, "abs");
        assert_eq!(layers.overview, Some("ovr"));
        assert_eq!(layers.detail, Some("det"));
    }

    #[test]
    fn detail_sentinel_not_confused_by_markdown_thematic_break() {
        // `---` inside detail used to collide with the old separator.
        // With the HTML-comment sentinel, a thematic break in detail
        // stays inside the detail layer.
        let body = "abs\n\novr\n<!--layer:detail-->\nbefore\n\n---\n\nafter";
        let layers = split_body_layers(body);
        assert_eq!(layers.abstract_, "abs");
        assert_eq!(layers.overview, Some("ovr"));
        assert_eq!(layers.detail, Some("before\n\n---\n\nafter"));
    }

    #[test]
    fn encode_layers_roundtrip_all_three() {
        let encoded = encode_body_layers("abs", Some("ovr"), Some("det"));
        let layers = split_body_layers(&encoded);
        assert_eq!(layers.abstract_, "abs");
        assert_eq!(layers.overview, Some("ovr"));
        assert_eq!(layers.detail, Some("det"));
    }

    #[test]
    fn encode_layers_abstract_only() {
        let encoded = encode_body_layers("just abstract", None, None);
        assert_eq!(encoded, "just abstract");
    }

    #[test]
    fn encode_layers_abstract_and_detail_skips_overview() {
        let encoded = encode_body_layers("abs", None, Some("det"));
        // Detail still attaches via its own separator — no empty
        // overview sandwich.
        assert_eq!(encoded, "abs\n<!--layer:detail-->\ndet");
        let layers = split_body_layers(&encoded);
        assert_eq!(layers.abstract_, "abs");
        assert_eq!(layers.overview, None);
        assert_eq!(layers.detail, Some("det"));
    }

    #[test]
    fn memory_entry_new_layered_emits_valid_body() {
        let e = MemoryEntry::new_layered(
            "episode",
            "summary",
            "Session sess1: 2 corrections, 3 learnings",
            Some("User asked to switch from black to ruff."),
            Some("- Use RS256\n- Don't use rm -rf"),
        );
        assert_eq!(
            e.abstract_layer(),
            "Session sess1: 2 corrections, 3 learnings"
        );
        assert_eq!(
            e.overview_layer(),
            Some("User asked to switch from black to ruff.")
        );
        assert_eq!(e.detail_layer(), Some("- Use RS256\n- Don't use rm -rf"));
    }

    #[test]
    fn memory_entry_compact_view_is_abstract() {
        let e = MemoryEntry::new_layered(
            "fact",
            "semantic",
            "User prefers Rust for CLI tools",
            Some("Mentioned while reviewing axum code."),
            None,
        );
        assert_eq!(e.compact_view(), "User prefers Rust for CLI tools");
    }

    #[test]
    fn memory_entry_overview_view_joins_abstract_and_overview() {
        let e = MemoryEntry::new_layered(
            "fact",
            "semantic",
            "abs line",
            Some("overview paragraph"),
            Some("detail not included in overview view"),
        );
        assert_eq!(e.overview_view(), "abs line\n\noverview paragraph");
    }

    #[test]
    fn memory_entry_overview_view_falls_back_to_abstract_when_absent() {
        let e = MemoryEntry::new_layered("fact", "semantic", "abs only", None, None);
        assert_eq!(e.overview_view(), "abs only");
    }

    #[test]
    fn memory_entry_full_view_returns_raw_body() {
        let e = MemoryEntry::new_layered(
            "episode",
            "summary",
            "abs",
            Some("ovr"),
            Some("det with\nmultiple lines"),
        );
        assert_eq!(
            e.full_view(),
            "abs\n\novr\n<!--layer:detail-->\ndet with\nmultiple lines"
        );
    }

    #[test]
    fn parse_and_slice_through_wire_format() {
        // End-to-end: parse `[@ns/status] body` then slice.
        let wire = "[@knowledge/curated] Session sess1: 1 corrections, 1 learnings\n\nUser flagged auth middleware.\n<!--layer:detail-->\n- Use RS256\n- Migrate sessions";
        let entry = MemoryEntry::parse(wire).unwrap();
        assert_eq!(entry.ns, "knowledge");
        assert_eq!(entry.status, "curated");
        assert_eq!(
            entry.abstract_layer(),
            "Session sess1: 1 corrections, 1 learnings"
        );
        assert_eq!(
            entry.overview_layer(),
            Some("User flagged auth middleware.")
        );
        assert_eq!(
            entry.detail_layer(),
            Some("- Use RS256\n- Migrate sessions")
        );
        // Compact view is what the volatile lane ships.
        assert_eq!(
            entry.compact_view(),
            "Session sess1: 1 corrections, 1 learnings"
        );
    }

    #[test]
    fn single_line_body_has_abstract_only() {
        // Legacy-style single-line entries still parse — they just
        // count as abstract-only. (L2 will reject them at write time
        // if the abstract doesn't meet MIN/MAX.)
        let entry =
            MemoryEntry::parse("[@pref/active] Focus project: matrixorigin/memoria").unwrap();
        assert_eq!(
            entry.abstract_layer(),
            "Focus project: matrixorigin/memoria"
        );
        assert!(entry.overview_layer().is_none());
        assert!(entry.detail_layer().is_none());
    }
}
