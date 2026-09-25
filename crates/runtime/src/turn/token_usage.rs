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
//!   subtract `prompt_tokens_details.cached_tokens` so `input_tokens` reflects
//!   only fresh input. Cache creation is rarely surfaced; when present as
//!   `prompt_tokens_details.cache_creation_input_tokens` we subtract too.
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

/// Records which normalized lanes the provider actually supplied. A zero
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
    same_frame_aliases: bool,
) -> Option<(TokenUsage, TokenUsagePresence)> {
    if dialect == UsageDialect::OpenAi
        && !usage_obj.contains_key("prompt_tokens")
        && !usage_obj.contains_key("completion_tokens")
        && ![
            "prompt_tokens_details",
            "prompt_cache_hit_tokens",
            "prompt_cache_miss_tokens",
        ]
        .iter()
        .any(|key| usage_obj.get(*key).is_some_and(|value| !value.is_null()))
        && [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ]
        .iter()
        .any(|key| usage_obj.contains_key(*key))
    {
        return parse_disjoint_usage(UsageDialect::AnthropicMessages, usage_obj);
    }
    match dialect {
        UsageDialect::OpenAi => parse_openai_usage(usage_obj, same_frame_aliases),
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

fn parse_openai_usage(
    u: &Map<String, Value>,
    same_frame_aliases: bool,
) -> Option<(TokenUsage, TokenUsagePresence)> {
    let mut input_invalid = false;
    let mut output_invalid = false;
    let prompt = decode_count(u.get("prompt_tokens"), &mut input_invalid);
    let hit = decode_count(u.get("prompt_cache_hit_tokens"), &mut input_invalid);
    let miss = decode_count(u.get("prompt_cache_miss_tokens"), &mut input_invalid);
    let top_read = decode_count(u.get("cache_read_input_tokens"), &mut input_invalid);
    let top_write = decode_count(u.get("cache_creation_input_tokens"), &mut input_invalid);
    let output = decode_count(u.get("completion_tokens"), &mut output_invalid);
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
    let native_partition = hit.is_some() || miss.is_some();
    let mut alias_conflict = false;
    {
        // These fields name the same disjoint buckets. Different streaming
        // frames may legitimately carry different cumulative snapshots.
        for (left, right) in [
            (hit, nested_read),
            (hit, top_read),
            (nested_read, top_read),
            (nested_write, top_write),
        ] {
            alias_conflict |= matches!((left, right), (Some(left), Some(right)) if left != right);
        }
    }
    input_invalid |= same_frame_aliases && alias_conflict;
    let inclusive = native_partition || nested_read.is_some() || nested_write.is_some();
    let cached = hit.or(nested_read).or(top_read);
    let creation = nested_write.or(top_write);
    let cache_total = cached.unwrap_or(0).checked_add(creation.unwrap_or(0));
    let fresh = if native_partition {
        miss.or_else(|| prompt.and_then(|p| cache_total.and_then(|cache| p.checked_sub(cache))))
    } else if inclusive {
        prompt.and_then(|p| cache_total.and_then(|cache| p.checked_sub(cache)))
    } else {
        prompt
    };
    let partition_conflict = prompt.is_some_and(|p| {
        (inclusive && cache_total.is_none_or(|cache| cache > p))
            || miss.is_some_and(|miss| miss > p)
            || (native_partition
                && hit.is_some()
                && miss.is_some()
                && miss.and_then(|miss| cache_total.and_then(|cache| miss.checked_add(cache)))
                    != Some(p))
    });
    let mut usage = TokenUsage {
        input_tokens: fresh.unwrap_or(0),
        cached_input_tokens: cached.unwrap_or(0),
        cache_creation_tokens: creation.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
    };
    // Native OpenAI/DeepSeek reports an inclusive prompt total. A proxy that
    // supplies only top-level cache lanes reports fresh input separately;
    // its physical total is known only after both cache lanes are present.
    let measured_input_tokens = if inclusive {
        prompt
    } else if top_read.is_some() || top_write.is_some() {
        fresh
            .and_then(|fresh| fresh.checked_add(top_read?))
            .and_then(|input| input.checked_add(top_write?))
            .filter(|total| i64::try_from(*total).is_ok())
    } else {
        prompt
    }
    .filter(|_| !input_invalid && !partition_conflict && !alias_conflict);
    let mut presence = TokenUsagePresence {
        fresh_input_tokens: fresh.is_some(),
        cache_read_tokens: cached.is_some(),
        cache_creation_tokens: creation.is_some(),
        output_tokens: output.is_some(),
        measured_input_tokens,
        input_invalid,
        output_invalid,
    };
    if !presence.any() && !input_invalid && !output_invalid {
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
    fn inclusive_prompt_measurement_is_independent_of_cache_billing_completeness() {
        let partial = serde_json::json!({
            "prompt_tokens": 100_000,
            "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 90_000}
        });
        let (_, presence) =
            super::parse_usage(super::UsageDialect::OpenAi, partial.as_object().unwrap()).unwrap();
        assert_eq!(presence.measured_input_tokens, Some(100_000));
        assert!(!presence.cache_creation_tokens);

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
            json!({"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":80},"cache_read_input_tokens":30}),
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
        let second = obj(json!({"prompt_tokens_details":{"cached_tokens":80}}));
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
    fn openai_plain_without_cache() {
        let u = obj(json!({"prompt_tokens": 100, "completion_tokens": 50}));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 100);
        assert_eq!(t.cached_input_tokens, 0);
        assert_eq!(t.cache_creation_tokens, 0);
        assert_eq!(t.output_tokens, 50);
        assert_eq!(t.total_tokens(), 150);
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
            "prompt_tokens_details": {"cached_tokens": 100}
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
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 200);
        assert_eq!(t.cached_input_tokens, 800);
        assert_eq!(t.cache_creation_tokens, 0);
        assert_eq!(t.output_tokens, 50);
        assert_eq!(t.input_tokens + t.cached_input_tokens, 1000);
    }

    #[test]
    fn deepseek_native_missing_miss_derives_fresh_from_prompt_total() {
        let u = obj(json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_cache_hit_tokens": 800
        }));
        let t = extract_usage(UsageDialect::OpenAi, &u).unwrap();
        assert_eq!(t.input_tokens, 200);
        assert_eq!(t.cached_input_tokens, 800);
        assert_eq!(t.total_tokens(), 1050);
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
            "prompt_tokens_details": {"cached_tokens": 0}
        }));
        let presence = extract_usage_presence(UsageDialect::OpenAi, &reported);
        assert!(presence.fresh_input_tokens);
        assert!(
            presence.output_tokens,
            "an explicitly reported zero is present"
        );
        assert!(presence.cache_read_tokens);
        assert!(!presence.cache_creation_tokens);

        let partial = obj(json!({"prompt_tokens": 18}));
        let presence = extract_usage_presence(UsageDialect::OpenAi, &partial);
        assert!(presence.fresh_input_tokens);
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
            "prompt_tokens_details": {"cached_tokens": 300},
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
