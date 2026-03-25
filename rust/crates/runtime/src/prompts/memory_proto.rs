/// Structured memory entry protocol (v1).
///
/// Every memory stored by mo-agent follows this wire format:
///
/// ```text
/// [@<namespace>/<status>] <body>
/// ```
///
/// Examples:
/// - `[@task/pending] Review PR #42`
/// - `[@plan/active] Finish API integration by Friday`
/// - `[@fact/semantic] User prefers Rust for CLI tools.`
/// - `[@episode/summary] ### Goals\nUser wants to fix auth...`
/// - `[@pref/active] Focus project: matrixorigin/memoria`
/// - `[@swap/archived] Swapped context from turn 5-12`
/// - `[@insight/active] User frequently works on Rust CLI + memory systems`
///
/// The tag line is machine-parseable via regex. The body carries the
/// semantic content that Memoria indexes for similarity search.
/// The `memory_type` field sent to Memoria is derived from the namespace
/// via `memory_ns::to_memory_type()`.
use super::memory_ns;

/// Protocol version tag for forward compatibility.
#[allow(dead_code)]
pub const VERSION: &str = "v1";

// ── Namespace short names (used in tags) ─────────────────────────
pub const NS_TASK: &str = "task";
pub const NS_PLAN: &str = "plan";
pub const NS_FACT: &str = "fact";
pub const NS_EPISODE: &str = "episode";
pub const NS_PREF: &str = "pref";
pub const NS_SWAP: &str = "swap";
pub const NS_INSIGHT: &str = "insight";
pub const NS_KNOWLEDGE: &str = "knowledge";

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
        _ => "semantic",
    }
}

// ── Source identifiers for provenance tracking ─────────────
pub const SRC_USER: &str = "user";
pub const SRC_COMPACT: &str = "compact";
pub const SRC_AUTO_COMPACT: &str = "auto_compact";
pub const SRC_EXTRACTED: &str = "extracted";
pub const SRC_SYNTHESIS: &str = "synthesis";
#[allow(dead_code)]
pub const SRC_SYSTEM: &str = "system";

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
}

impl EntryMeta {
    /// Build metadata for a given session context.
    pub fn from_session(session_id: Option<&str>, turn: u32, source: &str) -> Self {
        Self {
            session_id: session_id.map(|s| s.to_string()),
            turn: Some(turn),
            source: Some(source.to_string()),
            created_at: Some(chrono::Utc::now().to_rfc3339()),
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
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEntry {
    /// Namespace short name (e.g. "task", "plan", "fact").
    pub ns: String,
    /// Status (e.g. "pending", "active", "done", "summary").
    pub status: String,
    /// The semantic body content (everything after the tag line).
    pub body: String,
}

impl MemoryEntry {
    /// Create a new entry.
    pub fn new(ns: &str, status: &str, body: &str) -> Self {
        Self {
            ns: ns.to_string(),
            status: status.to_string(),
            body: body.to_string(),
        }
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
    #[allow(dead_code)]
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
    pub fn to_store_payload_with_meta(&self, meta: &EntryMeta) -> serde_json::Value {
        serde_json::json!({
            "content": self.encode(),
            "memory_type": self.memory_type(),
            "metadata": meta.to_json(),
        })
    }

    /// Build a JSON payload for purging entries by namespace tag.
    pub fn purge_payload(ns: &str) -> serde_json::Value {
        serde_json::json!({
            "topic": format!("[@{}/", ns),
            "reason": format!("purge all {} entries", ns),
        })
    }

    /// Build a JSON payload for purging entries by namespace + status.
    #[allow(dead_code)]
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
    /// Accepted formats:
    /// - `[@ns/status] body`        (v1 protocol)
    /// - `[ns:status] body`         (legacy, for backward compatibility)
    /// - `[ns:] body`               (legacy plan format)
    /// - `[Session Summary]\nbody`  (legacy compact format)
    /// - `[Auto-compact Summary]\n` (legacy auto-compact format)
    pub fn parse(content: &str) -> Option<Self> {
        let trimmed = content.trim();

        // ── v1 format: [@ns/status] body ──
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

        // ── Legacy: [Session Summary] or [Auto-compact Summary] ──
        if trimmed.starts_with("[Session Summary]") {
            let body = trimmed
                .strip_prefix("[Session Summary]")
                .unwrap_or("")
                .trim()
                .to_string();
            return Some(Self::new(NS_EPISODE, ST_SUMMARY, &body));
        }
        if trimmed.starts_with("[Auto-compact Summary]") {
            let body = trimmed
                .strip_prefix("[Auto-compact Summary]")
                .unwrap_or("")
                .trim()
                .to_string();
            return Some(Self::new(NS_EPISODE, ST_AUTO, &body));
        }

        // ── Legacy: [task:status] body ──
        if trimmed.starts_with("[task:")
            && let Some(close) = trimmed.find(']')
        {
            let status_part = &trimmed[6..close]; // e.g. "pending" or "done"
            let body = trimmed[close + 1..].trim().to_string();
            let status = if status_part.is_empty() {
                ST_PENDING
            } else {
                status_part
            };
            return Some(Self {
                ns: NS_TASK.to_string(),
                status: status.to_string(),
                body,
            });
        }

        // ── Legacy: [plan:] body ──
        if trimmed.starts_with("[plan:")
            && let Some(close) = trimmed.find(']')
        {
            let body = trimmed[close + 1..].trim().to_string();
            return Some(Self::new(NS_PLAN, ST_ACTIVE, &body));
        }

        None // unstructured memory — no tag
    }

    /// Check if this entry belongs to a given namespace.
    pub fn is_ns(&self, ns: &str) -> bool {
        self.ns == ns
    }

    /// Check if this entry has the given status.
    #[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
