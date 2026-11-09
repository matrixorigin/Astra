//! Canonical token usage accounting across LLM providers.
//!
//! Every provider reports token usage differently. This module normalizes them
//! into a single [`TokenUsage`] struct whose invariants are provider-independent.
//!
//! # Semantics
//!
//! Billable input tokens for a single LLM call are partitioned into three
//! disjoint buckets:
//!
//! - `input_tokens`         — fresh input, billed at full input rate
//! - `cached_input_tokens`  — served from prompt cache, billed at a discount
//! - `cache_creation_tokens`— written to prompt cache, billed at a premium
//!
//! Plus `output_tokens`. These four numbers are disjoint and sum to `total_tokens`.
//!
//! # Per-provider quirks (all normalized here)
//!
//! - **OpenAI-compatible**: `usage.prompt_tokens` INCLUDES cached tokens; we
//!   derive fresh input only when cache-read and cache-write evidence closes
//!   the partition. An inclusive total remains a context measurement when
//!   billing lanes are unknown. Both supported nested write aliases normalize
//!   through the same checked partition.
//! - **OpenCode OpenAI-compatible extension**: `usage.opencode.tokens` reports
//!   disjoint `input`, `cache_read`, `output` (including reasoning), and cache
//!   writes by lifetime. Both write lifetimes must be present to prove their sum.
//! - **DeepSeek native OpenAI-compatible**: top-level
//!   `prompt_cache_hit_tokens` and `prompt_cache_miss_tokens` are already the
//!   cached/fresh partition of inclusive `prompt_tokens`.
//! - **Bedrock Converse**: `usage.inputTokens` EXCLUDES both
//!   `cacheReadInputTokens` and `cacheWriteInputTokens`. Use values directly.
//! - **Anthropic Messages**: `usage.input_tokens` EXCLUDES both
//!   `cache_read_input_tokens` and `cache_creation_input_tokens`. Use values
//!   directly.

use astra_turn_types::NormalizedPromptCacheUsage;
use serde_json::{Map, Value};

/// Normalized per-call token usage. All fields are disjoint buckets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
}

/// Records which normalized lanes the provider evidence proves. A zero
/// value is meaningful only when its lane is present here; absent lanes are
/// unavailable and must not be rendered as zero by Explain Analyze.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsagePresence {
    pub fresh_input_tokens: bool,
    pub cache_read_tokens: bool,
    pub cache_creation_tokens: bool,
    pub output_tokens: bool,
    /// Inclusive input reported for this physical request. This remains
    /// useful for context limits when cache billing lanes are incomplete.
    pub measured_input_tokens: Option<u64>,
    /// Irreversible raw evidence defects within one physical attempt.
    pub input_invalid: bool,
    pub output_invalid: bool,
}

impl TokenUsagePresence {
    pub fn merge(&mut self, other: Self) {
        self.fresh_input_tokens |= other.fresh_input_tokens;
        self.cache_read_tokens |= other.cache_read_tokens;
        self.cache_creation_tokens |= other.cache_creation_tokens;
        self.output_tokens |= other.output_tokens;
        if other.measured_input_tokens.is_some() {
            self.measured_input_tokens = other.measured_input_tokens;
        }
        self.input_invalid |= other.input_invalid;
        self.output_invalid |= other.output_invalid;
        if self.input_invalid {
            self.fresh_input_tokens = false;
            self.cache_read_tokens = false;
            self.cache_creation_tokens = false;
            self.measured_input_tokens = None;
        }
        if self.output_invalid {
            self.output_tokens = false;
        }
    }

    pub fn any(self) -> bool {
        (!self.input_invalid
            && (self.fresh_input_tokens || self.cache_read_tokens || self.cache_creation_tokens))
            || (!self.output_invalid && self.output_tokens)
    }
}

impl TokenUsage {
    /// Apply cumulative disjoint-lane updates within one physical request.
    /// Missing lanes retain their previous value; reported zero overwrites it.
    /// Not for inclusive OpenAI partitions, which require raw-payload reassembly.
    pub fn update_disjoint_lanes(
        &mut self,
        presence: &mut TokenUsagePresence,
        update: Self,
        observed: TokenUsagePresence,
    ) {
        for (target, value, known) in [
            (
                &mut self.input_tokens,
                update.input_tokens,
                observed.fresh_input_tokens,
            ),
            (
                &mut self.cached_input_tokens,
                update.cached_input_tokens,
                observed.cache_read_tokens,
            ),
            (
                &mut self.cache_creation_tokens,
                update.cache_creation_tokens,
                observed.cache_creation_tokens,
            ),
            (
                &mut self.output_tokens,
                update.output_tokens,
                observed.output_tokens,
            ),
        ] {
            if known {
                *target = value;
            }
        }
        presence.merge(observed);
        self.quarantine(presence);
        // Disjoint providers can update just one cumulative lane per frame.
        // Recompute from the retained qualified lanes, never from a previous
        // frame's sum or a partial update's zero-filled projection.
        presence.measured_input_tokens = if !presence.input_invalid
            && presence.fresh_input_tokens
            && presence.cache_read_tokens
            && presence.cache_creation_tokens
        {
            self.input_tokens
                .checked_add(self.cached_input_tokens)
                .and_then(|total| total.checked_add(self.cache_creation_tokens))
                .filter(|total| i64::try_from(*total).is_ok())
        } else {
            None
        };
    }

    fn quarantine(&mut self, presence: &mut TokenUsagePresence) {
        presence.merge(TokenUsagePresence::default());
        if presence.input_invalid {
            self.input_tokens = 0;
            self.cached_input_tokens = 0;
            self.cache_creation_tokens = 0;
        }
        if presence.output_invalid {
            self.output_tokens = 0;
        }
    }

    /// Project a copy so later cumulative updates can repair temporary overflow.
    pub fn qualified_snapshot(
        mut self,
        mut presence: TokenUsagePresence,
    ) -> (Self, TokenUsagePresence) {
        use astra_turn_types::CanonicalTokenUsage;
        self.quarantine(&mut presence);
        let input = presence.fresh_input_tokens.then_some(self.input_tokens);
        let read = presence
            .cache_read_tokens
            .then_some(self.cached_input_tokens);
        let creation = presence
            .cache_creation_tokens
            .then_some(self.cache_creation_tokens);
        let output = presence.output_tokens.then_some(self.output_tokens);
        let input_valid = CanonicalTokenUsage::new(input, read, creation, None).is_ok();
        let output_valid = CanonicalTokenUsage::new(None, None, None, output).is_ok();
        let combined_valid = !input_valid
            || !output_valid
            || CanonicalTokenUsage::new(input, read, creation, output).is_ok();
        if !input_valid || !combined_valid {
            self.input_tokens = 0;
            self.cached_input_tokens = 0;
            self.cache_creation_tokens = 0;
            presence.fresh_input_tokens = false;
            presence.cache_read_tokens = false;
            presence.cache_creation_tokens = false;
        }
        if !output_valid || !combined_valid {
            self.output_tokens = 0;
            presence.output_tokens = false;
        }
        (self, presence)
    }

    /// Emit only measured lanes. Numeric accounting buckets alone cannot prove
    /// that a missing provider field was zero or that their sum is complete.
    pub fn to_qualified_json_map(&self, presence: TokenUsagePresence) -> Map<String, Value> {
        let (snapshot, presence) = self.qualified_snapshot(presence);
        let usage = astra_turn_types::CanonicalTokenUsage::new(
            presence.fresh_input_tokens.then_some(snapshot.input_tokens),
            presence
                .cache_read_tokens
                .then_some(snapshot.cached_input_tokens),
            presence
                .cache_creation_tokens
                .then_some(snapshot.cache_creation_tokens),
            presence.output_tokens.then_some(snapshot.output_tokens),
        );
        match usage.map(astra_turn_types::CanonicalTokenUsage::to_json) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        }
    }

    pub fn normalized_prompt_cache_usage(self) -> NormalizedPromptCacheUsage {
        NormalizedPromptCacheUsage::new(
            self.input_tokens,
            self.cached_input_tokens,
            self.cache_creation_tokens,
        )
    }

    pub fn total_tokens(&self) -> u64 {
        self.normalized_prompt_cache_usage()
            .total_tokens_with_output(self.output_tokens)
    }

    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0
            && self.cached_input_tokens == 0
            && self.cache_creation_tokens == 0
            && self.output_tokens == 0
    }

    /// Serialize to the canonical JSON shape used across the codebase.
    pub fn to_json_map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("input_tokens".into(), Value::from(self.input_tokens));
        m.insert(
            "cached_input_tokens".into(),
            Value::from(self.cached_input_tokens),
        );
        m.insert(
            "cache_creation_tokens".into(),
            Value::from(self.cache_creation_tokens),
        );
        m.insert("output_tokens".into(), Value::from(self.output_tokens));
        m.insert("total_tokens".into(), Value::from(self.total_tokens()));
        m
    }

    /// Build usage from an internal partial map.
    ///
    /// This is intentionally tolerant because some runtime aggregation paths
    /// accumulate the canonical buckets incrementally. Persisted events must
    /// use the stricter DB-side canonical validation before writing token
    /// usage columns.
    pub fn from_partial_json_map(m: &Map<String, Value>) -> Self {
        let read = |k: &str| -> u64 {
            m.get(k)
                .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)))
                .unwrap_or(0)
        };
        Self {
            input_tokens: read("input_tokens"),
            cached_input_tokens: read("cached_input_tokens"),
            cache_creation_tokens: read("cache_creation_tokens"),
            output_tokens: read("output_tokens"),
        }
    }
}

/// Protocol dialect for a provider. Determines which extractor reads usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageDialect {
    /// OpenAI-compatible: `prompt_tokens`, `completion_tokens`,
    /// `prompt_tokens_details.cached_tokens`. `prompt_tokens` INCLUDES cache.
    OpenAi,
    /// Bedrock Converse: `inputTokens`, `outputTokens`, `cacheReadInputTokens`,
    /// `cacheWriteInputTokens`. `inputTokens` EXCLUDES cache.
    BedrockConverse,
    /// Anthropic Messages: `input_tokens`, `output_tokens`,
    /// `cache_read_input_tokens`, `cache_creation_input_tokens`.
    /// `input_tokens` EXCLUDES cache read/write buckets.
    AnthropicMessages,
}

impl UsageDialect {
    pub fn for_provider(provider: &str) -> Self {
        match provider {
            "bedrock" => Self::BedrockConverse,
            "anthropic" => Self::AnthropicMessages,
            _ => Self::OpenAi,
        }
    }
}

/// Extract a [`TokenUsage`] from the raw `usage` JSON object returned by the
/// provider (either a non-streaming response or a streaming chunk).
///
/// Accepts the object directly (caller has already navigated to `v["usage"]`).
/// Returns `None` only when no recognized tokens field is present at all.
pub fn extract_usage(dialect: UsageDialect, usage_obj: &Map<String, Value>) -> Option<TokenUsage> {
    parse_usage(dialect, usage_obj).map(|(usage, _)| usage)
}

/// Normalize one raw provider sample and its qualification together.
/// Irreversible raw defects must not be inferred from assembled stream state.
pub fn parse_usage(
    dialect: UsageDialect,
    usage_obj: &Map<String, Value>,
) -> Option<(TokenUsage, TokenUsagePresence)> {
    let (mut usage, mut presence) = parse_usage_sample(dialect, usage_obj, true)?;
    let (_, qualified) = usage.qualified_snapshot(presence);
    // A single raw sample outside the canonical representation domain is an
    // irreversible evidence defect. Cross-frame snapshots use the private
    // parser and remain repairable without discarding their retained fields.
    presence.input_invalid |= (presence.fresh_input_tokens
        || presence.cache_read_tokens
        || presence.cache_creation_tokens)
        && !(qualified.fresh_input_tokens
            || qualified.cache_read_tokens
            || qualified.cache_creation_tokens);
    presence.output_invalid |= presence.output_tokens && !qualified.output_tokens;
    usage.quarantine(&mut presence);
    Some((usage, presence))
}

fn parse_usage_sample(
    dialect: UsageDialect,
    usage_obj: &Map<String, Value>,
    raw_sample: bool,
) -> Option<(TokenUsage, TokenUsagePresence)> {
    match dialect {
        UsageDialect::OpenAi => parse_openai_usage(usage_obj, raw_sample),
        UsageDialect::BedrockConverse | UsageDialect::AnthropicMessages => {
            parse_disjoint_usage(dialect, usage_obj)
        }
    }
}

/// Report which normalized usage lanes are backed by fields in this provider
/// payload. This is separate from [`TokenUsage`] because its zero-filled
/// accounting buckets intentionally do not preserve field presence.
pub fn extract_usage_presence(
    dialect: UsageDialect,
    usage_obj: &Map<String, Value>,
) -> TokenUsagePresence {
    parse_usage(dialect, usage_obj)
        .map(|(_, presence)| presence)
        .unwrap_or_default()
}

/// Reassemble one OpenAI attempt while retaining irreversible raw defects.
pub fn update_openai_usage(
    accumulated: &mut Map<String, Value>,
    update: &Map<String, Value>,
    previous: TokenUsagePresence,
) -> Option<(TokenUsage, TokenUsagePresence)> {
    let raw = parse_usage(UsageDialect::OpenAi, update);
    merge_reported_usage_fields(accumulated, update);
    let assembled = parse_usage_sample(UsageDialect::OpenAi, accumulated, false);
    let (mut usage, mut presence) = assembled.or(raw)?;
    let raw_presence = raw.map(|(_, presence)| presence).unwrap_or_default();
    presence.input_invalid |= previous.input_invalid || raw_presence.input_invalid;
    presence.output_invalid |= previous.output_invalid || raw_presence.output_invalid;
    usage.quarantine(&mut presence);
    Some(usage.qualified_snapshot(presence))
}

fn merge_reported_usage_fields(target: &mut Map<String, Value>, update: &Map<String, Value>) {
    for (key, value) in update {
        if value.as_u64().is_some() || value.as_i64().is_some() {
            target.insert(key.clone(), value.clone());
        } else if let Some(object) = value.as_object() {
            let entry = target
                .entry(key.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(existing) = entry.as_object_mut() {
                merge_reported_usage_fields(existing, object);
            }
        }
    }
}

fn decode_count(value: Option<&Value>, invalid: &mut bool) -> Option<u64> {
    match value {
        None | Some(Value::Null) => None,
        Some(value) => {
            let count = value.as_u64().filter(|count| i64::try_from(*count).is_ok());
            *invalid |= count.is_none();
            count
        }
    }
}

fn reconcile_usage_aliases(values: &[Option<u64>], conflict: &mut bool) -> Option<u64> {
    let mut reported = values.iter().flatten().copied();
    let value = reported.next()?;
    *conflict |= reported.any(|other| other != value);
    Some(value)
}

fn parse_openai_usage(
    u: &Map<String, Value>,
    raw_sample: bool,
) -> Option<(TokenUsage, TokenUsagePresence)> {
    let mut input_invalid = false;
    let mut output_invalid = false;
    // Decode every reported alias independently. Null is absence, and a field
    // on the output side cannot select or suppress an input dialect.
    let prompt = decode_count(u.get("prompt_tokens"), &mut input_invalid);
    let native_input = decode_count(u.get("input_tokens"), &mut input_invalid);
    let hit = decode_count(u.get("prompt_cache_hit_tokens"), &mut input_invalid);
    let miss = decode_count(u.get("prompt_cache_miss_tokens"), &mut input_invalid);
    let top_read = decode_count(u.get("cache_read_input_tokens"), &mut input_invalid);
    let top_write = decode_count(u.get("cache_creation_input_tokens"), &mut input_invalid);
    let completion = decode_count(u.get("completion_tokens"), &mut output_invalid);
    let native_output = decode_count(u.get("output_tokens"), &mut output_invalid);
    let details = u.get("prompt_tokens_details").and_then(Value::as_object);
    input_invalid |= u
        .get("prompt_tokens_details")
        .is_some_and(|value| !value.is_null() && !value.is_object());
    let nested_read = decode_count(
        details.and_then(|d| d.get("cached_tokens")),
        &mut input_invalid,
    );
    let nested_write = decode_count(
        details.and_then(|d| d.get("cache_creation_input_tokens")),
        &mut input_invalid,
    );
    let cache_write = decode_count(
        details.and_then(|d| d.get("cache_write_tokens")),
        &mut input_invalid,
    );
    let extension = u.get("opencode");
    let tokens = extension.and_then(|value| value.get("tokens"));
    let malformed_extension = [extension, tokens]
        .into_iter()
        .flatten()
        .any(|value| !value.is_null() && !value.is_object());
    input_invalid |= malformed_extension;
    output_invalid |= malformed_extension;
    let opencode = tokens.and_then(Value::as_object);
    let decode_opencode = |key: &str, invalid: &mut bool| {
        decode_count(opencode.and_then(|tokens| tokens.get(key)), invalid)
    };
    let opencode_input = decode_opencode("input", &mut input_invalid);
    let opencode_read = decode_opencode("cache_read", &mut input_invalid);
    let write_5m = decode_opencode("cache_write_5m", &mut input_invalid);
    let write_1h = decode_opencode("cache_write_1h", &mut input_invalid);
    let opencode_write = write_5m.zip(write_1h).and_then(|(short, long)| {
        let total = short
            .checked_add(long)
            .filter(|total| i64::try_from(*total).is_ok());
        input_invalid |= total.is_none();
        total
    });
    let opencode_output = decode_opencode("output", &mut output_invalid);
    let mut output_conflict = false;
    let output = reconcile_usage_aliases(
        &[completion, native_output, opencode_output],
        &mut output_conflict,
    );
    output_invalid |= raw_sample && output_conflict;
    let native_partition = hit.is_some() || miss.is_some();
    let mut alias_conflict = false;
    let inclusive = native_partition
        || nested_read.is_some()
        || nested_write.is_some()
        || cache_write.is_some()
        || (top_read.is_none() && top_write.is_none());
    // These fields mirror the same lanes, not additional billable usage.
    // Cumulative snapshots arriving in different frames may be repaired later.
    let cached = reconcile_usage_aliases(
        &[hit, nested_read, top_read, opencode_read],
        &mut alias_conflict,
    );
    // Only an explicit native hit/miss pair proves a complete two-bucket partition.
    // A total and a read count alone cannot distinguish fresh input from writes.
    let creation = reconcile_usage_aliases(
        &[cache_write, nested_write, top_write, opencode_write],
        &mut alias_conflict,
    )
    .or_else(|| (hit.is_some() && miss.is_some()).then_some(0));
    // A single lifetime bounds writes but cannot establish the write total.
    let partial_write = write_5m.xor(write_1h);
    alias_conflict |= creation
        .zip(partial_write)
        .is_some_and(|(total, reported)| reported > total);
    let derived_fresh = || {
        prompt.and_then(|total| {
            cached
                .zip(creation)
                .and_then(|(read, write)| read.checked_add(write))
                .and_then(|cache| total.checked_sub(cache))
        })
    };
    let fresh = if native_partition {
        miss.or_else(derived_fresh)
    } else if inclusive {
        derived_fresh()
    } else {
        // Explicit top-level proxy cache lanes select the disjoint dialect.
        prompt
    };
    let fresh =
        reconcile_usage_aliases(&[fresh, native_input, opencode_input], &mut alias_conflict);
    let known_sum = [fresh, cached, creation.or(partial_write)]
        .into_iter()
        .flatten()
        .try_fold(0_u64, u64::checked_add);
    let exceeds_prompt =
        inclusive && prompt.is_some_and(|total| known_sum.is_none_or(|sum| sum > total));
    // A raw OpenCode lower bound can contradict the prompt even when the
    // write lane is unknown. A mismatch found only after assembly can recover.
    input_invalid |= raw_sample
        && (alias_conflict || (creation.is_none() && partial_write.is_some() && exceeds_prompt));
    let partition_conflict = exceeds_prompt
        || (hit.is_some() && miss.is_some() && creation.is_some_and(|write| write != 0))
        || (inclusive
            && fresh.is_some()
            && cached.is_some()
            && creation.is_some()
            && prompt.is_some_and(|total| known_sum != Some(total)));
    let mut usage = TokenUsage {
        input_tokens: fresh.unwrap_or(0),
        cached_input_tokens: cached.unwrap_or(0),
        cache_creation_tokens: creation.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
    };
    // Native OpenAI/DeepSeek reports an inclusive prompt total. A proxy that
    // supplies only top-level cache lanes reports fresh input separately;
    // its physical total is known only after both cache lanes are present.
    let measured_partition = || {
        fresh
            .zip(cached)
            .zip(creation)
            .and_then(|((fresh, read), write)| fresh.checked_add(read)?.checked_add(write))
            .filter(|total| i64::try_from(*total).is_ok())
    };
    let measured_input_tokens = if inclusive {
        prompt.or_else(measured_partition)
    } else {
        measured_partition()
    }
    .filter(|_| !input_invalid && !partition_conflict && !alias_conflict);
    if prompt.zip(output).is_some_and(|(input, output)| {
        input
            .checked_add(output)
            .is_none_or(|sum| i64::try_from(sum).is_err())
    }) {
        input_invalid = true;
        output_invalid = true;
    }
    let mut presence = TokenUsagePresence {
        fresh_input_tokens: fresh.is_some(),
        cache_read_tokens: cached.is_some(),
        cache_creation_tokens: creation.is_some(),
        output_tokens: output.is_some(),
        measured_input_tokens,
        input_invalid,
        output_invalid,
    };
    if !presence.any()
        && presence.measured_input_tokens.is_none()
        && !input_invalid
        && !output_invalid
    {
        return None;
    }
    usage.quarantine(&mut presence);
    // Assembled partition conflicts are reversible when a later cumulative
    // frame supplies the matching prompt total. Raw defects above are not.
    if partition_conflict || alias_conflict {
        usage.input_tokens = 0;
        usage.cached_input_tokens = 0;
        usage.cache_creation_tokens = 0;
        presence.fresh_input_tokens = false;
        presence.cache_read_tokens = false;
        presence.cache_creation_tokens = false;
        presence.measured_input_tokens = None;
    }
    if output_conflict {
        usage.output_tokens = 0;
        presence.output_tokens = false;
    }
    Some((usage, presence))
}

fn parse_disjoint_usage(
    dialect: UsageDialect,
    u: &Map<String, Value>,
) -> Option<(TokenUsage, TokenUsagePresence)> {
    let keys = match dialect {
        UsageDialect::BedrockConverse => [
            "inputTokens",
            "cacheReadInputTokens",
            "cacheWriteInputTokens",
            "outputTokens",
        ],
        UsageDialect::AnthropicMessages => [
            "input_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
            "output_tokens",
        ],
        UsageDialect::OpenAi => unreachable!("inclusive input requires OpenAI normalization"),
    };
    if !keys.iter().any(|key| u.contains_key(*key)) {
        return None;
    }
    let [
        (input, input_invalid),
        (cached, cached_invalid),
        (creation, creation_invalid),
        (output, output_invalid),
    ] = keys.map(|key| {
        let mut invalid = false;
        let count = decode_count(u.get(key), &mut invalid);
        (count, invalid)
    });
    let mut usage = TokenUsage {
        input_tokens: input.unwrap_or(0),
        cached_input_tokens: cached.unwrap_or(0),
        cache_creation_tokens: creation.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
    };
    let mut presence = TokenUsagePresence {
        fresh_input_tokens: input.is_some(),
        cache_read_tokens: cached.is_some(),
        cache_creation_tokens: creation.is_some(),
        output_tokens: output.is_some(),
        measured_input_tokens: input
            .and_then(|input| input.checked_add(cached?))
            .and_then(|input| input.checked_add(creation?))
            .filter(|total| i64::try_from(*total).is_ok()),
        input_invalid: input_invalid || cached_invalid || creation_invalid,
        output_invalid,
    };
    usage.quarantine(&mut presence);
    Some((usage, presence))
}

#[cfg(test)]
mod tests {
    #[test]
    fn null_openai_markers_do_not_hide_reported_alias_conflicts() {
        for generic in [
            serde_json::json!({"prompt_tokens":null,"completion_tokens":null,"input_tokens":4,"output_tokens":7}),
            serde_json::json!({"prompt_tokens":null,"completion_tokens":8,"input_tokens":4,"output_tokens":7}),
            serde_json::json!({"prompt_tokens":16,"completion_tokens":null,"input_tokens":4,"output_tokens":7}),
            serde_json::json!({"prompt_tokens":16,"completion_tokens":8,"input_tokens":4,"output_tokens":7}),
        ] {
            let mut raw = generic;
            raw["cache_read_input_tokens"] = serde_json::json!(6);
            raw["cache_creation_input_tokens"] = serde_json::json!(5);
            // The nested inclusive cache counts make prompt_tokens=16 agree
            // with OpenCode input=5; input_tokens=4 must still be checked.
            raw["prompt_tokens_details"] =
                serde_json::json!({"cached_tokens":6,"cache_write_tokens":5});
            raw["opencode"] = serde_json::json!({"tokens":{"input":5,"output":8,"cache_read":6,"cache_write_5m":2,"cache_write_1h":3}});
            let (usage, presence) =
                super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
            assert!(presence.input_invalid, "{raw}");
            assert!(presence.output_invalid, "{raw}");
            assert!(usage.to_qualified_json_map(presence).is_empty(), "{raw}");
        }
    }

    #[test]
    fn null_openai_markers_preserve_consistent_disjoint_evidence() {
        for (input, output, read, short, long) in [(4, 7, 6, 2, 3), (0, 0, 0, 0, 0)] {
            let raw = serde_json::json!({
                "prompt_tokens":null,"completion_tokens":null,
                "input_tokens":input,"output_tokens":output,
                "cache_read_input_tokens":read,"cache_creation_input_tokens":short+long,
                "opencode":{"tokens":{"input":input,"output":output,"cache_read":read,"cache_write_5m":short,"cache_write_1h":long}}
            });
            let (usage, presence) =
                super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
            assert_eq!(
                presence.measured_input_tokens,
                Some(input + read + short + long)
            );
            assert_eq!(
                serde_json::Value::Object(usage.to_qualified_json_map(presence)),
                serde_json::json!({
                    "input_tokens":input,"cached_input_tokens":read,"cache_creation_tokens":short+long,
                    "output_tokens":output,"total_tokens":input+read+short+long+output
                })
            );
        }
    }

    #[test]
    fn null_markers_cannot_make_raw_stream_alias_conflicts_repairable() {
        let contradictory = serde_json::json!({
            "prompt_tokens":null,"completion_tokens":null,"input_tokens":4,"output_tokens":7,
            "cache_read_input_tokens":6,"cache_creation_input_tokens":5,
            "opencode":{"tokens":{"input":5,"output":8,"cache_read":6,"cache_write_5m":2,"cache_write_1h":3}}
        });
        let mut accumulated = serde_json::Map::new();
        let (_, invalid) = super::update_openai_usage(
            &mut accumulated,
            contradictory.as_object().unwrap(),
            super::TokenUsagePresence::default(),
        )
        .unwrap();
        let matching = serde_json::json!({"opencode":{"tokens":{"input":4,"output":7}}});
        let (usage, presence) =
            super::update_openai_usage(&mut accumulated, matching.as_object().unwrap(), invalid)
                .unwrap();
        assert!(presence.input_invalid && presence.output_invalid);
        assert!(usage.to_qualified_json_map(presence).is_empty());
    }

    #[test]
    fn opencode_extension_preserves_existing_disjoint_dialects() {
        for generic in [
            serde_json::json!({"prompt_tokens":4,"completion_tokens":7,"cache_read_input_tokens":6,"cache_creation_input_tokens":5}),
            serde_json::json!({"input_tokens":4,"output_tokens":7}),
            serde_json::json!({"input_tokens":4,"output_tokens":7,"cache_read_input_tokens":6,"cache_creation_input_tokens":5}),
        ] {
            let mut raw = generic;
            raw["opencode"] = serde_json::json!({"tokens":{"input":4,"output":7,"cache_read":6,"cache_write_5m":2,"cache_write_1h":3}});
            let (usage, presence) =
                super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
            assert_eq!(presence.measured_input_tokens, Some(15), "{raw}");
            assert_eq!(
                serde_json::Value::Object(usage.to_qualified_json_map(presence)),
                serde_json::json!({
                    "input_tokens":4,"cached_input_tokens":6,"cache_creation_tokens":5,"output_tokens":7,"total_tokens":22
                }),
                "{raw}"
            );
        }
    }

    #[test]
    fn opencode_extension_checks_openai_disjoint_alias_conflicts() {
        for (key, count, invalid_input, invalid_output) in [
            ("input", 5, true, false),
            ("cache_read", 7, true, false),
            ("cache_write_5m", 3, true, false),
            ("output", 8, false, true),
        ] {
            let mut raw = serde_json::json!({
                "input_tokens":4,"output_tokens":7,"cache_read_input_tokens":6,"cache_creation_input_tokens":5,
                "opencode":{"tokens":{"input":4,"output":7,"cache_read":6,"cache_write_5m":2,"cache_write_1h":3}}
            });
            raw["opencode"]["tokens"][key] = serde_json::json!(count);
            let (_, presence) =
                super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
            assert_eq!(presence.input_invalid, invalid_input, "{raw}");
            assert_eq!(presence.output_invalid, invalid_output, "{raw}");
        }
    }

    #[test]
    fn opencode_usage_retains_reported_input_and_zero_writes() {
        let raw = serde_json::json!({
            "prompt_tokens": 17774,
            "completion_tokens": 1051,
            "prompt_tokens_details": {"cached_tokens": 9600},
            "opencode": {"tokens": {
                "input": 8174, "output": 1051, "reasoning": 537,
                "cache_read": 9600, "cache_write_5m": 0, "cache_write_1h": 0
            }}
        });
        let (usage, presence) =
            super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
        assert_eq!(presence.measured_input_tokens, Some(17774));
        assert_eq!(
            serde_json::Value::Object(usage.to_qualified_json_map(presence)),
            serde_json::json!({
                "input_tokens": 8174, "cached_input_tokens": 9600,
                "cache_creation_tokens": 0, "output_tokens": 1051, "total_tokens": 18825
            })
        );
    }

    #[test]
    fn opencode_usage_preserves_partial_and_nonzero_lanes() {
        for (tokens, expected, measured) in [
            (
                serde_json::json!({"input":4,"cache_read":6,"cache_write_5m":2,"cache_write_1h":3,"output":7}),
                serde_json::json!({"input_tokens":4,"cached_input_tokens":6,"cache_creation_tokens":5,"output_tokens":7,"total_tokens":22}),
                Some(15),
            ),
            (
                serde_json::json!({"input":0,"cache_read":0,"cache_write_5m":0,"cache_write_1h":0,"output":0}),
                serde_json::json!({"input_tokens":0,"cached_input_tokens":0,"cache_creation_tokens":0,"output_tokens":0,"total_tokens":0}),
                Some(0),
            ),
            (
                serde_json::json!({"input":4,"cache_read":6,"cache_write_5m":0,"output":7}),
                serde_json::json!({"input_tokens":4,"cached_input_tokens":6,"output_tokens":7}),
                None,
            ),
            (
                serde_json::json!({"cache_write_5m":null,"cache_write_1h":0,"output":7}),
                serde_json::json!({"output_tokens":7}),
                None,
            ),
        ] {
            let raw = serde_json::json!({"opencode":{"tokens":tokens}});
            let (usage, presence) =
                super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
            assert_eq!(presence.measured_input_tokens, measured);
            assert_eq!(
                serde_json::Value::Object(usage.to_qualified_json_map(presence)),
                expected
            );
        }
    }

    #[test]
    fn empty_opencode_metadata_preserves_existing_proxy_input_semantics() {
        for tokens in [serde_json::json!({}), serde_json::json!({"output":7})] {
            let raw = serde_json::json!({
                "prompt_tokens":4,"completion_tokens":7,
                "cache_read_input_tokens":6,"cache_creation_input_tokens":5,
                "opencode":{"tokens":tokens}
            });
            let (usage, presence) =
                super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
            assert_eq!(presence.measured_input_tokens, Some(15));
            assert_eq!(
                serde_json::Value::Object(usage.to_qualified_json_map(presence)),
                serde_json::json!({
                    "input_tokens":4,"cached_input_tokens":6,"cache_creation_tokens":5,"output_tokens":7,"total_tokens":22
                })
            );
        }
    }

    #[test]
    fn opencode_usage_rejects_invalid_counts_and_conflicting_input_evidence() {
        let valid = serde_json::json!({
            "prompt_tokens":15,"completion_tokens":7,
            "prompt_tokens_details":{"cached_tokens":6,"cache_write_tokens":5},
            "opencode":{"tokens":{"input":4,"cache_read":6,"cache_write_5m":2,"cache_write_1h":3,"output":7}}
        });
        for key in ["input", "cache_read", "cache_write_5m", "cache_write_1h"] {
            for invalid in [
                serde_json::json!(-1),
                serde_json::json!("bad"),
                serde_json::json!(u64::MAX),
            ] {
                let mut raw = valid.clone();
                raw["opencode"]["tokens"][key] = invalid;
                let (usage, presence) =
                    super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap())
                        .unwrap();
                assert!(presence.input_invalid, "{key}: {raw}");
                assert_eq!(
                    serde_json::Value::Object(usage.to_qualified_json_map(presence)),
                    serde_json::json!({"output_tokens":7})
                );
            }
        }
        for (key, mismatch) in [("input", 5), ("cache_read", 7), ("cache_write_5m", 3)] {
            let mut raw = valid.clone();
            raw["opencode"]["tokens"][key] = serde_json::json!(mismatch);
            let (_, presence) =
                super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
            assert!(presence.input_invalid, "conflicting {key}: {raw}");
        }
        let overflow = serde_json::json!({"opencode":{"tokens":{
            "input":0,"cache_read":0,"cache_write_5m":i64::MAX,"cache_write_1h":1,"output":7
        }}});
        let (_, presence) =
            super::parse_usage(super::UsageDialect::OpenAi, overflow.as_object().unwrap()).unwrap();
        assert!(presence.input_invalid);
        assert!(presence.output_tokens);
        let partial_conflict = serde_json::json!({
            "prompt_tokens":15,"completion_tokens":7,
            "prompt_tokens_details":{"cached_tokens":6,"cache_write_tokens":5},
            "opencode":{"tokens":{"input":4,"cache_write_5m":6}}
        });
        let (_, presence) = super::parse_usage(
            super::UsageDialect::OpenAi,
            partial_conflict.as_object().unwrap(),
        )
        .unwrap();
        assert!(presence.input_invalid);
        assert!(presence.output_tokens);
    }

    #[test]
    fn opencode_output_conflicts_do_not_discard_valid_input() {
        for output in [
            serde_json::json!(-1),
            serde_json::json!("bad"),
            serde_json::json!(8),
        ] {
            let raw = serde_json::json!({
                "prompt_tokens":15,"completion_tokens":7,
                "opencode":{"tokens":{"input":4,"cache_read":6,"cache_write_5m":2,"cache_write_1h":3,"output":output}}
            });
            let (usage, presence) =
                super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
            assert!(presence.output_invalid);
            assert!(!presence.input_invalid);
            assert_eq!(
                serde_json::Value::Object(usage.to_qualified_json_map(presence)),
                serde_json::json!({
                    "input_tokens":4,"cached_input_tokens":6,"cache_creation_tokens":5
                })
            );
        }
        for malformed in [serde_json::json!([]), serde_json::json!("bad")] {
            let raw = serde_json::json!({"prompt_tokens":15,"completion_tokens":7,"opencode":{"tokens":malformed}});
            let (_, presence) =
                super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
            assert!(presence.input_invalid && presence.output_invalid);
        }
    }

    #[test]
    fn opencode_partial_write_evidence_bounds_inclusive_input() {
        for lifetime in ["cache_write_5m", "cache_write_1h"] {
            let mut raw = serde_json::json!({
                "prompt_tokens":15,"completion_tokens":7,
                "opencode":{"tokens":{"input":4,"cache_read":6}}
            });
            raw["opencode"]["tokens"][lifetime] = serde_json::json!(6);
            let (usage, presence) =
                super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
            assert_eq!(
                serde_json::Value::Object(usage.to_qualified_json_map(presence)),
                serde_json::json!({"output_tokens":7}),
                "{raw}"
            );
            assert_eq!(presence.measured_input_tokens, None);

            raw["prompt_tokens"] = serde_json::json!(16);
            let (usage, presence) =
                super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
            assert_eq!(presence.measured_input_tokens, Some(16));
            assert_eq!(
                serde_json::Value::Object(usage.to_qualified_json_map(presence)),
                serde_json::json!({"input_tokens":4,"cached_input_tokens":6,"output_tokens":7}),
                "a matching lower bound does not prove the other lifetime was reported"
            );
        }
    }

    #[test]
    fn opencode_stream_snapshots_repair_cross_frame_conflicts_but_not_raw_conflicts() {
        let first = serde_json::json!({
            "prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":40},
            "opencode":{"tokens":{"input":60,"cache_read":40,"cache_write_5m":0,"cache_write_1h":0,"output":7}}
        });
        let mut accumulated = serde_json::Map::new();
        let mut previous = super::TokenUsagePresence::default();
        for (update, expected) in [
            (
                first.clone(),
                serde_json::json!({"input_tokens":60,"cached_input_tokens":40,"cache_creation_tokens":0,"output_tokens":7,"total_tokens":107}),
            ),
            (
                serde_json::json!({"opencode":{"tokens":{"input":50,"cache_read":50}}}),
                serde_json::json!({"output_tokens":7}),
            ),
            (
                serde_json::json!({"prompt_tokens_details":{"cached_tokens":50}}),
                serde_json::json!({"input_tokens":50,"cached_input_tokens":50,"cache_creation_tokens":0,"output_tokens":7,"total_tokens":107}),
            ),
            (
                serde_json::json!({"completion_tokens":8}),
                serde_json::json!({"input_tokens":50,"cached_input_tokens":50,"cache_creation_tokens":0}),
            ),
            (
                serde_json::json!({"opencode":{"tokens":{"output":8}}}),
                serde_json::json!({"input_tokens":50,"cached_input_tokens":50,"cache_creation_tokens":0,"output_tokens":8,"total_tokens":108}),
            ),
        ] {
            let (usage, presence) =
                super::update_openai_usage(&mut accumulated, update.as_object().unwrap(), previous)
                    .unwrap();
            assert_eq!(
                serde_json::Value::Object(usage.to_qualified_json_map(presence)),
                expected
            );
            previous = presence;
        }
        let mut contradictory = first.clone();
        contradictory["opencode"]["tokens"]["output"] = serde_json::json!(8);
        contradictory["opencode"]["tokens"]["cache_read"] = serde_json::json!(41);
        let mut accumulated = serde_json::Map::new();
        let (_, invalid) = super::update_openai_usage(
            &mut accumulated,
            contradictory.as_object().unwrap(),
            super::TokenUsagePresence::default(),
        )
        .unwrap();
        let (usage, still_invalid) =
            super::update_openai_usage(&mut accumulated, first.as_object().unwrap(), invalid)
                .unwrap();
        assert!(still_invalid.input_invalid && still_invalid.output_invalid);
        assert!(usage.to_qualified_json_map(still_invalid).is_empty());
    }

    #[test]
    fn measured_totals_do_not_complete_unknown_partitions() {
        for raw in [
            serde_json::json!({"prompt_tokens":100,"completion_tokens":7}),
            serde_json::json!({"prompt_tokens":100,"completion_tokens":7,"prompt_cache_hit_tokens":40}),
        ] {
            let (_, p) =
                super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
            assert_eq!(p.measured_input_tokens, Some(100));
            assert!(!p.fresh_input_tokens && !p.cache_creation_tokens);
        }
        let raw = serde_json::json!({"prompt_tokens":100,"completion_tokens":7,"prompt_cache_miss_tokens":80,"prompt_tokens_details":{"cached_tokens":10,"cache_write_tokens":30}});
        let (_, p) =
            super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
        assert!(!p.fresh_input_tokens && !p.cache_read_tokens && !p.cache_creation_tokens);
        assert!(p.output_tokens);
    }

    #[test]
    fn cache_write_alias_rejects_invalid_and_conflicting_evidence() {
        for value in [
            serde_json::json!(-1),
            serde_json::json!("bad"),
            serde_json::json!(u64::MAX),
            serde_json::json!(31),
        ] {
            let raw = serde_json::json!({"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":40,"cache_creation_input_tokens":30,"cache_write_tokens":value}});
            let (_, presence) =
                super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
            assert!(presence.input_invalid);
            assert!(!presence.cache_creation_tokens && !presence.fresh_input_tokens);
            assert!(presence.output_tokens);
        }
        let raw = serde_json::json!({"prompt_cache_hit_tokens":40,"prompt_cache_miss_tokens":60,"prompt_tokens_details":{"cache_write_tokens":30}});
        let (_, presence) =
            super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
        assert!(!presence.cache_creation_tokens && !presence.fresh_input_tokens);
    }

    #[test]
    fn openai_cache_write_alias_and_unknown_partition() {
        for field in ["cache_write_tokens", "cache_creation_input_tokens"] {
            let mut raw = serde_json::json!({"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":40}});
            raw["prompt_tokens_details"][field] = serde_json::json!(30);
            let (usage, presence) =
                super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
            assert_eq!(
                (
                    usage.input_tokens,
                    usage.cached_input_tokens,
                    usage.cache_creation_tokens
                ),
                (30, 40, 30)
            );
            assert!(presence.fresh_input_tokens && presence.cache_creation_tokens);
        }
        for write in [None, Some(serde_json::Value::Null)] {
            let mut raw = serde_json::json!({"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":40}});
            if let Some(write) = write {
                raw["prompt_tokens_details"]["cache_write_tokens"] = write;
            }
            let (usage, presence) =
                super::parse_usage(super::UsageDialect::OpenAi, raw.as_object().unwrap()).unwrap();
            assert_eq!(usage.cached_input_tokens, 40);
            assert_eq!(presence.measured_input_tokens, Some(100));
            assert!(!presence.fresh_input_tokens && !presence.cache_creation_tokens);
        }
    }

    #[test]
    fn inclusive_prompt_measurement_keeps_unknown_creation() {
        let partial = serde_json::json!({
            "prompt_tokens": 100_000,
            "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 90_000}
        });
        let (_, presence) =
            super::parse_usage(super::UsageDialect::OpenAi, partial.as_object().unwrap()).unwrap();
        assert_eq!(presence.measured_input_tokens, Some(100_000));
        assert!(!presence.cache_creation_tokens);
        assert!(!presence.fresh_input_tokens);

        for invalid in [
            serde_json::json!({"prompt_tokens": -1, "completion_tokens": 20}),
            serde_json::json!({"prompt_tokens": 100, "prompt_tokens_details": {"cached_tokens": 101}}),
        ] {
            let (_, presence) =
                super::parse_usage(super::UsageDialect::OpenAi, invalid.as_object().unwrap())
                    .unwrap();
            assert_eq!(presence.measured_input_tokens, None);
        }
    }

    #[test]
    fn openai_native_hit_miss_without_total_proves_zero_creation() {
        let (usage, presence) = super::parse_usage(
            super::UsageDialect::OpenAi,
            &obj(json!({
                "prompt_cache_hit_tokens": 90_000,
                "prompt_cache_miss_tokens": 10_000,
                "completion_tokens": 20
            })),
        )
        .unwrap();
        assert_eq!(usage.input_tokens, 10_000);
        assert_eq!(usage.cached_input_tokens, 90_000);
        assert_eq!(usage.cache_creation_tokens, 0);
        assert!(presence.fresh_input_tokens);
        assert!(presence.cache_read_tokens);
        assert!(presence.cache_creation_tokens);
        assert!(presence.output_tokens);
    }

    #[test]
    fn openai_top_level_read_lane_without_write_lane_keeps_creation_unknown() {
        let (_, presence) = super::parse_usage(
            super::UsageDialect::OpenAi,
            &obj(json!({
                "prompt_tokens": 100,
                "cache_read_input_tokens": 40,
                "completion_tokens": 5
            })),
        )
        .unwrap();
        // Top-level proxy lanes may report a fresh-exclusive `prompt_tokens`;
        // without the write lane the creation bucket stays unknown.
        assert!(!presence.cache_creation_tokens);
    }

    #[test]
    fn openai_raw_combined_overflow_stays_invalid_after_assembled_repair() {
        let mut raw = Map::new();
        let (_, presence) = update_openai_usage(
            &mut raw,
            &obj(json!({"prompt_tokens":i64::MAX,"completion_tokens":1})),
            Default::default(),
        )
        .unwrap();
        assert!(presence.input_invalid && presence.output_invalid);
        let (usage, presence) = update_openai_usage(
            &mut raw,
            &obj(json!({"prompt_tokens":10,"completion_tokens":7})),
            presence,
        )
        .unwrap();
        assert!(presence.input_invalid && presence.output_invalid);
        assert!(usage.to_qualified_json_map(presence).is_empty());
    }

    #[test]
    fn raw_combined_overflow_cannot_revive_with_later_valid_update() {
        for (raw, output_survives) in [
            (
                json!({"inputTokens":i64::MAX,"cacheReadInputTokens":1,"outputTokens":7}),
                true,
            ),
            (json!({"inputTokens":i64::MAX,"outputTokens":1}), false),
        ] {
            let (mut usage, mut presence) =
                parse_usage(UsageDialect::BedrockConverse, &obj(raw)).unwrap();
            assert!(presence.input_invalid);
            assert_eq!(presence.output_invalid, !output_survives);
            let (update, observed) = parse_usage(UsageDialect::BedrockConverse, &obj(json!({"inputTokens":10,"cacheReadInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":7}))).unwrap();
            usage.update_disjoint_lanes(&mut presence, update, observed);
            let map = usage.to_qualified_json_map(presence);
            assert!(!map.contains_key("input_tokens"));
            assert_eq!(
                map.get("output_tokens").and_then(Value::as_u64),
                output_survives.then_some(7)
            );
        }
    }

    #[test]
    fn cumulative_disjoint_updates_recompute_inclusive_input_from_retained_lanes() {
        let (mut usage, mut presence) = parse_usage(
            UsageDialect::AnthropicMessages,
            &obj(json!({
                "input_tokens": 100,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
                "output_tokens": 1
            })),
        )
        .unwrap();
        assert_eq!(presence.measured_input_tokens, Some(100));

        let (update, observed) = parse_usage(
            UsageDialect::AnthropicMessages,
            &obj(json!({"input_tokens": 200, "output_tokens": 2})),
        )
        .unwrap();
        usage.update_disjoint_lanes(&mut presence, update, observed);
        assert_eq!(presence.measured_input_tokens, Some(200));

        let (mut split_usage, mut split_presence) = parse_usage(
            UsageDialect::BedrockConverse,
            &obj(json!({"inputTokens": 50})),
        )
        .unwrap();
        for (raw, expected) in [
            (json!({"cacheReadInputTokens": 40}), None),
            (json!({"cacheWriteInputTokens": 10}), Some(100)),
            (json!({"inputTokens": 80}), Some(130)),
        ] {
            let (update, observed) = parse_usage(UsageDialect::BedrockConverse, &obj(raw)).unwrap();
            split_usage.update_disjoint_lanes(&mut split_presence, update, observed);
            assert_eq!(split_presence.measured_input_tokens, expected);
        }
    }

    #[test]
    fn qualified_range_snapshot_preserves_evidence_for_later_repair() {
        let raw = obj(json!({"inputTokens":i64::MAX,"cacheWriteInputTokens":0}));
        let (mut retained, mut presence) =
            parse_usage(UsageDialect::BedrockConverse, &raw).unwrap();
        let (update, observed) = parse_usage(
            UsageDialect::BedrockConverse,
            &obj(json!({"cacheReadInputTokens":1,"outputTokens":7})),
        )
        .unwrap();
        retained.update_disjoint_lanes(&mut presence, update, observed);
        let (projected, qualified) = retained.qualified_snapshot(presence);
        assert_eq!(
            projected.to_qualified_json_map(qualified),
            obj(json!({"output_tokens":7}))
        );
        assert!(!qualified.fresh_input_tokens);
        assert_eq!(retained.input_tokens, i64::MAX as u64);
        let (update, observed) = parse_usage(
            UsageDialect::BedrockConverse,
            &obj(json!({"inputTokens":10})),
        )
        .unwrap();
        retained.update_disjoint_lanes(&mut presence, update, observed);
        let (projected, qualified) = retained.qualified_snapshot(presence);
        assert!(qualified.fresh_input_tokens && qualified.cache_read_tokens);
        assert_eq!(projected.cached_input_tokens, 1);
        assert_eq!(
            projected.to_qualified_json_map(qualified)["total_tokens"],
            18
        );
        let combined = TokenUsage {
            input_tokens: i64::MAX as u64,
            cached_input_tokens: 0,
            cache_creation_tokens: 0,
            output_tokens: 1,
        };
        let (_, qualified) = combined.qualified_snapshot(presence);
        assert!(
            !qualified.any(),
            "unrepresentable aggregate is not exact zero"
        );
    }

    #[test]
    fn raw_counts_outside_persistable_domain_invalidate_only_their_side() {
        for key in [
            "inputTokens",
            "cacheReadInputTokens",
            "cacheWriteInputTokens",
            "outputTokens",
        ] {
            for count in [i64::MAX as u64, i64::MAX as u64 + 1] {
                let mut raw = Map::new();
                raw.insert(key.into(), count.into());
                let (usage, presence) = parse_usage(UsageDialect::BedrockConverse, &raw).unwrap();
                let overflow = count > i64::MAX as u64;
                assert_eq!(presence.input_invalid, overflow && key != "outputTokens");
                assert_eq!(presence.output_invalid, overflow && key == "outputTokens");
                assert_eq!(usage.to_qualified_json_map(presence).is_empty(), overflow);
            }
        }
        let raw = obj(
            json!({"prompt_tokens":i64::MAX as u64 + 1,"prompt_tokens_details":{"cached_tokens":100},"completion_tokens":7}),
        );
        let (usage, presence) = parse_usage(UsageDialect::OpenAi, &raw).unwrap();
        assert!(presence.input_invalid);
        assert_eq!(
            usage.to_qualified_json_map(presence),
            obj(json!({"output_tokens":7}))
        );
    }

    #[test]
    fn null_openai_markers_preserve_disjoint_fallback() {
        for key in [
            "prompt_tokens_details",
            "prompt_cache_hit_tokens",
            "prompt_cache_miss_tokens",
        ] {
            let mut raw = obj(json!({"input_tokens":10,"output_tokens":7}));
            raw.insert(key.into(), Value::Null);
            let (usage, presence) = parse_usage(UsageDialect::OpenAi, &raw).unwrap();
            assert_eq!(
                usage.to_qualified_json_map(presence),
                obj(json!({"input_tokens":10,"output_tokens":7}))
            );
            let (updated, observed) =
                update_openai_usage(&mut Map::new(), &raw, Default::default()).unwrap();
            assert_eq!((updated, observed), (usage, presence));
        }
    }

    #[test]
    fn alias_conflicts_are_quarantined_in_both_update_orders() {
        for (left, right) in [
            (
                json!({"prompt_cache_hit_tokens":80}),
                json!({"prompt_tokens_details":{"cached_tokens":30}}),
            ),
            (
                json!({"prompt_cache_hit_tokens":80}),
                json!({"cache_read_input_tokens":30}),
            ),
            (
                json!({"prompt_tokens_details":{"cached_tokens":80}}),
                json!({"cache_read_input_tokens":30}),
            ),
            (
                json!({"prompt_tokens_details":{"cache_creation_input_tokens":80}}),
                json!({"cache_creation_input_tokens":30}),
            ),
        ] {
            for reverse in [false, true] {
                let mut raw = obj(json!({"prompt_tokens":100,"completion_tokens":7}));
                let mut previous = TokenUsagePresence::default();
                for frame in if reverse {
                    [&right, &left]
                } else {
                    [&left, &right]
                } {
                    let (_, presence) =
                        update_openai_usage(&mut raw, frame.as_object().unwrap(), previous)
                            .unwrap();
                    previous = presence;
                }
                assert!(!previous.input_invalid);
                assert!(
                    !previous.fresh_input_tokens
                        && !previous.cache_read_tokens
                        && !previous.cache_creation_tokens
                );
                assert!(previous.output_tokens);
            }
            let mut same_frame = obj(json!({"prompt_tokens":100,"completion_tokens":7}));
            merge_reported_usage_fields(&mut same_frame, left.as_object().unwrap());
            merge_reported_usage_fields(&mut same_frame, right.as_object().unwrap());
            let mut raw = Map::new();
            let (_, previous) =
                update_openai_usage(&mut raw, &same_frame, Default::default()).unwrap();
            assert!(previous.input_invalid);
            let corrected = obj(json!({
                "prompt_tokens":1000,
                "prompt_cache_hit_tokens":80,
                "prompt_tokens_details":{"cached_tokens":80,"cache_creation_input_tokens":80},
                "cache_read_input_tokens":80,
                "cache_creation_input_tokens":80,
                "completion_tokens":7
            }));
            let (usage, next) = update_openai_usage(&mut raw, &corrected, previous).unwrap();
            assert!(next.input_invalid);
            assert_eq!(
                usage.to_qualified_json_map(next),
                obj(json!({"output_tokens":7}))
            );
            let (_, fresh) = parse_usage(UsageDialect::OpenAi, &corrected).unwrap();
            assert!(fresh.fresh_input_tokens && !fresh.input_invalid);
        }
    }

    #[test]
    fn only_same_frame_alias_conflicts_are_irreversible() {
        let conflicting = obj(
            json!({"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":80,"cache_write_tokens":0},"cache_read_input_tokens":30}),
        );
        let (usage, presence) = parse_usage(UsageDialect::OpenAi, &conflicting).unwrap();
        assert!(presence.input_invalid);
        assert_eq!(
            usage.to_qualified_json_map(presence),
            obj(json!({"output_tokens":7}))
        );
        let mut accumulated = Map::new();
        let first =
            obj(json!({"prompt_tokens":100,"completion_tokens":7,"cache_read_input_tokens":30}));
        let (_, previous) =
            update_openai_usage(&mut accumulated, &first, Default::default()).unwrap();
        let second =
            obj(json!({"prompt_tokens_details":{"cached_tokens":80,"cache_write_tokens":0}}));
        let (usage, presence) = update_openai_usage(&mut accumulated, &second, previous).unwrap();
        assert!(!presence.input_invalid);
        assert_eq!(
            usage.to_qualified_json_map(presence),
            obj(json!({"output_tokens":7}))
        );
        let (usage, presence) = update_openai_usage(
            &mut accumulated,
            &obj(json!({"cache_read_input_tokens":80})),
            presence,
        )
        .unwrap();
        assert!(presence.fresh_input_tokens);
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.cached_input_tokens, 80);
    }

    #[test]
    fn openai_raw_invalid_survives_lossy_merge_and_later_valid_counts() {
        for invalid in [
            json!({"cache_creation_input_tokens":0,"prompt_tokens_details":{"cached_tokens":"bad"}}),
            json!({"cache_read_input_tokens":80,"prompt_cache_hit_tokens":"bad"}),
            json!({"cache_read_input_tokens":80,"prompt_cache_miss_tokens":-1}),
            json!({"prompt_tokens":"bad"}),
            json!({"prompt_tokens_details":{"cached_tokens":-1}}),
        ] {
            let mut raw = Map::new();
            let mut presence = TokenUsagePresence::default();
            let valid = json!({"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":80},"completion_tokens":7});
            for frame in [&valid, &invalid, &valid] {
                let (usage, next) =
                    update_openai_usage(&mut raw, frame.as_object().unwrap(), presence).unwrap();
                presence = next;
                if presence.input_invalid {
                    assert_eq!(
                        usage.to_qualified_json_map(presence),
                        obj(json!({"output_tokens":7}))
                    );
                }
            }
            assert!(presence.input_invalid);
        }
    }

    #[test]
    fn disjoint_invalid_updates_revoke_old_evidence_for_the_attempt() {
        for invalid_output in [false, true] {
            let mut accumulated = TokenUsage::default();
            let mut presence = TokenUsagePresence::default();
            let valid = obj(
                json!({"inputTokens":10,"cacheReadInputTokens":80,"cacheWriteInputTokens":0,"outputTokens":7}),
            );
            let invalid = if invalid_output {
                obj(json!({"outputTokens":"bad"}))
            } else {
                obj(json!({"cacheReadInputTokens":-1}))
            };
            for raw in [&valid, &invalid, &valid] {
                let (update, observed) = parse_usage(UsageDialect::BedrockConverse, raw).unwrap();
                accumulated.update_disjoint_lanes(&mut presence, update, observed);
            }
            let result = accumulated.to_qualified_json_map(presence);
            if invalid_output {
                assert_eq!(result["input_tokens"], 10);
                assert!(!result.contains_key("output_tokens"));
                assert_eq!(accumulated.output_tokens, 0);
            } else {
                assert_eq!(result, obj(json!({"output_tokens":7})));
                assert_eq!(accumulated.input_tokens, 0);
                assert_eq!(accumulated.cached_input_tokens, 0);
            }
            let (_, fresh_attempt) = parse_usage(UsageDialect::BedrockConverse, &valid).unwrap();
            assert!(!fresh_attempt.input_invalid && !fresh_attempt.output_invalid);
        }
    }

    #[test]
    fn cache_only_samples_preserve_counts_and_explicit_zero() {
        for count in [0, 80] {
            for (dialect, raw) in [
                (
                    UsageDialect::OpenAi,
                    json!({"prompt_tokens_details":{"cached_tokens":count}}),
                ),
                (
                    UsageDialect::BedrockConverse,
                    json!({"cacheReadInputTokens":count}),
                ),
            ] {
                let (usage, presence) = parse_usage(dialect, &obj(raw)).unwrap();
                assert_eq!(
                    usage.to_qualified_json_map(presence),
                    obj(json!({"cached_input_tokens":count}))
                );
            }
        }
    }

    #[test]
    fn conflicting_input_partitions_do_not_poison_independent_output() {
        for raw in [
            json!({"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":80},"cache_creation_input_tokens":30}),
            json!({"prompt_tokens":100,"prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":30,"completion_tokens":7}),
            json!({"prompt_tokens":100,"prompt_cache_hit_tokens":101,"completion_tokens":7}),
            json!({"prompt_tokens":100,"prompt_cache_miss_tokens":101,"completion_tokens":7}),
            json!({"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":u64::MAX,"cache_creation_input_tokens":1},"completion_tokens":7}),
        ] {
            let raw = obj(raw);
            let (usage, presence) = parse_usage(UsageDialect::OpenAi, &raw).unwrap();
            assert_eq!(
                usage.to_qualified_json_map(presence),
                obj(json!({"output_tokens":7}))
            );
        }
        // A later cumulative frame can repair a temporary cross-frame mismatch.
        let raw = obj(
            json!({"prompt_tokens":110,"prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":30,"completion_tokens":7}),
        );
        let (usage, presence) = parse_usage(UsageDialect::OpenAi, &raw).unwrap();
        assert!(presence.fresh_input_tokens && presence.cache_read_tokens);
        assert_eq!(usage.input_tokens, 30);
        assert_eq!(usage.cached_input_tokens, 80);
    }

    #[test]
    fn qualified_serialization_preserves_unknown_zero_and_checked_total() {
        let usage = TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 900,
            cache_creation_tokens: 0,
            output_tokens: 20,
        };
        let mut presence = TokenUsagePresence::default();
        assert!(usage.to_qualified_json_map(presence).is_empty());
        presence.cache_creation_tokens = true;
        assert_eq!(
            usage.to_qualified_json_map(presence),
            obj(json!({"cache_creation_tokens": 0}))
        );
        presence.fresh_input_tokens = true;
        presence.cache_read_tokens = true;
        let partial = usage.to_qualified_json_map(presence);
        assert!(!partial.contains_key("output_tokens"));
        assert!(!partial.contains_key("total_tokens"));
        presence.output_tokens = true;
        assert_eq!(usage.to_qualified_json_map(presence)["total_tokens"], 1020);
        let overflow = TokenUsage {
            input_tokens: u64::MAX,
            ..usage
        };
        assert!(
            !overflow
                .to_qualified_json_map(presence)
                .contains_key("total_tokens")
        );
    }

    #[test]
    fn negative_provider_counts_are_unknown_but_reported_zero_is_known() {
        use super::*;
        for (dialect, input, output) in [
            (UsageDialect::OpenAi, "prompt_tokens", "completion_tokens"),
            (
                UsageDialect::AnthropicMessages,
                "input_tokens",
                "output_tokens",
            ),
            (UsageDialect::BedrockConverse, "inputTokens", "outputTokens"),
        ] {
            let mut raw = serde_json::Map::new();
            raw.insert(input.into(), serde_json::json!(-1));
            raw.insert(output.into(), serde_json::json!(0));
            let presence = extract_usage_presence(dialect, &raw);
            assert!(!presence.fresh_input_tokens);
            assert!(presence.output_tokens);
            raw.insert(input.into(), serde_json::json!(0));
            if dialect == UsageDialect::OpenAi {
                raw.insert(
                    "prompt_tokens_details".into(),
                    serde_json::json!({"cached_tokens":0,"cache_write_tokens":0}),
                );
            }
            assert!(extract_usage_presence(dialect, &raw).fresh_input_tokens);
        }
    }

    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().expect("expected object")
    }

    // ── TokenUsage invariants ──────────────────────────────────────────────

    #[test]
    fn total_tokens_sums_all_buckets() {
        let u = TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 40,
            cache_creation_tokens: 10,
            output_tokens: 50,
        };
        assert_eq!(u.total_tokens(), 200);
    }

    #[test]
    fn is_empty_true_for_default() {
        assert!(TokenUsage::default().is_empty());
    }

    #[test]
    fn roundtrip_through_json_map() {
        let original = TokenUsage {
            input_tokens: 123,
            cached_input_tokens: 45,
            cache_creation_tokens: 6,
            output_tokens: 789,
        };
        let m = original.to_json_map();
        assert_eq!(m["input_tokens"], json!(123));
        assert_eq!(m["cached_input_tokens"], json!(45));
        assert_eq!(m["cache_creation_tokens"], json!(6));
        assert_eq!(m["output_tokens"], json!(789));
        assert_eq!(m["total_tokens"], json!(963));
        let back = TokenUsage::from_partial_json_map(&m);
        assert_eq!(back, original);
    }

    #[test]
    fn partial_json_map_missing_buckets_default_to_zero() {
        let m = obj(json!({
            "input_tokens": 7,
            "output_tokens": 3,
        }));
        let usage = TokenUsage::from_partial_json_map(&m);
        assert_eq!(
            usage,
            TokenUsage {
                input_tokens: 7,
                cached_input_tokens: 0,
                cache_creation_tokens: 0,
                output_tokens: 3,
            }
        );
        assert_eq!(usage.total_tokens(), 10);
    }

    // ── Dialect routing ────────────────────────────────────────────────��───

    #[test]
    fn test_dialect_routing() {
        let cases: &[(&str, UsageDialect)] = &[
            ("bedrock", UsageDialect::BedrockConverse),
            ("anthropic", UsageDialect::AnthropicMessages),
            ("openai", UsageDialect::OpenAi),
            ("glm", UsageDialect::OpenAi),
            ("qwen", UsageDialect::OpenAi),
        ];
        for (provider, expected) in cases {
            assert_eq!(
                UsageDialect::for_provider(provider),
                *expected,
                "dialect for {provider}"
            );
        }
    }

    #[test]
    fn provider_usage_matrix_normalizes_disjoint_input_buckets() {
        struct Case {
            name: &'static str,
            dialect: UsageDialect,
            usage: Value,
            expected: Option<NormalizedPromptCacheUsage>,
        }

        let cases = [
            Case {
                name: "openai inclusive cache details",
                dialect: UsageDialect::OpenAi,
                usage: json!({
                    "prompt_tokens": 1100,
                    "completion_tokens": 50,
                    "prompt_tokens_details": {
                        "cached_tokens": 800,
                        "cache_creation_input_tokens": 100
                    }
                }),
                expected: Some(NormalizedPromptCacheUsage {
                    fresh_input_tokens: 200,
                    cache_read_tokens: 800,
                    cache_creation_tokens: 100,
                }),
            },
            Case {
                name: "openai compatible disjoint aliases",
                dialect: UsageDialect::OpenAi,
                usage: json!({
                    "prompt_tokens": 200,
                    "completion_tokens": 50,
                    "cache_read_input_tokens": 800,
                    "cache_creation_input_tokens": 100
                }),
                expected: Some(NormalizedPromptCacheUsage {
                    fresh_input_tokens: 200,
                    cache_read_tokens: 800,
                    cache_creation_tokens: 100,
                }),
            },
            Case {
                name: "deepseek native inclusive hit miss partition",
                dialect: UsageDialect::OpenAi,
                usage: json!({
                    "prompt_tokens": 1000,
                    "completion_tokens": 50,
                    "prompt_cache_hit_tokens": 800,
                    "prompt_cache_miss_tokens": 200
                }),
                expected: Some(NormalizedPromptCacheUsage {
                    fresh_input_tokens: 200,
                    cache_read_tokens: 800,
                    cache_creation_tokens: 0,
                }),
            },
            Case {
                name: "anthropic disjoint cache fields",
                dialect: UsageDialect::AnthropicMessages,
                usage: json!({
                    "input_tokens": 200,
                    "output_tokens": 50,
                    "cache_read_input_tokens": 800,
                    "cache_creation_input_tokens": 100
                }),
                expected: Some(NormalizedPromptCacheUsage {
                    fresh_input_tokens: 200,
                    cache_read_tokens: 800,
                    cache_creation_tokens: 100,
                }),
            },
            Case {
                name: "bedrock disjoint cache fields",
                dialect: UsageDialect::BedrockConverse,
                usage: json!({
                    "inputTokens": 200,
                    "outputTokens": 50,
                    "cacheReadInputTokens": 800,
                    "cacheWriteInputTokens": 100
                }),
                expected: Some(NormalizedPromptCacheUsage {
                    fresh_input_tokens: 200,
                    cache_read_tokens: 800,
                    cache_creation_tokens: 100,
                }),
            },
            Case {
                name: "missing usage",
                dialect: UsageDialect::OpenAi,
                usage: json!({}),
                expected: None,
            },
            Case {
                name: "contradictory inclusive values quarantine input",
                dialect: UsageDialect::OpenAi,
                usage: json!({
                    "prompt_tokens": 100,
                    "completion_tokens": 50,
                    "prompt_tokens_details": {
                        "cached_tokens": 800,
                        "cache_creation_input_tokens": 100
                    }
                }),
                expected: Some(NormalizedPromptCacheUsage {
                    fresh_input_tokens: 0,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                }),
            },
        ];

        for case in cases {
            let normalized = case
                .usage
                .as_object()
                .and_then(|usage| extract_usage(case.dialect, usage))
                .map(TokenUsage::normalized_prompt_cache_usage);
            assert_eq!(normalized, case.expected, "case: {}", case.name);
            if let Some(usage) = normalized {
                assert_eq!(
                    usage.total_input_tokens(),
                    usage
                        .fresh_input_tokens
                        .saturating_add(usage.cache_read_tokens)
                        .saturating_add(usage.cache_creation_tokens),
                    "case: {}",
                    case.name
                );
            }
        }
    }

    // ── OpenAI extractor ───────────────────────────────────────────────────

    #[test]
    fn openai_plain_total_preserves_measurement_without_inventing_fresh() {
        let u = obj(json!({"prompt_tokens":100,"completion_tokens":50}));
        let (usage, presence) = parse_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(presence.measured_input_tokens, Some(100));
        assert!(!presence.fresh_input_tokens);
        assert_eq!(usage.output_tokens, 50);
    }

    #[test]
    fn openai_path_accepts_anthropic_native_usage_aliases() {
        let u = obj(json!({
            "input_tokens": 200,
            "output_tokens": 50,
            "cache_read_input_tokens": 800,
            "cache_creation_input_tokens": 100
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 200);
        assert_eq!(t.cached_input_tokens, 800);
        assert_eq!(t.cache_creation_tokens, 100);
        assert_eq!(t.output_tokens, 50);
        assert_eq!(t.total_tokens(), 1150);
    }

    #[test]
    fn openai_with_cached_tokens_deducts_from_prompt() {
        // OpenAI prompt_tokens INCLUDES cached tokens — the 900 fresh + 100
        // cached split must come out after parsing.
        let u = obj(json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 100, "cache_write_tokens": 0}
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 900);
        assert_eq!(t.cached_input_tokens, 100);
        assert_eq!(t.cache_creation_tokens, 0);
        assert_eq!(t.output_tokens, 50);
        // Billing identity: fresh + cached + creation = original prompt_tokens
        assert_eq!(
            t.input_tokens + t.cached_input_tokens + t.cache_creation_tokens,
            1000
        );
    }

    #[test]
    fn deepseek_native_hit_miss_fields_do_not_double_count_prompt_total() {
        let u = obj(json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_cache_hit_tokens": 800,
            "prompt_cache_miss_tokens": 200
        }));
        let (t, presence) = parse_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 200);
        assert_eq!(t.cached_input_tokens, 800);
        assert_eq!(t.cache_creation_tokens, 0);
        assert_eq!(t.output_tokens, 50);
        assert_eq!(t.input_tokens + t.cached_input_tokens, 1000);
        assert!(presence.cache_creation_tokens);
        assert_eq!(
            t.to_qualified_json_map(presence)["cache_creation_tokens"],
            0
        );
        let zero = obj(
            json!({"prompt_tokens":0,"prompt_cache_hit_tokens":0,"prompt_cache_miss_tokens":0}),
        );
        let (usage, presence) = parse_usage(UsageDialect::OpenAi, &zero).unwrap();
        assert!(presence.cache_creation_tokens);
        assert_eq!(
            usage.to_qualified_json_map(presence)["cache_creation_tokens"],
            0
        );
        for raw in [
            json!({"prompt_tokens":1000,"prompt_cache_hit_tokens":800}),
            json!({"prompt_tokens":1000,"prompt_cache_miss_tokens":200}),
            json!({"prompt_tokens":1000,"prompt_cache_hit_tokens":800,"prompt_cache_miss_tokens":201}),
            json!({"prompt_tokens":1000,"prompt_cache_hit_tokens":800,"prompt_cache_miss_tokens":200,"cache_creation_input_tokens":"invalid"}),
            json!({"prompt_tokens":1000,"prompt_cache_hit_tokens":800,"prompt_cache_miss_tokens":200,"cache_creation_input_tokens":1}),
            json!({"prompt_tokens":1000,"prompt_cache_hit_tokens":800,"prompt_cache_miss_tokens":200,"prompt_tokens_details":{"cached_tokens":799}}),
        ] {
            let (usage, presence) = parse_usage(UsageDialect::OpenAi, &obj(raw)).unwrap();
            assert!(!presence.cache_creation_tokens);
            assert!(
                !usage
                    .to_qualified_json_map(presence)
                    .contains_key("cache_creation_tokens")
            );
        }
        // A complete explicit partition proves the zero write bucket without
        // double-counting the inclusive total.
        for raw in [
            json!({"prompt_cache_hit_tokens":800,"prompt_cache_miss_tokens":200}),
            json!({"prompt_tokens":1000,"prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":0}}),
        ] {
            let (usage, presence) = parse_usage(UsageDialect::OpenAi, &obj(raw)).unwrap();
            assert!(presence.cache_creation_tokens);
            assert_eq!(usage.cache_creation_tokens, 0);
            assert_eq!(usage.cached_input_tokens, 800);
            assert_eq!(usage.input_tokens, 200);
            assert_eq!(
                usage.to_qualified_json_map(presence)["cache_creation_tokens"],
                0
            );
        }
    }

    #[test]
    fn deepseek_missing_miss_keeps_fresh_unknown() {
        let u =
            obj(json!({"prompt_tokens":1000,"completion_tokens":50,"prompt_cache_hit_tokens":800}));
        let (usage, presence) = parse_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(presence.measured_input_tokens, Some(1000));
        assert_eq!(usage.cached_input_tokens, 800);
        assert!(!presence.fresh_input_tokens && !presence.cache_creation_tokens);
    }

    #[test]
    fn openai_with_cache_creation_also_deducts() {
        let u = obj(json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_tokens_details": {
                "cached_tokens": 100,
                "cache_creation_input_tokens": 50
            }
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 850);
        assert_eq!(t.cached_input_tokens, 100);
        assert_eq!(t.cache_creation_tokens, 50);
        assert_eq!(t.total_tokens(), 1050); // input + cached + creation + output
    }

    #[test]
    fn openai_top_level_cache_creation_is_honored() {
        // Some proxies (Anthropic-on-OpenAI-compatible) surface the cache
        // creation at the top level of the usage object.
        let u = obj(json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 100},
            "cache_creation_input_tokens": 50
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 850);
        assert_eq!(t.cached_input_tokens, 100);
        assert_eq!(t.cache_creation_tokens, 50);
    }

    #[test]
    fn openai_empty_usage_returns_none() {
        let u = obj(json!({}));
        assert!(extract_usage(UsageDialect::OpenAi, &u).is_none());
    }

    #[test]
    fn usage_presence_distinguishes_missing_lanes_from_reported_zero() {
        let reported = obj(json!({
            "prompt_tokens": 18,
            "completion_tokens": 0,
            "prompt_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0}
        }));
        let presence = extract_usage_presence(UsageDialect::OpenAi, &reported);
        assert!(presence.fresh_input_tokens);
        assert!(
            presence.output_tokens,
            "an explicitly reported zero is present"
        );
        assert!(presence.cache_read_tokens);
        // Explicit read/write zeros are evidence; absent lanes remain unknown.
        assert!(presence.cache_creation_tokens);

        let partial = obj(json!({"prompt_tokens": 18}));
        let presence = extract_usage_presence(UsageDialect::OpenAi, &partial);
        assert_eq!(presence.measured_input_tokens, Some(18));
        assert!(!presence.fresh_input_tokens);
        assert!(!presence.output_tokens, "an absent lane is unavailable");
    }

    #[test]
    fn openai_inclusive_contract_violation_quarantines_input_preserving_output() {
        let u = obj(json!({
            "prompt_tokens": 50,
            "completion_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 9999}
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 0);
        assert_eq!(t.cached_input_tokens, 0);
        let presence = extract_usage_presence(UsageDialect::OpenAi, &u);
        assert_eq!(
            t.to_qualified_json_map(presence),
            obj(json!({"output_tokens":10}))
        );
    }

    // ── Anthropic-native disjoint shape (proxied as OpenAI-compatible) ─────

    /// Some Anthropic-behind-OpenAI proxies pass the native Anthropic shape
    /// through: `prompt_tokens` is already the FRESH input (disjoint from
    /// `cache_read_input_tokens` + `cache_creation_input_tokens`). The
    /// extractor must NOT subtract cache counts in that case — doing so
    /// would under-report fresh input and violate the billing identity.
    /// Boundary case: `cached + creation == prompt_total` exactly.
    /// With field-presence disambiguation, nested `prompt_tokens_details`
    /// forces inclusive semantics even at the tie.
    #[test]
    fn openai_inclusive_at_boundary_prefers_nested_signal() {
        let u = obj(json!({
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 100}
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 0, "inclusive: fresh = 100 - 100 = 0");
        assert_eq!(t.cached_input_tokens, 100);
    }

    /// Boundary case: the SAME arithmetic tie but top-level
    /// `cache_read_input_tokens` (no nested details) signals disjoint.
    /// prompt_tokens is already the fresh count; cache counts are separate.
    #[test]
    fn openai_disjoint_at_boundary_prefers_top_level_signal() {
        let u = obj(json!({
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "cache_read_input_tokens": 100
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 100, "disjoint: prompt IS fresh");
        assert_eq!(t.cached_input_tokens, 100);
        assert_eq!(t.total_tokens(), 210);
    }

    /// When both shapes' markers coexist (cached in nested AND top-level),
    /// prefer the nested-inclusive interpretation — that's how OpenAI-native
    /// clients that also echo Anthropic-style aliases behave.
    #[test]
    fn openai_both_signals_present_prefers_inclusive() {
        let u = obj(json!({
            "prompt_tokens": 1000,
            "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 300, "cache_write_tokens": 0},
            "cache_read_input_tokens": 300
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 700);
        assert_eq!(t.cached_input_tokens, 300);
    }

    #[test]
    fn openai_disjoint_shape_keeps_prompt_as_fresh() {
        let u = obj(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "cache_read_input_tokens": 200,
            "cache_creation_input_tokens": 50
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        // prompt_tokens(100) < cached(200)+creation(50)=250 → disjoint shape.
        // Fresh input = prompt_tokens verbatim.
        assert_eq!(t.input_tokens, 100);
        assert_eq!(t.cached_input_tokens, 200);
        assert_eq!(t.cache_creation_tokens, 50);
        assert_eq!(t.output_tokens, 20);
        assert_eq!(t.total_tokens(), 370);
    }

    #[test]
    fn openai_disjoint_proxy_measures_only_complete_physical_input() {
        let complete = obj(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "cache_read_input_tokens": 200,
            "cache_creation_input_tokens": 50
        }));
        let (usage, presence) = parse_usage(UsageDialect::OpenAi, &complete).unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(presence.measured_input_tokens, Some(350));

        let mut incomplete = complete;
        incomplete.remove("cache_creation_input_tokens");
        let (_, presence) = parse_usage(UsageDialect::OpenAi, &incomplete).unwrap();
        assert_eq!(presence.measured_input_tokens, None);

        let inclusive = obj(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 80},
            "cache_read_input_tokens": 80
        }));
        let (_, presence) = parse_usage(UsageDialect::OpenAi, &inclusive).unwrap();
        assert_eq!(presence.measured_input_tokens, Some(100));
    }

    #[test]
    fn openai_stream_reassembles_disjoint_proxy_measurement_in_either_order() {
        let prompt = obj(json!({"prompt_tokens": 100, "completion_tokens": 20}));
        let read = obj(json!({"cache_read_input_tokens": 200}));
        let write = obj(json!({"cache_creation_input_tokens": 50}));
        for frames in [[&prompt, &read, &write], [&read, &write, &prompt]] {
            let mut accumulated = Map::new();
            let mut previous = TokenUsagePresence::default();
            for (index, frame) in frames.into_iter().enumerate() {
                let (usage, presence) =
                    update_openai_usage(&mut accumulated, frame, previous).unwrap();
                if index == 1 {
                    assert_eq!(presence.measured_input_tokens, None);
                }
                if index == 2 {
                    assert_eq!(presence.measured_input_tokens, Some(350));
                    assert_eq!(usage.input_tokens, 100);
                    assert_eq!(usage.cached_input_tokens, 200);
                    assert_eq!(usage.cache_creation_tokens, 50);
                }
                previous = presence;
            }
        }
    }

    #[test]
    fn openai_inclusive_shape_still_subtracts() {
        // Sanity: inclusive shape (prompt ⊇ cached + creation) still works.
        let u = obj(json!({
            "prompt_tokens": 500,
            "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 200},
            "cache_creation_input_tokens": 50
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 250);
        assert_eq!(t.cached_input_tokens, 200);
        assert_eq!(t.cache_creation_tokens, 50);
        assert_eq!(t.output_tokens, 20);
    }

    // ── Bedrock extractor ──────────────────────────────────────────────────

    #[test]
    fn bedrock_plain_without_cache() {
        let u = obj(json!({"inputTokens": 100, "outputTokens": 50}));
        let t = extract_usage(UsageDialect::BedrockConverse, &u).unwrap();
        assert_eq!(t.input_tokens, 100);
        assert_eq!(t.cached_input_tokens, 0);
        assert_eq!(t.cache_creation_tokens, 0);
        assert_eq!(t.output_tokens, 50);
        assert_eq!(t.total_tokens(), 150);
    }

    #[test]
    fn bedrock_input_tokens_excludes_cache_read() {
        // Bedrock contract: inputTokens is DISJOINT from cacheReadInputTokens.
        // A correct total is input + cacheRead + cacheWrite + output.
        let u = obj(json!({
            "inputTokens": 200,
            "outputTokens": 50,
            "cacheReadInputTokens": 800,
            "cacheWriteInputTokens": 100
        }));
        let t = extract_usage(UsageDialect::BedrockConverse, &u).unwrap();
        assert_eq!(t.input_tokens, 200);
        assert_eq!(t.cached_input_tokens, 800);
        assert_eq!(t.cache_creation_tokens, 100);
        assert_eq!(t.output_tokens, 50);
        assert_eq!(t.total_tokens(), 1150);
    }

    #[test]
    fn bedrock_cache_only_without_totals() {
        // Model returned cache but no totalTokens — our total must still be
        // correct from disjoint parts.
        let u = obj(json!({
            "inputTokens": 0,
            "outputTokens": 200,
            "cacheReadInputTokens": 5000
        }));
        let t = extract_usage(UsageDialect::BedrockConverse, &u).unwrap();
        assert_eq!(t.total_tokens(), 5200);
    }

    #[test]
    fn bedrock_empty_usage_returns_none() {
        let u = obj(json!({}));
        assert!(extract_usage(UsageDialect::BedrockConverse, &u).is_none());
    }

    // ── Anthropic Messages extractor ───────────────────────────────────────

    #[test]
    fn anthropic_messages_usage_is_disjoint() {
        let u = obj(json!({
            "input_tokens": 200,
            "output_tokens": 50,
            "cache_read_input_tokens": 800,
            "cache_creation_input_tokens": 100
        }));
        let t = extract_usage(UsageDialect::AnthropicMessages, &u).unwrap();
        assert_eq!(t.input_tokens, 200);
        assert_eq!(t.cached_input_tokens, 800);
        assert_eq!(t.cache_creation_tokens, 100);
        assert_eq!(t.output_tokens, 50);
        assert_eq!(t.total_tokens(), 1150);
    }

    #[test]
    fn test_anthropic_messages_extraction() {
        // Empty usage returns None.
        assert!(extract_usage(UsageDialect::AnthropicMessages, &obj(json!({}))).is_none());
        // Explicit all-zero lanes remain evidence instead of collapsing into
        // an unobserved usage object.
        assert!(
            extract_usage(
                UsageDialect::AnthropicMessages,
                &obj(json!({
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0
                }))
            )
            .is_some()
        );
        // Cache-only should still be Some.
        let t = extract_usage(
            UsageDialect::AnthropicMessages,
            &obj(json!({
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_read_input_tokens": 500
            })),
        )
        .unwrap();
        assert_eq!(t.cached_input_tokens, 500);
    }

    // ── Canonical JSON shape used in SSE events ────────────────────────────

    #[test]
    fn json_map_uses_canonical_keys_only() {
        let t = TokenUsage {
            input_tokens: 1,
            cached_input_tokens: 2,
            cache_creation_tokens: 3,
            output_tokens: 4,
        };
        let m = t.to_json_map();
        let keys: Vec<&String> = m.keys().collect();
        // Must not leak legacy names like prompt/completion/cache_read.
        assert!(keys.iter().all(|k| matches!(
            k.as_str(),
            "input_tokens"
                | "cached_input_tokens"
                | "cache_creation_tokens"
                | "output_tokens"
                | "total_tokens"
        )));
    }
}
