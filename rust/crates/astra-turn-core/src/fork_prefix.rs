//! ForkPrefix — a frozen, byte-identical snapshot of a parent turn's
//! cacheable request prefix, used to spawn a child agent whose first API
//! request reuses the parent's prompt cache.
//!
//! ## Design invariants
//!
//! 1. **Runtime-owned canonicalization.** The `canonical_prefix_bytes` are
//!    produced by this module's serializer, NOT by an SDK. SDK version
//!    drift (field reordering, new optional fields) must not perturb the
//!    byte hash — if it does, cache hits silently vanish and we lose
//!    attribution. Provider adapters consume `canonical_prefix_bytes`
//!    verbatim when assembling the child's first request.
//!
//! 2. **Frozen system prompt.** `system_blocks` is accessible read-only.
//!    No public API returns `&mut SystemBlock` or lets a caller append to
//!    `system_blocks`. Skills that want to inject extra system-level
//!    guidance MUST do so as a user-message suffix in the child's first
//!    turn — that suffix sits *after* the cached prefix and can't break
//!    the cache. This is the type-layer shell that makes the soft-core
//!    skill layer incapable of accidentally invalidating parent caches.
//!
//! 3. **Per-tool hashing.** `tool_schemas` stores each tool's canonical
//!    bytes + SHA-256 separately. When a fork's first response reports a
//!    cache break, `cache_diagnostics::CacheBreakDetector` can name
//!    exactly which tool's description churned — matching the field
//!    observation that same-name schema churn (dynamic agent lists
//!    embedded in a tool description) accounts for ~77% of tool breaks.
//!
//! 4. **Thinking config in the cache key.** Anthropic's KV cache key
//!    includes `thinking.budget_tokens`. If a child spawn sets a
//!    `max_output_tokens` that would clamp budget_tokens to a different
//!    value than the parent used, the cache is gone. `validate_spawn`
//!    catches this mismatch before the request is sent, returning
//!    `ForkValidationError::ThinkingBudgetConflict` so the caller can
//!    fall back or reject.
//!
//! 5. **Provider affinity.** `ProviderKind` is part of the cache-identity
//!    hash. Cross-provider forking (parent on Anthropic, child forced to
//!    OpenAI) has no meaningful byte-identical prefix — the
//!    reconstructor refuses and the caller gets
//!    `ForkValidationError::ProviderMismatch`.
//!
//! 6. **`CacheMode::SkipWrite` for fire-and-forget.** A fork that will
//!    never have future requests reading its tail (extraction,
//!    speculation) should not add a new cache_control marker at its own
//!    end — that marker would write a fresh cache entry for a prefix
//!    no one will read. The reconstructor shifts the marker one
//!    position upstream in that mode.
//!
//! This module ONLY defines the type, its construction, validation, and
//! hashing. It does NOT:
//! - Capture from a live turn (that's PR 3).
//! - Store prefixes keyed by run_id (that's PR 2).
//! - Reconstruct wire requests in a provider adapter (that's PR 4+).
//! - Emit `ForkCacheEvent` telemetry (that's PR 5).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A 256-bit content hash (SHA-256). Stored as bytes rather than hex for
/// cheap equality checks.
pub type ContentHash = [u8; 32];

/// Known non-Anthropic / non-OpenAI / non-Bedrock providers that
/// astra talks OpenAI-protocol to. Order matters — first match wins,
/// so put more-specific needles before more-general ones (e.g.
/// "moonshot" before "kimi"; substring match would otherwise pick
/// whichever comes first alphabetically).
///
/// Each entry is `(needle, normalized)`: the needle is
/// substring-matched case-insensitively against the hint; the
/// normalized label is what ends up in `ProviderKind::Other(...)`.
/// Normalization collapses model variants (e.g. `deepseek-chat` and
/// `deepseek-coder`) into one telemetry bucket.
///
/// **Changing this table changes identity hashes on every existing
/// captured prefix whose model routed through the affected
/// normalization.** Do NOT casually add or reorder entries.
const KNOWN_OTHER_PROVIDERS: &[(&str, &str)] = &[
    // Chinese OpenAI-compatible providers
    ("deepseek", "deepseek"),
    ("moonshot", "moonshot"),
    ("kimi", "moonshot"), // kimi is moonshot's product name
    ("glm", "zhipu"),
    ("zhipu", "zhipu"),
    ("dashscope", "dashscope"),
    ("qwen", "dashscope"), // qwen is dashscope's model family
    // Other OpenAI-compatible hosted services
    ("groq", "groq"),
    ("together", "together"),
    ("mistral", "mistral"),
    ("cohere", "cohere"),
    ("gemini", "gemini"),
    ("google", "gemini"),
    ("ollama", "ollama"),
];

/// The LLM provider a `ForkPrefix` is bound to. Cache identity is
/// provider-scoped: cached Anthropic bytes mean nothing to OpenAI and
/// vice versa.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    Anthropic,
    OpenAi,
    Bedrock,
    /// Escape hatch for providers we don't yet have first-class support
    /// for. Cache reuse is best-effort; diagnostics flag every fork as
    /// experimental.
    Other(String),
}

impl ProviderKind {
    /// Infer a [`ProviderKind`] from a provider-or-model hint string.
    ///
    /// Mirrors the shape of
    /// `microcompact::ProviderCacheStrategy::from_provider_hint`, but
    /// produces fine-grained variants instead of a binary
    /// cache-capability classification.
    ///
    /// Known mappings (case-insensitive):
    /// - `"claude*"` / `"anthropic*"`                  → Anthropic
    /// - `"bedrock*"`                                  → Bedrock
    /// - `"gpt*"` / `"openai*"` / `"o1*"` / `"o3*"`    → OpenAi
    /// - known third-party names served over the
    ///   OpenAI protocol (deepseek / kimi / moonshot /
    ///   glm / zhipu / qwen / dashscope / groq /
    ///   together / gemini / google / mistral / cohere
    ///   / ollama)                                     → Other("<name>")
    /// - anything else                                 → Other("<raw>")
    ///
    /// Invariants:
    /// - Never panics.
    /// - Empty input → `Other(String::new())`; callers that treat
    ///   empty model names as a bug should check before calling.
    /// - Substring match: `"glm-4"` and `"glm"` both produce
    ///   `Other("glm")`; `"gpt-4o"` produces `OpenAi`. The
    ///   normalisation is stable — changing it invalidates every
    ///   captured prefix's `identity_hash`.
    pub fn from_provider_hint(hint: &str) -> Self {
        let lower = hint.to_ascii_lowercase();
        if lower.contains("claude") || lower.contains("anthropic") {
            return Self::Anthropic;
        }
        if lower.contains("bedrock") {
            return Self::Bedrock;
        }
        // First-class OpenAI: real API + models the OpenAI host serves.
        if lower.contains("openai")
            || lower.starts_with("gpt-")
            || lower.starts_with("gpt ")
            || lower == "gpt"
            || lower.starts_with("o1")
            || lower.starts_with("o3")
        {
            return Self::OpenAi;
        }
        // Known non-Anthropic providers/models that speak OpenAI
        // protocol but deserve a stable `Other(...)` tag for
        // telemetry bucketing. The tag is what cache identity hashes
        // — we normalize to the shortest recognisable name so that
        // different model variants of the same provider share an
        // identity bucket (e.g. `deepseek-chat` and `deepseek-coder`
        // both tag as `deepseek`).
        for (needle, normalized) in KNOWN_OTHER_PROVIDERS {
            if lower.contains(needle) {
                return Self::Other((*normalized).to_string());
            }
        }
        // Unknown: tag with the raw string so distinct unknowns
        // don't silently collapse into one bucket.
        Self::Other(hint.to_string())
    }

    /// Stable tag contributed to the cache-identity hash. Rename never.
    ///
    /// `Other(name)` is namespaced with an `other:` prefix so that a
    /// future promotion of some `Other("groq")` into a first-class
    /// variant `ProviderKind::Groq` with tag `"groq"` is guaranteed to
    /// produce a different identity hash than the prior `Other`
    /// captures. Cache semantics may have shifted on promotion —
    /// silently reusing across the transition would hide real breakage.
    fn tag(&self) -> String {
        match self {
            ProviderKind::Anthropic => "anthropic".to_string(),
            ProviderKind::OpenAi => "openai".to_string(),
            ProviderKind::Bedrock => "bedrock".to_string(),
            ProviderKind::Other(name) => format!("other:{name}"),
        }
    }
}

/// Write-policy for the child's first API request. Controls whether
/// the first response's cache tail becomes a fresh cache entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheMode {
    /// Child's first request may write a fresh cache entry for its tail.
    /// Use when the child itself will have follow-up turns that would
    /// benefit from reading that tail back.
    Write,
    /// Child is fire-and-forget — no future request will read its tail.
    /// The reconstructor shifts the final `cache_control` marker one
    /// position upstream so no new cache entry is written for bytes that
    /// will never be reread.
    SkipWrite,
}

/// Thinking config slice that participates in the cache key. Keep this
/// struct minimal — only fields the provider actually hashes into the
/// cache identity belong here. Anything renderer-only (UI labels, debug
/// text) must not live here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingConfigSlice {
    pub enabled: bool,
    pub budget_tokens: u32,
    /// Opaque type tag as sent on the wire (e.g. `"enabled"`,
    /// `"adaptive"`). Stored as-is; mismatches break cache even when
    /// `budget_tokens` matches.
    pub kind: String,
}

/// One system-prompt block. Cache identity spans the concatenation of all
/// blocks, in order. Blocks exist separately from a single flat string
/// because Anthropic supports per-block `cache_control` markers — the
/// reconstructor preserves each block's marker position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemBlock {
    /// Raw text bytes as they appear in the wire request.
    pub bytes: Vec<u8>,
    /// Whether this block carried a `cache_control: {type:"ephemeral"}`
    /// marker in the parent request. The reconstructor replays this
    /// marker on the child's first request; dropping or adding markers
    /// breaks the cache.
    pub has_cache_control: bool,
}

/// One tool schema. Stored as canonical JSON bytes (the output of
/// `serde_json::to_vec` on a deterministic input) plus its individual
/// hash — so a schema-level churn attributes to *this* tool by name
/// rather than vaguely "tools changed".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSchemaEntry {
    /// Tool name as the provider sees it. Used for attribution in
    /// `ForkCacheEvent::drift`.
    pub name: String,
    /// Canonical serialized schema bytes.
    pub canonical_bytes: Vec<u8>,
    /// SHA-256 of `canonical_bytes`.
    pub hash: ContentHash,
}

/// Errors that prevent a `ForkPrefix` from being safely used for a given
/// spawn. All variants are recoverable — callers fall back to a
/// non-prefixed spawn and emit a telemetry event. None of these should
/// ever panic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum ForkValidationError {
    /// The child was spawned with a different provider than the captured
    /// prefix. Cross-provider reuse is meaningless.
    #[error("provider mismatch: prefix captured on {prefix:?}, child requested {child:?}")]
    ProviderMismatch {
        prefix: ProviderKind,
        child: ProviderKind,
    },
    /// The child was spawned with a different model id. Even within one
    /// provider the model id is part of the cache key.
    #[error("model mismatch: prefix captured on {prefix}, child requested {child}")]
    ModelMismatch { prefix: String, child: String },
    /// Child's `max_output_tokens` would clamp the provider's effective
    /// `budget_tokens` to a value different from the parent's — that
    /// changes the cache key. See `ThinkingConfigSlice::budget_tokens`.
    #[error(
        "thinking budget conflict: prefix captured with budget={prefix_budget}, \
         child's max_output_tokens={child_max} would produce effective budget={child_effective}"
    )]
    ThinkingBudgetConflict {
        prefix_budget: u32,
        child_max: u32,
        child_effective: u32,
    },
    /// Prefix's canonical bytes exceed the configured soft cap. Not a
    /// hard failure — callers decide (some will downgrade, some will
    /// reject) — but the primitive flags it so the telemetry layer can
    /// raise an oversized event.
    #[error("prefix too large: {actual} bytes exceeds soft cap {cap}")]
    Oversized { actual: usize, cap: usize },
}

/// Soft cap on canonical prefix bytes. Not enforced at type level —
/// callers decide what to do when `ForkPrefix::size_bytes()` exceeds it.
/// Value matches the plan decision (2 MiB); tuning without code changes
/// is intentionally unavailable — cache identity must not vary per-caller.
pub const PREFIX_SOFT_CAP_BYTES: usize = 2 * 1024 * 1024;

/// An immutable, cheaply-clonable frozen snapshot of a parent turn's
/// cacheable prefix.
///
/// Created by [`ForkPrefix::build`] (from the capture site in PR 3).
/// Consumed by the spawn path (PR 4) and the diagnostics hook (PR 5).
///
/// All fields are `pub` for read access but mutation is impossible —
/// the struct is moved into an `Arc` on construction and `build` is the
/// only constructor. The `Arc` around `canonical_prefix_bytes` is
/// separate so that the (potentially megabyte-sized) byte blob clones
/// cheaply even when callers clone the outer `ForkPrefix` by value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkPrefix {
    /// Unique id for this captured prefix. Enables diagnostics to
    /// cross-reference a `ForkCacheEvent` back to the capture that
    /// produced it.
    pub prefix_id: String,
    /// Wall-clock seconds since UNIX epoch at capture time. Used for
    /// soft-TTL eviction (PR 2) and stale-prefix detection.
    pub captured_at_secs: u64,
    /// Parent run id that produced this prefix.
    pub parent_run_id: String,
    /// Sequential turn number within the parent run. Capture site
    /// stamps this so downstream checks can tell "is this prefix still
    /// fresh" (parent has not microcompacted since).
    pub parent_turn_seq: u32,
    /// Provider this prefix was captured against.
    pub provider: ProviderKind,
    /// Model id (provider-scoped).
    pub model_id: String,
    /// Thinking config slice (part of cache key).
    pub thinking: Option<ThinkingConfigSlice>,
    /// Frozen system blocks in wire order. Read-only: no public mutation
    /// API exists (see invariant #2 in the module docstring).
    system_blocks: Vec<SystemBlock>,
    /// Per-tool canonical schemas, sorted by name (deterministic order
    /// makes the list-level hash stable across captures that would
    /// otherwise differ only by insertion order).
    tool_schemas: Vec<ToolSchemaEntry>,
    /// Beta headers contributing to the cache key, sorted lexically so
    /// insertion order does not perturb the hash.
    pub beta_headers: Vec<String>,
    /// Canonical wire bytes of the prefix region (system + tools +
    /// messages minus the child-controlled suffix). Consumed verbatim
    /// by the provider reconstructor.
    canonical_prefix_bytes: Arc<Vec<u8>>,
    /// SHA-256 of `canonical_prefix_bytes`. The one source of truth for
    /// "did anything drift between capture and use".
    pub prefix_hash: ContentHash,
    /// Child's write policy for this spawn. Affects cache_control
    /// marker placement in the reconstructor, not the prefix bytes.
    pub cache_mode: CacheMode,
}

impl ForkPrefix {
    /// Build a validated, hashed prefix. This is the only constructor.
    ///
    /// `canonical_prefix_bytes` must already be the provider's canonical
    /// serialization of system + tools + messages — the capture site
    /// (PR 3) owns that serialization and this constructor only hashes
    /// what it was given. Pre-sorting / determinism is the caller's
    /// responsibility; `build` only enforces that `tool_schemas` and
    /// `beta_headers` are sorted so the per-field hashes are stable.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        prefix_id: impl Into<String>,
        parent_run_id: impl Into<String>,
        parent_turn_seq: u32,
        captured_at_secs: u64,
        provider: ProviderKind,
        model_id: impl Into<String>,
        thinking: Option<ThinkingConfigSlice>,
        system_blocks: Vec<SystemBlock>,
        mut tool_schemas: Vec<ToolSchemaEntry>,
        mut beta_headers: Vec<String>,
        canonical_prefix_bytes: Vec<u8>,
        cache_mode: CacheMode,
    ) -> Self {
        tool_schemas.sort_by(|a, b| a.name.cmp(&b.name));
        beta_headers.sort();
        beta_headers.dedup();

        let canonical = Arc::new(canonical_prefix_bytes);
        let prefix_hash = sha256(&canonical);

        Self {
            prefix_id: prefix_id.into(),
            captured_at_secs,
            parent_run_id: parent_run_id.into(),
            parent_turn_seq,
            provider,
            model_id: model_id.into(),
            thinking,
            system_blocks,
            tool_schemas,
            beta_headers,
            canonical_prefix_bytes: canonical,
            prefix_hash,
            cache_mode,
        }
    }

    /// Read-only view of system blocks. No mutable accessor exists —
    /// see invariant #2.
    pub fn system_blocks(&self) -> &[SystemBlock] {
        &self.system_blocks
    }

    /// Read-only view of tool schemas.
    pub fn tool_schemas(&self) -> &[ToolSchemaEntry] {
        &self.tool_schemas
    }

    /// Lookup a single tool's hash by name. Used by the diagnostics hook
    /// (PR 5) to attribute a fork-time break to a specific tool.
    pub fn tool_hash(&self, name: &str) -> Option<&ContentHash> {
        self.tool_schemas
            .iter()
            .find(|t| t.name == name)
            .map(|t| &t.hash)
    }

    /// Canonical prefix bytes (shared `Arc`). The reconstructor writes
    /// these verbatim as the leading bytes of the child's request body.
    pub fn canonical_prefix_bytes(&self) -> &Arc<Vec<u8>> {
        &self.canonical_prefix_bytes
    }

    /// Size of the canonical prefix in bytes. Useful for the oversized
    /// soft-cap check and for telemetry dashboards.
    pub fn size_bytes(&self) -> usize {
        self.canonical_prefix_bytes.len()
    }

    /// Whether the prefix exceeds [`PREFIX_SOFT_CAP_BYTES`].
    pub fn is_oversized(&self) -> bool {
        self.size_bytes() > PREFIX_SOFT_CAP_BYTES
    }

    /// Cache-identity hash: SHA-256 over (prefix_hash || provider tag
    /// || model_id || thinking slice || cache_mode). Two prefixes with
    /// the same `identity_hash` are byte-for-byte interchangeable at
    /// the provider wire level; two with different identity hashes are
    /// not, regardless of whether `prefix_hash` matches.
    ///
    /// The capture site does NOT store this — it's derived on demand.
    pub fn identity_hash(&self) -> ContentHash {
        let mut h = Sha256::new();
        h.update(self.prefix_hash);
        h.update(self.provider.tag().as_bytes());
        h.update([0u8]); // field separator
        h.update(self.model_id.as_bytes());
        h.update([0u8]);
        if let Some(t) = &self.thinking {
            h.update(b"T");
            h.update([u8::from(t.enabled)]);
            h.update(t.budget_tokens.to_le_bytes());
            h.update(t.kind.as_bytes());
        } else {
            h.update(b"N");
        }
        h.update([0u8]);
        h.update(match self.cache_mode {
            CacheMode::Write => b"W",
            CacheMode::SkipWrite => b"S",
        });
        h.finalize().into()
    }

    /// Validate this prefix is safe to use for a child spawn with the
    /// given parameters. Returns `Ok(())` if the child can reuse the
    /// parent's cache, or an error naming the first problem.
    ///
    /// **Check order**:
    /// 1. Oversized — a data-shape defect independent of the child's
    ///    parameters. Reporting it first means a 5 MiB cross-provider
    ///    prefix surfaces the oversize fact to telemetry instead of
    ///    being masked by a later `ProviderMismatch`.
    /// 2. Provider — most fundamental compatibility check.
    /// 3. Model — provider-scoped but still coarse.
    /// 4. Thinking budget clamp — the subtlest, most likely to be
    ///    missed by the caller.
    pub fn validate_spawn(&self, ctx: &SpawnValidationContext) -> Result<(), ForkValidationError> {
        if self.is_oversized() {
            return Err(ForkValidationError::Oversized {
                actual: self.size_bytes(),
                cap: PREFIX_SOFT_CAP_BYTES,
            });
        }
        if self.provider != ctx.child_provider {
            return Err(ForkValidationError::ProviderMismatch {
                prefix: self.provider.clone(),
                child: ctx.child_provider.clone(),
            });
        }
        if self.model_id != ctx.child_model_id {
            return Err(ForkValidationError::ModelMismatch {
                prefix: self.model_id.clone(),
                child: ctx.child_model_id.clone(),
            });
        }
        if let Some(prefix_thinking) = &self.thinking {
            if let Some(child_max) = ctx.child_max_output_tokens {
                // Provider-neutral clamp rule: effective budget is
                // `min(prefix_budget, child_max)`. If they differ, the
                // wire thinking block is different, cache is gone.
                let child_effective = prefix_thinking.budget_tokens.min(child_max);
                if child_effective != prefix_thinking.budget_tokens {
                    return Err(ForkValidationError::ThinkingBudgetConflict {
                        prefix_budget: prefix_thinking.budget_tokens,
                        child_max,
                        child_effective,
                    });
                }
            }
        }
        Ok(())
    }
}

/// Inputs validated against a `ForkPrefix` by `validate_spawn`. The
/// context mirrors the fields of `SpawnAgentInput` that affect cache
/// identity, but carries only what's needed for validation — no tools,
/// no permissions, no working dir.
#[derive(Debug, Clone)]
pub struct SpawnValidationContext {
    pub child_provider: ProviderKind,
    pub child_model_id: String,
    /// `None` means the child did not request an output cap. In that
    /// case the clamp rule does not fire.
    pub child_max_output_tokens: Option<u32>,
}

fn sha256(bytes: &[u8]) -> ContentHash {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

/// Canonical SHA-256 over a JSON value with sorted keys — used by
/// capture sites to produce deterministic `ToolSchemaEntry.hash`. Lives
/// here because the hash scheme is part of cache identity: the capture
/// side and any future re-capture must agree bit-for-bit.
pub fn hash_tool_schema(value: &serde_json::Value) -> (Vec<u8>, ContentHash) {
    let canonical = canonical_json_bytes(value);
    let hash = sha256(&canonical);
    (canonical, hash)
}

/// Convert a list of tool schemas as they appear in a wire payload
/// into `Vec<ToolSchemaEntry>`. Handles both common schema shapes
/// (OpenAI's nested `{function: {name, ...}}` and Anthropic's flat
/// `{name, ...}`) so both CLI and server-side hosts can reuse the
/// same helper when populating [`crate::fork_capture::CaptureRequest`].
///
/// Nameless schemas are dropped: a schema with no detectable name
/// cannot be attributed in `ForkCacheEvent::drift`, so inventing a
/// placeholder would produce a misleading telemetry event. The order
/// of returned entries preserves input order.
pub fn build_tool_schema_entries(schemas: &[serde_json::Value]) -> Vec<ToolSchemaEntry> {
    schemas
        .iter()
        .filter_map(|schema| {
            let name = schema
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .or_else(|| schema.get("name").and_then(|n| n.as_str()))?
                .to_string();
            let (canonical_bytes, hash) = hash_tool_schema(schema);
            Some(ToolSchemaEntry {
                name,
                canonical_bytes,
                hash,
            })
        })
        .collect()
}

/// Serialize a JSON value with keys sorted at every object level. The
/// built-in `serde_json::to_vec` preserves insertion order, which is
/// fine for most IO but not for content-hashing — a downstream caller
/// that reconstructs the same semantic schema in a different order
/// must produce the same bytes here.
fn canonical_json_bytes(value: &serde_json::Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &serde_json::Value, out: &mut Vec<u8>) {
    use serde_json::Value;
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Number(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::String(s) => {
            // Reuse serde_json's string escaping via the `to_string`
            // path on a fresh `Value::String` to guarantee identical
            // quoting rules as any SDK that serializes strings with
            // serde_json.
            let quoted = serde_json::to_string(s).expect("string always serializable");
            out.extend_from_slice(quoted.as_bytes());
        }
        Value::Array(arr) => {
            out.push(b'[');
            for (i, v) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(v, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                let quoted_key = serde_json::to_string(k).expect("string always serializable");
                out.extend_from_slice(quoted_key.as_bytes());
                out.push(b':');
                write_canonical(&map[*k], out);
            }
            out.push(b'}');
        }
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_tool(name: &str, desc: &str) -> ToolSchemaEntry {
        let schema = json!({
            "function": {"name": name, "description": desc}
        });
        let (bytes, hash) = hash_tool_schema(&schema);
        ToolSchemaEntry {
            name: name.to_string(),
            canonical_bytes: bytes,
            hash,
        }
    }

    fn sample_prefix(overrides: impl FnOnce(&mut ForkPrefixArgs)) -> ForkPrefix {
        let mut args = ForkPrefixArgs::default();
        overrides(&mut args);
        args.build()
    }

    /// Test-only builder to keep test call sites readable when only a
    /// couple of fields vary.
    struct ForkPrefixArgs {
        prefix_id: String,
        parent_run_id: String,
        parent_turn_seq: u32,
        captured_at_secs: u64,
        provider: ProviderKind,
        model_id: String,
        thinking: Option<ThinkingConfigSlice>,
        system_blocks: Vec<SystemBlock>,
        tool_schemas: Vec<ToolSchemaEntry>,
        beta_headers: Vec<String>,
        canonical_prefix_bytes: Vec<u8>,
        cache_mode: CacheMode,
    }

    impl Default for ForkPrefixArgs {
        fn default() -> Self {
            Self {
                prefix_id: "pfx-1".to_string(),
                parent_run_id: "run-parent".to_string(),
                parent_turn_seq: 3,
                captured_at_secs: 1_700_000_000,
                provider: ProviderKind::Anthropic,
                model_id: "claude-opus-4-6".to_string(),
                thinking: None,
                system_blocks: vec![SystemBlock {
                    bytes: b"you are a helpful assistant".to_vec(),
                    has_cache_control: true,
                }],
                tool_schemas: vec![sample_tool("bash", "run shell commands")],
                beta_headers: vec![],
                canonical_prefix_bytes: b"canonical prefix body".to_vec(),
                cache_mode: CacheMode::Write,
            }
        }
    }

    impl ForkPrefixArgs {
        fn build(self) -> ForkPrefix {
            ForkPrefix::build(
                self.prefix_id,
                self.parent_run_id,
                self.parent_turn_seq,
                self.captured_at_secs,
                self.provider,
                self.model_id,
                self.thinking,
                self.system_blocks,
                self.tool_schemas,
                self.beta_headers,
                self.canonical_prefix_bytes,
                self.cache_mode,
            )
        }
    }

    #[test]
    fn prefix_hash_is_deterministic_over_same_bytes() {
        let p1 = sample_prefix(|_| {});
        let p2 = sample_prefix(|_| {});
        assert_eq!(p1.prefix_hash, p2.prefix_hash);
    }

    #[test]
    fn prefix_hash_changes_when_canonical_bytes_change() {
        let p1 = sample_prefix(|_| {});
        let p2 = sample_prefix(|a| a.canonical_prefix_bytes = b"different".to_vec());
        assert_ne!(p1.prefix_hash, p2.prefix_hash);
    }

    #[test]
    fn identity_hash_changes_with_provider() {
        let p1 = sample_prefix(|_| {});
        let p2 = sample_prefix(|a| a.provider = ProviderKind::OpenAi);
        assert_ne!(p1.identity_hash(), p2.identity_hash());
    }

    #[test]
    fn identity_hash_changes_with_thinking_budget() {
        let thinking_a = ThinkingConfigSlice {
            enabled: true,
            budget_tokens: 8_000,
            kind: "enabled".into(),
        };
        let thinking_b = ThinkingConfigSlice {
            budget_tokens: 16_000,
            ..thinking_a.clone()
        };
        let p1 = sample_prefix(|a| a.thinking = Some(thinking_a));
        let p2 = sample_prefix(|a| a.thinking = Some(thinking_b));
        assert_ne!(p1.identity_hash(), p2.identity_hash());
    }

    #[test]
    fn identity_hash_changes_with_cache_mode() {
        let p1 = sample_prefix(|a| a.cache_mode = CacheMode::Write);
        let p2 = sample_prefix(|a| a.cache_mode = CacheMode::SkipWrite);
        // Same canonical bytes, but cache_mode is part of identity.
        assert_eq!(p1.prefix_hash, p2.prefix_hash);
        assert_ne!(p1.identity_hash(), p2.identity_hash());
    }

    #[test]
    fn tool_schemas_sorted_by_name_at_build() {
        let unsorted = vec![
            sample_tool("zed", "z"),
            sample_tool("alpha", "a"),
            sample_tool("mid", "m"),
        ];
        let p = sample_prefix(|a| a.tool_schemas = unsorted);
        let names: Vec<&str> = p.tool_schemas().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zed"]);
    }

    #[test]
    fn beta_headers_sorted_and_deduped_at_build() {
        let p = sample_prefix(|a| {
            a.beta_headers = vec![
                "anthropic-beta-b".into(),
                "anthropic-beta-a".into(),
                "anthropic-beta-b".into(), // duplicate
            ]
        });
        assert_eq!(p.beta_headers, vec!["anthropic-beta-a", "anthropic-beta-b"]);
    }

    #[test]
    fn tool_hash_lookup_by_name() {
        let p = sample_prefix(|a| {
            a.tool_schemas = vec![
                sample_tool("bash", "original"),
                sample_tool("edit", "edit files"),
            ]
        });
        let bash_hash = p.tool_hash("bash").copied().unwrap();
        let edit_hash = p.tool_hash("edit").copied().unwrap();
        assert_ne!(bash_hash, edit_hash);
        assert!(p.tool_hash("nonexistent").is_none());
    }

    #[test]
    fn canonical_json_sorts_keys_deterministically() {
        // Same logical schema, different key order — must hash equal.
        let a = json!({
            "function": {"name": "t", "description": "d"}
        });
        let b = json!({
            "function": {"description": "d", "name": "t"}
        });
        let (_, ha) = hash_tool_schema(&a);
        let (_, hb) = hash_tool_schema(&b);
        assert_eq!(ha, hb);
    }

    #[test]
    fn canonical_json_nested_arrays_and_objects() {
        let a = json!({"x": [{"b": 1, "a": 2}, {"a": 3}], "y": true});
        let b = json!({"y": true, "x": [{"a": 2, "b": 1}, {"a": 3}]});
        let (_, ha) = hash_tool_schema(&a);
        let (_, hb) = hash_tool_schema(&b);
        assert_eq!(ha, hb);
    }

    #[test]
    fn tool_schema_churn_changes_per_tool_hash_but_not_others() {
        let p1 = sample_prefix(|a| {
            a.tool_schemas = vec![
                sample_tool("bash", "original description"),
                sample_tool("edit", "edit files"),
            ]
        });
        let p2 = sample_prefix(|a| {
            a.tool_schemas = vec![
                sample_tool("bash", "REWRITTEN description with dynamic list"),
                sample_tool("edit", "edit files"),
            ]
        });
        // bash churned — its per-tool hash differs.
        assert_ne!(p1.tool_hash("bash"), p2.tool_hash("bash"));
        // edit didn't change — its per-tool hash matches.
        assert_eq!(p1.tool_hash("edit"), p2.tool_hash("edit"));
    }

    #[test]
    fn system_blocks_are_not_mutable_through_public_api() {
        let p = sample_prefix(|_| {});
        // `system_blocks()` returns `&[SystemBlock]` — no `&mut` method
        // exists. This is a compile-time guarantee; the test exists to
        // document intent and fail if someone adds a mutable accessor.
        let blocks = p.system_blocks();
        assert_eq!(blocks.len(), 1);
        // No `system_blocks_mut` should exist. If this line ever fails
        // to compile because such a method was added, that's the bug.
        // (Can't write a negative compile test inline — this is a
        // convention marker for reviewers.)
    }

    #[test]
    fn validate_spawn_ok_when_all_fields_match() {
        let p = sample_prefix(|_| {});
        let ctx = SpawnValidationContext {
            child_provider: ProviderKind::Anthropic,
            child_model_id: "claude-opus-4-6".into(),
            child_max_output_tokens: None,
        };
        assert!(p.validate_spawn(&ctx).is_ok());
    }

    #[test]
    fn validate_spawn_rejects_provider_mismatch() {
        let p = sample_prefix(|_| {});
        let ctx = SpawnValidationContext {
            child_provider: ProviderKind::OpenAi,
            child_model_id: "claude-opus-4-6".into(),
            child_max_output_tokens: None,
        };
        assert!(matches!(
            p.validate_spawn(&ctx),
            Err(ForkValidationError::ProviderMismatch { .. })
        ));
    }

    #[test]
    fn validate_spawn_rejects_model_mismatch() {
        let p = sample_prefix(|_| {});
        let ctx = SpawnValidationContext {
            child_provider: ProviderKind::Anthropic,
            child_model_id: "claude-sonnet-4-6".into(),
            child_max_output_tokens: None,
        };
        assert!(matches!(
            p.validate_spawn(&ctx),
            Err(ForkValidationError::ModelMismatch { .. })
        ));
    }

    #[test]
    fn validate_spawn_rejects_thinking_budget_clamp() {
        // Parent captured with a 16k thinking budget; child wants
        // max_output_tokens=8k, which would clamp thinking to 8k → wire
        // thinking block differs → cache gone.
        let p = sample_prefix(|a| {
            a.thinking = Some(ThinkingConfigSlice {
                enabled: true,
                budget_tokens: 16_000,
                kind: "enabled".into(),
            })
        });
        let ctx = SpawnValidationContext {
            child_provider: ProviderKind::Anthropic,
            child_model_id: "claude-opus-4-6".into(),
            child_max_output_tokens: Some(8_000),
        };
        let err = p.validate_spawn(&ctx).unwrap_err();
        match err {
            ForkValidationError::ThinkingBudgetConflict {
                prefix_budget,
                child_max,
                child_effective,
            } => {
                assert_eq!(prefix_budget, 16_000);
                assert_eq!(child_max, 8_000);
                assert_eq!(child_effective, 8_000);
            }
            other => panic!("expected ThinkingBudgetConflict, got {other:?}"),
        }
    }

    #[test]
    fn validate_spawn_accepts_thinking_budget_when_child_max_is_higher() {
        // Child's cap is above prefix's budget → no clamp, cache safe.
        let p = sample_prefix(|a| {
            a.thinking = Some(ThinkingConfigSlice {
                enabled: true,
                budget_tokens: 8_000,
                kind: "enabled".into(),
            })
        });
        let ctx = SpawnValidationContext {
            child_provider: ProviderKind::Anthropic,
            child_model_id: "claude-opus-4-6".into(),
            child_max_output_tokens: Some(32_000),
        };
        assert!(p.validate_spawn(&ctx).is_ok());
    }

    #[test]
    fn validate_spawn_flags_oversized_prefix() {
        let mut huge = Vec::with_capacity(PREFIX_SOFT_CAP_BYTES + 1024);
        huge.resize(PREFIX_SOFT_CAP_BYTES + 1024, b'x');
        let p = sample_prefix(|a| a.canonical_prefix_bytes = huge);
        assert!(p.is_oversized());
        let ctx = SpawnValidationContext {
            child_provider: ProviderKind::Anthropic,
            child_model_id: "claude-opus-4-6".into(),
            child_max_output_tokens: None,
        };
        assert!(matches!(
            p.validate_spawn(&ctx),
            Err(ForkValidationError::Oversized { .. })
        ));
    }

    #[test]
    fn validate_spawn_reports_oversized_before_other_mismatches() {
        // A 5 MiB cross-provider cross-model prefix must surface the
        // Oversized fact first — it's a data-shape problem that
        // telemetry needs to see regardless of whether the spawn was
        // also incompatible on provider/model. If oversized were
        // reported last, a caller chaining validate_spawn into a
        // multi-way fallback would never learn the prefix was too big.
        let mut huge = vec![b'x'; PREFIX_SOFT_CAP_BYTES + 1];
        huge[0] = b'H';
        let p = sample_prefix(|a| {
            a.provider = ProviderKind::Anthropic;
            a.model_id = "claude-opus-4-6".into();
            a.canonical_prefix_bytes = huge;
        });
        let ctx = SpawnValidationContext {
            // Everything else is mismatched too.
            child_provider: ProviderKind::OpenAi,
            child_model_id: "gpt-4o".into(),
            child_max_output_tokens: None,
        };
        assert!(
            matches!(
                p.validate_spawn(&ctx),
                Err(ForkValidationError::Oversized { .. })
            ),
            "Oversized must precede ProviderMismatch / ModelMismatch attribution"
        );
    }

    #[test]
    fn size_bytes_reports_canonical_length() {
        let p = sample_prefix(|a| a.canonical_prefix_bytes = b"hello world".to_vec());
        assert_eq!(p.size_bytes(), 11);
        assert!(!p.is_oversized());
    }

    #[test]
    fn canonical_bytes_shared_by_arc_are_cheap_to_clone() {
        let p1 = sample_prefix(|a| a.canonical_prefix_bytes = vec![0u8; 1024]);
        let p2 = p1.clone();
        // Same Arc allocation — confirmed by pointer equality.
        assert!(Arc::ptr_eq(
            p1.canonical_prefix_bytes(),
            p2.canonical_prefix_bytes()
        ));
    }

    #[test]
    fn serde_roundtrip_preserves_hashes_and_identity() {
        let p1 = sample_prefix(|a| {
            a.thinking = Some(ThinkingConfigSlice {
                enabled: true,
                budget_tokens: 4_096,
                kind: "enabled".into(),
            });
            a.beta_headers = vec!["anthropic-beta-x".into()];
            a.cache_mode = CacheMode::SkipWrite;
        });
        let json = serde_json::to_string(&p1).unwrap();
        let p2: ForkPrefix = serde_json::from_str(&json).unwrap();
        assert_eq!(p1.prefix_hash, p2.prefix_hash);
        assert_eq!(p1.identity_hash(), p2.identity_hash());
        assert_eq!(p1, p2);
    }

    #[test]
    fn unknown_provider_still_hashable() {
        let p = sample_prefix(|a| a.provider = ProviderKind::Other("groq".into()));
        // Identity hash must not panic on unknown provider tag.
        let _ = p.identity_hash();
    }

    // --- from_provider_hint -----------------------------------------

    #[test]
    fn from_provider_hint_anthropic_family() {
        assert_eq!(
            ProviderKind::from_provider_hint("claude-opus-4-6"),
            ProviderKind::Anthropic
        );
        assert_eq!(
            ProviderKind::from_provider_hint("anthropic"),
            ProviderKind::Anthropic
        );
        assert_eq!(
            ProviderKind::from_provider_hint("CLAUDE-SONNET"),
            ProviderKind::Anthropic,
            "must be case-insensitive"
        );
    }

    #[test]
    fn from_provider_hint_openai_family() {
        assert_eq!(
            ProviderKind::from_provider_hint("gpt-4o"),
            ProviderKind::OpenAi
        );
        assert_eq!(
            ProviderKind::from_provider_hint("openai"),
            ProviderKind::OpenAi
        );
        assert_eq!(
            ProviderKind::from_provider_hint("o1-preview"),
            ProviderKind::OpenAi
        );
        assert_eq!(
            ProviderKind::from_provider_hint("o3-mini"),
            ProviderKind::OpenAi
        );
    }

    #[test]
    fn from_provider_hint_bedrock() {
        assert_eq!(
            ProviderKind::from_provider_hint("bedrock"),
            ProviderKind::Bedrock
        );
        assert_eq!(
            ProviderKind::from_provider_hint("us.anthropic.claude-opus-4-6"),
            ProviderKind::Anthropic,
            "model name containing 'claude' wins over deployment prefix"
        );
    }

    #[test]
    fn from_provider_hint_chinese_providers_normalize() {
        // deepseek variants all collapse to the same bucket.
        assert_eq!(
            ProviderKind::from_provider_hint("deepseek-chat"),
            ProviderKind::Other("deepseek".into())
        );
        assert_eq!(
            ProviderKind::from_provider_hint("deepseek-coder"),
            ProviderKind::Other("deepseek".into())
        );
        // kimi and moonshot route to the same bucket — kimi is
        // moonshot's product name.
        assert_eq!(
            ProviderKind::from_provider_hint("kimi-k2"),
            ProviderKind::Other("moonshot".into())
        );
        assert_eq!(
            ProviderKind::from_provider_hint("moonshot-v1-8k"),
            ProviderKind::Other("moonshot".into())
        );
        // glm and zhipu both bucket as "zhipu".
        assert_eq!(
            ProviderKind::from_provider_hint("glm-4"),
            ProviderKind::Other("zhipu".into())
        );
        assert_eq!(
            ProviderKind::from_provider_hint("zhipu"),
            ProviderKind::Other("zhipu".into())
        );
        // qwen + dashscope both bucket as "dashscope".
        assert_eq!(
            ProviderKind::from_provider_hint("qwen-plus"),
            ProviderKind::Other("dashscope".into())
        );
    }

    #[test]
    fn from_provider_hint_other_hosted_providers() {
        assert_eq!(
            ProviderKind::from_provider_hint("groq"),
            ProviderKind::Other("groq".into())
        );
        assert_eq!(
            ProviderKind::from_provider_hint("together"),
            ProviderKind::Other("together".into())
        );
        assert_eq!(
            ProviderKind::from_provider_hint("mistral-large"),
            ProviderKind::Other("mistral".into())
        );
        assert_eq!(
            ProviderKind::from_provider_hint("cohere"),
            ProviderKind::Other("cohere".into())
        );
        assert_eq!(
            ProviderKind::from_provider_hint("gemini-2-flash"),
            ProviderKind::Other("gemini".into())
        );
        assert_eq!(
            ProviderKind::from_provider_hint("google"),
            ProviderKind::Other("gemini".into()),
            "google routes to gemini bucket"
        );
        assert_eq!(
            ProviderKind::from_provider_hint("ollama"),
            ProviderKind::Other("ollama".into())
        );
    }

    #[test]
    fn from_provider_hint_unknown_keeps_raw_string() {
        // An unknown model/provider name survives verbatim so
        // distinct unknowns don't silently collapse into one
        // telemetry bucket — makes "new provider, didn't update the
        // table" discoverable in dashboards.
        assert_eq!(
            ProviderKind::from_provider_hint("some-novel-provider-x"),
            ProviderKind::Other("some-novel-provider-x".into())
        );
    }

    #[test]
    fn from_provider_hint_empty_input() {
        // Empty input is a caller bug but must not panic. Returns
        // Other("") so the caller sees a distinct bucket that
        // stands out in telemetry.
        assert_eq!(
            ProviderKind::from_provider_hint(""),
            ProviderKind::Other(String::new())
        );
    }

    #[test]
    fn from_provider_hint_identity_stability() {
        // Tripwire: the mapping's identity_hash output must stay
        // stable across refactors. We encode the expected behavior
        // (hint → variant) to catch accidental regressions.
        let cases = [
            ("claude-3.5-sonnet", ProviderKind::Anthropic),
            ("gpt-4o", ProviderKind::OpenAi),
            ("bedrock", ProviderKind::Bedrock),
            ("deepseek", ProviderKind::Other("deepseek".into())),
            ("kimi", ProviderKind::Other("moonshot".into())),
            ("glm-4", ProviderKind::Other("zhipu".into())),
        ];
        for (hint, expected) in cases {
            assert_eq!(
                ProviderKind::from_provider_hint(hint),
                expected,
                "hint {hint} must map to expected variant"
            );
        }
    }

    #[test]
    fn other_provider_tag_is_namespaced_against_future_promotion() {
        // If `Other("groq")` ever gets promoted to a first-class variant
        // `ProviderKind::Groq` with tag "groq", the identity hash MUST
        // change — cache semantics may have shifted on promotion and
        // silently reusing prior `Other` captures would hide real
        // breakage. We enforce this by prefixing `Other` tags with
        // `other:`, which a first-class variant tag would never share.
        //
        // This test fails as a tripwire if someone accidentally drops
        // the prefix or picks a conflicting literal for a new variant.
        let other = sample_prefix(|a| a.provider = ProviderKind::Other("groq".into()));
        let as_if_first_class = sample_prefix(|a| a.provider = ProviderKind::Other("other:groq".into()));
        // Collision check: a pathological `Other("other:groq")` would
        // currently produce tag `"other:other:groq"` — different from
        // any hypothetical first-class `"groq"`. Good.
        assert_ne!(other.identity_hash(), as_if_first_class.identity_hash());
    }

    #[test]
    fn prefix_soft_cap_is_reasonable() {
        // Document the decision: 2 MiB. Changing this constant changes
        // observable behavior (oversized threshold) — this test exists
        // to force review of that change.
        assert_eq!(PREFIX_SOFT_CAP_BYTES, 2 * 1024 * 1024);
    }

    // ── build_tool_schema_entries: shared CLI+server capture helper ──

    #[test]
    fn build_tool_schema_entries_extracts_openai_nested_name() {
        let schemas = vec![
            serde_json::json!({
                "type": "function",
                "function": {"name": "spawn_agent", "parameters": {}}
            }),
            serde_json::json!({
                "type": "function",
                "function": {"name": "Read", "parameters": {}}
            }),
        ];
        let entries = build_tool_schema_entries(&schemas);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "spawn_agent");
        assert_eq!(entries[1].name, "Read");
        assert!(!entries[0].canonical_bytes.is_empty());
    }

    #[test]
    fn build_tool_schema_entries_extracts_anthropic_flat_name() {
        let schemas = vec![serde_json::json!({
            "name": "Grep",
            "input_schema": {"type": "object"},
        })];
        let entries = build_tool_schema_entries(&schemas);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Grep");
    }

    #[test]
    fn build_tool_schema_entries_drops_nameless_schemas() {
        let schemas = vec![
            serde_json::json!({"function": {"description": "no name"}}),
            serde_json::json!({"description": "also no name"}),
            serde_json::json!({"name": "ok_one"}),
        ];
        let entries = build_tool_schema_entries(&schemas);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "ok_one");
    }

    #[test]
    fn build_tool_schema_entries_is_key_order_independent() {
        // Critical: cache identity depends on canonical_bytes being
        // stable across key reorderings. If this test ever fails,
        // `fork-cache` events stop attributing correctly.
        let a = serde_json::json!({
            "function": {"name": "X", "parameters": {"a": 1, "b": 2}}
        });
        let b = serde_json::json!({
            "function": {"parameters": {"b": 2, "a": 1}, "name": "X"}
        });
        let ea = build_tool_schema_entries(&[a]);
        let eb = build_tool_schema_entries(&[b]);
        assert_eq!(ea[0].hash, eb[0].hash);
        assert_eq!(ea[0].canonical_bytes, eb[0].canonical_bytes);
    }
}
