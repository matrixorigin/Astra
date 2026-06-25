//! Cache-performance diagnostic rules surfaced through
//! `introspect(facet="cache")`.
//!
//! All four rules are **pure functions**: they consume a slice of
//! [`RoundSnapshot`] entries (a recent-history ring captured by the
//! runtime, optionally enriched from `llm_capture_*.json` files) and
//! return a list of [`CacheFinding`]s. The runtime layer is responsible
//! for (a) populating `RoundSnapshot` entries, (b) formatting findings
//! into the introspect output. This module cares only about the rules.
//!
//! Rule inventory (see [`evaluate_all`]):
//! 1. `cc_marker_frozen` — marker positions unchanged across 3+
//!    consecutive rounds of the same turn → agentic tool-loop rolling
//!    regression.
//! 2. `tool_marker_not_on_tail` — always-load tool cache_control lands
//!    strictly before the last tool → later tools fall outside the
//!    cached prefix.
//! 3. `cache_read_collapsed` — `cache_read` drops >50% between
//!    consecutive rounds of the same turn/provider → a previously
//!    stable prefix was broken.
//! 4. `cache_creation_waste` — within a single turn, cumulative
//!    `cache_creation / cache_read > 0.3` → repeated prefix rebuilds.
//!
//! Each rule emits at most one finding; the `evaluate_all` driver
//! concatenates them in a deterministic order (rule id ascending) so
//! the output is stable across runs.

use serde::{Deserialize, Serialize};

/// One cache snapshot per LLM round. Populated by the runtime from the
/// per-turn cache ring and/or `llm_capture_*.json` files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoundSnapshot {
    pub turn: u32,
    pub round: u32,
    pub provider: String,
    pub model: String,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// Count of tool schemas sent in this request.
    pub tool_count: u32,
    /// Index of the last tool whose schema carries a `cache_control`
    /// marker (`None` if no tool carries it — OpenAI-compat / cache
    /// disabled). 0-indexed within `request.tools`.
    pub tool_cc_index: Option<u32>,
    /// Indices (within `request.messages`) that carry a `cache_control`
    /// marker — the message-level tail breakpoint positions.
    /// Empty for non-Anthropic providers.
    pub message_cc_indices: Vec<u32>,
    /// Indices of messages whose content contains known volatile
    /// patterns (`## Self-Awareness`, live turn/token counters, session
    /// anchors). Populated by the capture parser. Used by
    /// `rule_volatile_in_cached_prefix` to detect volatile bytes in
    /// positions the provider's cache can't tolerate.
    #[serde(default)]
    pub volatile_msg_indices: Vec<u32>,
    /// Total number of messages — needed when reasoning about "is the
    /// volatile message at the tail?" independently of which indices
    /// happen to be populated.
    #[serde(default)]
    pub message_count: u32,
    /// Roles of each message in order (`system`, `user`, `assistant`,
    /// `tool`). Populated by the capture parser. The volatile rule
    /// uses this to distinguish "volatile in a system message block"
    /// (where the runtime owns block-level cache_control layout and
    /// we should trust it) from "volatile in a user/tool mid-history
    /// message" (the real regression the rule was built to catch).
    #[serde(default)]
    pub message_roles: Vec<String>,
}

/// Severity of a diagnostic finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    /// Informational — nothing broken, but worth surfacing (e.g. OpenAI
    /// path so the reader doesn't expect cache metrics).
    Info,
    /// Sub-optimal — real cache tokens being wasted but not a crisis.
    Warn,
    /// Known regression — cache effectively broken for this traffic.
    Critical,
}

/// One actionable finding emitted by a diagnostic rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheFinding {
    /// Stable identifier, e.g. `"cc_marker_frozen"`. Used for
    /// test-assertions and future machine-parseable consumers.
    pub rule_id: &'static str,
    pub severity: Severity,
    /// One-line human summary.
    pub narrative: String,
    /// Concrete thing the operator should change / look at.
    pub actionable_fix: String,
    /// Round(s) that triggered the rule (turn, round pairs).
    pub triggered_on: Vec<(u32, u32)>,
}

/// Build a [`RoundSnapshot`] from a single captured LLM request/response
/// JSON (the shape that the runtime writes to
/// `~/.astra/sessions/<sid>/llm_capture_t{N}_r{M}_*.json` when
/// `full_llm_capture` is enabled, and the fixture under
/// `tests/fixtures/cache_diagnosis_d0640d3d/`).
///
/// Pure parser — no I/O. Missing fields degrade to defaults (empty
/// strings, 0 tokens, no cc indices) rather than panicking so a
/// malformed or partially-written capture never crashes diagnosis.
#[must_use]
pub fn snapshot_from_capture_json(v: &serde_json::Value) -> RoundSnapshot {
    use serde_json::Value;
    let turn = v.get("turn").and_then(Value::as_u64).unwrap_or(0) as u32;
    let round = v.get("round").and_then(Value::as_u64).unwrap_or(0) as u32;
    let provider = v
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let model = v
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let usage = v.get("response").and_then(|r| r.get("usage"));
    let cache_read_tokens = usage
        .and_then(|u| u.get("cached_input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_creation_tokens = usage
        .and_then(|u| u.get("cache_creation_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let req = v.get("request");
    let tool_count = req
        .and_then(|r| r.get("tool_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let tool_cc_index = req
        .and_then(|r| r.get("tools"))
        .and_then(Value::as_array)
        .and_then(|arr| {
            // Find the LAST tool carrying a `cache_control` marker —
            // Anthropic's cache_control on tools marks the prefix boundary.
            arr.iter().enumerate().rev().find_map(|(idx, tool)| {
                let at_top = tool.get("cache_control").is_some();
                let in_fn = tool
                    .get("function")
                    .and_then(|f| f.get("cache_control"))
                    .is_some();
                if at_top || in_fn {
                    Some(idx as u32)
                } else {
                    None
                }
            })
        });

    let msgs_arr = req
        .and_then(|r| r.get("messages"))
        .and_then(Value::as_array);
    let message_count = msgs_arr.map(|a| a.len() as u32).unwrap_or(0);
    let message_roles: Vec<String> = msgs_arr
        .map(|arr| {
            arr.iter()
                .map(|m| {
                    m.get("role")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default();
    let message_cc_indices = msgs_arr
        .map(|arr| {
            arr.iter()
                .enumerate()
                .filter_map(|(i, m)| {
                    let at_msg = m.get("cache_control").is_some();
                    let in_content =
                        m.get("content")
                            .and_then(Value::as_array)
                            .is_some_and(|blocks| {
                                blocks.iter().any(|b| b.get("cache_control").is_some())
                            });
                    (at_msg || in_content).then_some(i as u32)
                })
                .collect::<Vec<u32>>()
        })
        .unwrap_or_default();

    // Volatile-content detection: scan every message's flattened text
    // for known volatile patterns. The list is deliberately conservative
    // — each pattern here has been observed in production astra traffic
    // to carry per-round values that defeat prefix caching:
    //   - `## Self-Awareness`  — the block rendered by SelfModel, carries
    //     live `Turn: N` and `Tokens: M/K` counters (session 986a553e).
    //   - `[session-memory:`   — the session-memory manifest, re-rendered
    //     per turn with updated state.
    //
    // Detection is substring-based to keep the parser dependency-free;
    // the rule code consuming these indices treats presence as a signal,
    // never as a definitive identification of "what" the volatile is.
    let message_volatile_indices = msgs_arr
        .map(|arr| {
            arr.iter()
                .enumerate()
                .filter_map(|(i, m)| {
                    let text = flatten_message_text_for_scan(m);
                    if contains_volatile_pattern(&text) {
                        Some(i as u32)
                    } else {
                        None
                    }
                })
                .collect::<Vec<u32>>()
        })
        .unwrap_or_default();

    RoundSnapshot {
        turn,
        round,
        provider,
        model,
        cache_read_tokens,
        cache_creation_tokens,
        tool_count,
        tool_cc_index,
        message_cc_indices,
        volatile_msg_indices: message_volatile_indices,
        message_count,
        message_roles,
    }
}

/// Flatten a message's content into a single string suitable for
/// substring scanning. Handles both `content: "str"` and
/// `content: [{type:"text", text:"…"}, …]` shapes. Non-text blocks are
/// dropped (tool_use, tool_result payload JSON doesn't carry the
/// volatile patterns we look for).
fn flatten_message_text_for_scan(m: &serde_json::Value) -> String {
    use serde_json::Value;
    match m.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    out.push_str(t);
                    out.push('\n');
                }
            }
            out
        }
        _ => String::new(),
    }
}

/// Safely look up a message's role from the parsed snapshot. Returns
/// `""` when the index is out of range or roles weren't populated
/// (older snapshots serialized before `message_roles` was added).
fn role_at(snap: &RoundSnapshot, idx: u32) -> &str {
    snap.message_roles
        .get(idx as usize)
        .map(String::as_str)
        .unwrap_or("")
}

fn contains_volatile_pattern(text: &str) -> bool {
    // Each pattern requires a CO-OCCURRENCE with a structural sibling
    // so incidental mentions in tool output (e.g. a git show of a
    // commit whose body quotes `## Self-Awareness`) don't register as
    // astra-injected volatile content. Observed false positive in
    // session bc5764b6: a `tool` message carrying the commit body for
    // d2d6f96a matched `## Self-Awareness` alone.
    //
    // Real volatile produced by the SelfModel renderer always emits
    // the section header immediately followed by `Turn: N | Tokens: M`.
    // The session-memory manifest carries a similarly distinctive header
    // + `goal:` line. Gate on both to cut false positives.
    if text.contains("## Self-Awareness") && text.contains("Turn: ") && text.contains("Tokens: ") {
        return true;
    }
    if text.contains("[session-memory:") && text.contains("goal:") {
        return true;
    }
    false
}

/// Scan a session directory for `llm_capture_t{N}_r{M}_*.json` files
/// and return the parsed [`RoundSnapshot`]s sorted by (turn, round).
///
/// Designed for the `full_llm_capture=true` path: the runtime writes
/// one JSON per LLM round, and this reads them back in traversal
/// order. Returns `Ok(vec![])` when the directory exists but contains
/// no captures (e.g. `full_llm_capture` was never enabled on this
/// session).
///
/// Missing / unreadable / malformed files are skipped with a log
/// warning rather than failing — one bad capture shouldn't hide the
/// rest of the session from diagnosis.
pub fn load_session_captures(session_dir: &std::path::Path) -> std::io::Result<Vec<RoundSnapshot>> {
    let entries = match std::fs::read_dir(session_dir) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut rows: Vec<(u32, u32, std::path::PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with("llm_capture_t") || !name.ends_with(".json") {
            continue;
        }
        // Accept both the production name (`llm_capture_t3_r0_bridge_inprocess_success_…`)
        // and the scrubbed fixture name (`t3_r0.json`). The `t{N}_r{M}`
        // tokens always appear and fully identify the position.
        let rest = name.trim_start_matches("llm_capture_");
        let Some((t_tok, after_t)) = rest.split_once('_') else {
            continue;
        };
        let Some(t_num) = t_tok.strip_prefix('t').and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some((r_tok, _rest)) = after_t.split_once('_').or_else(|| after_t.split_once('.'))
        else {
            continue;
        };
        let Some(r_num) = r_tok.strip_prefix('r').and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        rows.push((t_num, r_num, path));
    }
    rows.sort_by_key(|(t, r, _)| (*t, *r));

    let mut out = Vec::with_capacity(rows.len());
    for (_, _, path) in rows {
        let text = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "astra_turn_core::cache_diagnosis",
                    path = %path.display(),
                    err = %e,
                    "skipping unreadable capture file",
                );
                continue;
            }
        };
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "astra_turn_core::cache_diagnosis",
                    path = %path.display(),
                    err = %e,
                    "skipping malformed capture file",
                );
                continue;
            }
        };
        out.push(snapshot_from_capture_json(&v));
    }
    Ok(out)
}

/// Run all rules over a recent-history slice and return findings in
/// deterministic (rule_id) order.
#[must_use]
pub fn evaluate_all(rounds: &[RoundSnapshot]) -> Vec<CacheFinding> {
    let mut out = Vec::new();
    if let Some(f) = rule_cc_marker_frozen(rounds) {
        out.push(f);
    }
    if let Some(f) = rule_tool_marker_not_on_tail(rounds) {
        out.push(f);
    }
    if let Some(f) = rule_cache_read_collapsed(rounds) {
        out.push(f);
    }
    if let Some(f) = rule_cache_creation_waste(rounds) {
        out.push(f);
    }
    if let Some(f) = rule_volatile_in_cached_prefix(rounds) {
        out.push(f);
    }
    out.sort_by_key(|f| f.rule_id);
    out
}

/// **Rule 1 — cc_marker_frozen.**
///
/// Within a single turn (same `turn` id), if the message-level
/// `cache_control` indices are **byte-identical** across 3+ consecutive
/// rounds, the rolling-breakpoint scheme has failed. This is the
/// session-d0640d3d bedrock pathology: a 14-round tool loop where cc
/// was stuck at `[0, 8, 10]` while message_count grew from 12 to 40.
///
/// Scope: only fires on Anthropic-protocol providers (others don't
/// carry message_cc_indices at all).
#[must_use]
fn rule_cc_marker_frozen(rounds: &[RoundSnapshot]) -> Option<CacheFinding> {
    // Group by turn; we only care about within-turn patterns.
    let mut by_turn: std::collections::BTreeMap<u32, Vec<&RoundSnapshot>> =
        std::collections::BTreeMap::new();
    for r in rounds {
        if r.message_cc_indices.is_empty() {
            continue;
        }
        by_turn.entry(r.turn).or_default().push(r);
    }
    for (turn, group) in by_turn {
        if group.len() < 3 {
            continue;
        }
        // Sort by round so adjacency is meaningful.
        let mut sorted: Vec<&RoundSnapshot> = group.into_iter().collect();
        sorted.sort_by_key(|r| r.round);
        // Find any 3-round run with identical cc_indices.
        let mut run_start = 0usize;
        for i in 1..sorted.len() {
            if sorted[i].message_cc_indices != sorted[run_start].message_cc_indices {
                run_start = i;
                continue;
            }
            if i - run_start >= 2 {
                // We have a run of length >=3 with identical indices.
                let triggered: Vec<(u32, u32)> = sorted[run_start..=i]
                    .iter()
                    .map(|r| (r.turn, r.round))
                    .collect();
                let frozen = &sorted[run_start].message_cc_indices;
                return Some(CacheFinding {
                    rule_id: "cc_marker_frozen",
                    severity: Severity::Critical,
                    narrative: format!(
                        "turn {turn}: message-level cache_control indices {frozen:?} \
                        stayed identical across {} consecutive rounds while the \
                         message count grew — the tail breakpoint isn't advancing \
                         (agentic tool-loop regression).",
                        triggered.len(),
                    ),
                    actionable_fix:
                        "Verify `find_message_cache_breakpoint_target` keeps walking to the \
                         newest non-system tail when the conversation extends past the last \
                         user message, while still skipping trailing system-only injections."
                            .into(),
                    triggered_on: triggered,
                });
            }
        }
    }
    None
}

/// **Rule 2 — tool_marker_not_on_tail.**
///
/// If the always-load tool cache_control marker lands at an index strictly
/// before the last tool, every tool after it falls outside the cached
/// prefix. In session d0640d3d the always-load set omitted `web_search`
/// (idx 20 of 21), so the marker landed on `skill` (idx 19) and every
/// request paid tokens on web_search's schema.
#[must_use]
fn rule_tool_marker_not_on_tail(rounds: &[RoundSnapshot]) -> Option<CacheFinding> {
    // We only need one example; use the most recent round with a marker.
    let sample = rounds
        .iter()
        .rev()
        .find(|r| r.tool_cc_index.is_some() && r.tool_count > 0)?;
    let marker_idx = sample.tool_cc_index.unwrap();
    let last_idx = sample.tool_count.saturating_sub(1);
    if marker_idx >= last_idx {
        return None;
    }
    let gap = last_idx - marker_idx;
    Some(CacheFinding {
        rule_id: "tool_marker_not_on_tail",
        severity: Severity::Warn,
        narrative: format!(
            "tool cache_control marker sits at index {marker_idx} of {total} — \
             the trailing {gap} tool schema{s} fall outside the cached prefix \
             and are re-tokenized every request.",
            total = sample.tool_count,
            s = if gap == 1 { "" } else { "s" },
        ),
        actionable_fix: "If the trailing schemas are meant to be static every turn, audit \
             `default_always_load_tool_names()`; deferred or turn-selected tools after the marker \
             are expected to sit outside the cached prefix."
            .into(),
        triggered_on: vec![(sample.turn, sample.round)],
    })
}

/// **Rule 3 — cache_read_collapsed.**
///
/// Within a single turn and provider, if `cache_read_tokens` drops by
/// more than 50% between consecutive rounds, a previously-stable cache
/// entry just got invalidated — typically because a message earlier in
/// the history changed bytes (self-awareness block, volatile system
/// content, etc. leaking into the cached prefix).
#[must_use]
fn rule_cache_read_collapsed(rounds: &[RoundSnapshot]) -> Option<CacheFinding> {
    // Walk consecutive round pairs within the same (turn, provider).
    let mut sorted: Vec<&RoundSnapshot> = rounds.iter().collect();
    sorted.sort_by_key(|r| (r.turn, r.round));
    for pair in sorted.windows(2) {
        let (prev, curr) = (pair[0], pair[1]);
        if prev.turn != curr.turn || prev.provider != curr.provider {
            continue;
        }
        // `turn` + `round` in the fixture index are captured from the
        // LLM exchange's local counters, which can legitimately repeat
        // (e.g. different `astra chat` invocations each start at
        // turn=1 round=0). When we see duplicate (turn, round) pairs
        // we're looking at unrelated invocations, not a prefix
        // collapse — skip them to avoid false positives.
        if prev.round == curr.round {
            continue;
        }
        if prev.cache_read_tokens < 1_000 {
            // Too small to trigger — avoid false positives on first
            // rounds where cache isn't warm yet.
            continue;
        }
        if (curr.cache_read_tokens as f64) < (prev.cache_read_tokens as f64) * 0.5 {
            return Some(CacheFinding {
                rule_id: "cache_read_collapsed",
                severity: Severity::Critical,
                narrative: format!(
                    "turn {turn} round {prev_round}→{curr_round} ({prov}): \
                     cache_read dropped from {prev_r} to {curr_r} tokens \
                     — a stable prefix got invalidated.",
                    turn = curr.turn,
                    prev_round = prev.round,
                    curr_round = curr.round,
                    prov = curr.provider,
                    prev_r = prev.cache_read_tokens,
                    curr_r = curr.cache_read_tokens,
                ),
                actionable_fix: "Diff messages[0..=prev_tail_cc] bytes between rounds. Common \
                     causes: volatile content (Self-Awareness, turn counter) leaked \
                     into the system block, or a historical cache_control got \
                     silently stripped between rounds."
                    .into(),
                triggered_on: vec![(prev.turn, prev.round), (curr.turn, curr.round)],
            });
        }
    }
    None
}

/// **Rule 4 — cache_creation_waste.**
///
/// Within a single turn, if the ratio of **post-first-round**
/// `cache_creation / cache_read` exceeds 0.3 across 3+ rounds, the
/// runtime is repeatedly rebuilding the cached prefix instead of
/// amortizing it. Session d0640d3d t6 fires cleanly (14 rounds, 94%
/// waste); bc5764b6 t5 (2 rounds, first-round-heavy) does NOT fire
/// — the 10 K of first-round creation is inherent, not waste.
///
/// Why skip the first round: cache_creation on round 0 is the
/// *cost of first filling the cache entry*. Counting it against the
/// ratio effectively penalizes **any** turn that started with a
/// fresh session. The pathology the rule is built to catch is
/// repeated re-creation across tool-loop rounds, which only shows
/// up from round 2 onward.
///
/// Why require 3+ rounds: a 2-round sample means 1 "real" data
/// point after dropping round 0 — not enough to conclude churn vs.
/// single-round artifact.
#[must_use]
fn rule_cache_creation_waste(rounds: &[RoundSnapshot]) -> Option<CacheFinding> {
    // Aggregate per-turn on anthropic-protocol providers (openai-compat
    // returns cache_creation=0 by definition). Track rounds individually
    // so we can drop the earliest one before computing the ratio.
    use std::collections::BTreeMap;
    let mut per_turn: BTreeMap<u32, Vec<&RoundSnapshot>> = BTreeMap::new();
    for r in rounds {
        if r.cache_read_tokens == 0 && r.cache_creation_tokens == 0 {
            continue;
        }
        per_turn.entry(r.turn).or_default().push(r);
    }
    for (turn, mut group) in per_turn {
        if group.len() < 3 {
            // Need >=3 so we have >=2 "real" rounds after dropping
            // the first.
            continue;
        }
        group.sort_by_key(|r| r.round);
        // Drop the first round: its cache_creation is the inherent
        // cost of filling the cache, not waste.
        let amortized = &group[1..];
        let reads: u64 = amortized.iter().map(|r| r.cache_read_tokens).sum();
        let creations: u64 = amortized.iter().map(|r| r.cache_creation_tokens).sum();
        if reads < 5_000 {
            // Too little post-amortization signal to decide.
            continue;
        }
        let ratio = creations as f64 / reads as f64;
        if ratio > 0.3 {
            let pct = (ratio * 100.0).round() as u32;
            let trigs: Vec<(u32, u32)> = amortized.iter().map(|r| (r.turn, r.round)).collect();
            return Some(CacheFinding {
                rule_id: "cache_creation_waste",
                severity: Severity::Warn,
                narrative: format!(
                    "turn {turn}: cache_creation/cache_read ratio is {pct}% \
                     (creation={creations}, read={reads}) across {n} post-first \
                     rounds — the cache is being rebuilt, not amortized. \
                     (First round's creation is excluded as it's the natural \
                     fill cost.)",
                    n = trigs.len(),
                ),
                actionable_fix: "Common causes: tail breakpoint not advancing (check \
                     cc_marker_frozen first), mid-prefix volatile content, or \
                     tool-schema drift between rounds."
                    .into(),
                triggered_on: trigs,
            });
        }
    }
    None
}

/// **Rule 5 — volatile_in_cached_prefix.**
///
/// Each round the runtime injects "volatile" content (Self-Awareness
/// block, session-memory manifest, attention manifest) somewhere in
/// the request. Where that content may safely live depends on the
/// provider's prompt-cache semantics (see [`crate::cache_placement`]):
///
/// - `MarkerIsolated` providers (Anthropic / Bedrock): volatile content
///   must sit AFTER the last `cache_control` marker. Before the marker
///   is still in the cached prefix and every round's new bytes poison
///   the cache.
/// - `TailSuffix` providers (OpenAI auto-prefix): volatile content
///   must be in the LAST message only. Earlier positions are inside
///   the auto-prefix and break on every change.
/// - `CurrentUserOnly` providers (MiniMax strict history): volatile
///   content may only appear on round 0 of a visible turn. Session
///   986a553e observed volatile bytes at msg[7] in every tool-loop
///   round, causing cache_read to collapse from 7680 to 0 for six
///   consecutive rounds.
/// - `Free` / unknown: not enforced.
///
/// Signal is provider+model aware; wrong-placement gets Critical,
/// matching-placement is silent. If `volatile_msg_indices` is empty,
/// the rule has nothing to check → silent.
#[must_use]
fn rule_volatile_in_cached_prefix(rounds: &[RoundSnapshot]) -> Option<CacheFinding> {
    use crate::cache_placement::{CacheCapability, VolatilePlacement};
    // Take the most recent round with any volatile signal at a
    // message position we actually police. System messages carry
    // their own block-level cache_control layout (runtime owns that);
    // the rule is specifically about volatile bytes appearing at
    // user/assistant/tool mid-history positions where message-level
    // cc alone can't protect them.
    let sample = rounds.iter().rev().find(|r| {
        r.volatile_msg_indices
            .iter()
            .any(|&idx| role_at(r, idx) != "system")
    })?;
    let cap = CacheCapability::for_provider_and_model(&sample.provider, &sample.model);
    // Last non-system volatile index — that's the relevant one.
    let vol_idx = *sample
        .volatile_msg_indices
        .iter()
        .rev()
        .find(|&&idx| role_at(sample, idx) != "system")?;
    let count = sample.message_count;
    if count == 0 {
        return None;
    }
    let tail_idx = count - 1;

    match cap.volatile_placement {
        VolatilePlacement::MarkerIsolated => {
            // For anthropic-protocol providers the volatile content must
            // sit after every cache_control marker. If any cc marker
            // index is >= the volatile index, the volatile bytes are
            // inside the cached prefix.
            let offenders: Vec<u32> = sample
                .message_cc_indices
                .iter()
                .copied()
                .filter(|&cc| cc >= vol_idx)
                .collect();
            if offenders.is_empty() {
                return None;
            }
            Some(CacheFinding {
                rule_id: "volatile_in_cached_prefix",
                severity: Severity::Critical,
                narrative: format!(
                    "volatile content at msg[{vol_idx}] is inside the cached \
                     prefix: cache_control markers at {offenders:?} sit on or \
                     after it, so per-round changes invalidate the cache.",
                ),
                actionable_fix: "Move the volatile block AFTER the last cache_control \
                     marker, or emit it as a new content block after the \
                     final marker in the system message."
                    .into(),
                triggered_on: vec![(sample.turn, sample.round)],
            })
        }
        VolatilePlacement::TailSuffix => {
            // Last message must carry all volatile content; anything
            // before is inside the auto-prefix.
            if vol_idx == tail_idx {
                return None;
            }
            Some(CacheFinding {
                rule_id: "volatile_in_cached_prefix",
                severity: Severity::Critical,
                narrative: format!(
                    "volatile content at msg[{vol_idx}] of {count} is inside \
                     the OpenAI auto-prefix range — anything before the last \
                     message (msg[{tail_idx}]) breaks cache on every change.",
                ),
                actionable_fix: "Append volatile content to the final user message's body \
                     rather than a mid-history synthetic preamble."
                    .into(),
                triggered_on: vec![(sample.turn, sample.round)],
            })
        }
        VolatilePlacement::CurrentUserOnly => {
            // MiniMax-style strict history: volatile injection must be
            // skipped on EVERY round. A round-0-only injection still
            // makes msg[1] bytes differ vs round 1+, so cache misses
            // anyway — see `VolatilePlacement::CurrentUserOnly` docs.
            // Any sample with volatile content is a violation.
            Some(CacheFinding {
                rule_id: "volatile_in_cached_prefix",
                severity: Severity::Critical,
                narrative: format!(
                    "{prov} ({model}) uses strict-history prompt cache; \
                     volatile content at msg[{vol_idx}] on round {round} \
                     invalidates the turn's cache — strict-history \
                     providers cannot tolerate volatile bytes anywhere in \
                     the history, including round 0.",
                    prov = sample.provider,
                    model = sample.model,
                    round = sample.round,
                ),
                actionable_fix: "Suppress volatile-content injection entirely for this \
                     provider. See \
                     `CacheCapability::should_inject_volatile_on_round`."
                    .into(),
                triggered_on: vec![(sample.turn, sample.round)],
            })
        }
        VolatilePlacement::Free => None,
    }
}

// NOTE: `rule_deepseek_anthropic_tools_not_cached` was removed after
// the original "provider silently ignores tool-level cache_control"
// claim was falsified by controlled probes. With 21 tools + astra's
// 4-block system payload + tool-level cc marker on the last tool,
// DeepSeek's `/anthropic` endpoint cached 9088 of 9116 input tokens
// (99%) on the warm call — and kept caching even with no tool-level
// marker at all (auto-prefix over the tools array). The 2432-flat
// pattern seen in production sessions therefore has an astra-side
// cause (something in the per-round wire payload varies); keep the
// telemetry but stop diagnosing the endpoint. See
// `tests/fixtures/deepseek_anthropic_cache_probe.py` for the
// reproduction.

/// Render findings + round-level aggregates as markdown suitable for
/// returning to the LLM from `introspect(facet="cache")`. Designed
/// to be informative but compact (~30-60 lines depending on the
/// number of findings).
///
/// When `rounds` is empty — e.g. `full_llm_capture` was off and the
/// runtime has no per-turn ring yet — the output explains *why* it
/// can't diagnose so the LLM doesn't waste follow-up questions.
#[must_use]
pub fn render_findings_markdown(rounds: &[RoundSnapshot], findings: &[CacheFinding]) -> String {
    let mut out = String::new();
    out.push_str("## Cache Diagnosis\n\n");

    if rounds.is_empty() {
        out.push_str(
            "_No per-round cache snapshots available in this session._ \
             Enable `full_llm_capture=true` in the session metadata to \
             populate `~/.astra/sessions/{sid}/llm_capture_t{N}_r{M}_*.json`, \
             or wait for the in-memory per-turn ring (runtime wiring \
             pending) to accumulate rounds.\n",
        );
        return out;
    }

    // Rounds overview — single table row per (turn, round).
    out.push_str(
        "| turn | round | provider  | cache_read | cache_creation | msg_cc | tool_cc / total |\n",
    );
    out.push_str(
        "| ---- | ----- | --------- | ---------- | -------------- | ------ | ---------------- |\n",
    );
    for r in rounds {
        let msg_cc_str = if r.message_cc_indices.is_empty() {
            "—".to_string()
        } else {
            format!("{:?}", r.message_cc_indices)
        };
        let tool_cc_str = match r.tool_cc_index {
            Some(i) => format!("{} / {}", i, r.tool_count),
            None => format!("— / {}", r.tool_count),
        };
        out.push_str(&format!(
            "| {:>4} | {:>5} | {:<9} | {:>10} | {:>14} | {} | {} |\n",
            r.turn,
            r.round,
            r.provider,
            r.cache_read_tokens,
            r.cache_creation_tokens,
            msg_cc_str,
            tool_cc_str,
        ));
    }
    out.push('\n');

    // Findings section.
    if findings.is_empty() {
        out.push_str("### Findings\n\nNo regressions detected. ✓\n");
        return out;
    }
    out.push_str(&format!("### Findings ({})\n\n", findings.len()));
    for f in findings {
        let sev_icon = match f.severity {
            Severity::Info => "ℹ",
            Severity::Warn => "⚠",
            Severity::Critical => "🔴",
        };
        out.push_str(&format!(
            "- **{icon} `{id}`** — {narr}\n  - **Fix:** {fix}\n  - Triggered on: {trig:?}\n",
            icon = sev_icon,
            id = f.rule_id,
            narr = f.narrative,
            fix = f.actionable_fix,
            trig = f.triggered_on,
        ));
    }
    out
}

// ═════════════════════════════════════════════════════════════════════════
// Tests
// ═════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn snap(
        turn: u32,
        round: u32,
        provider: &str,
        cr: u64,
        cc: u64,
        msg_cc: &[u32],
        tool_count: u32,
        tool_cc: Option<u32>,
    ) -> RoundSnapshot {
        RoundSnapshot {
            turn,
            round,
            provider: provider.into(),
            model: "test-model".into(),
            cache_read_tokens: cr,
            cache_creation_tokens: cc,
            tool_count,
            tool_cc_index: tool_cc,
            message_cc_indices: msg_cc.to_vec(),
            volatile_msg_indices: Vec::new(),
            message_count: 0,
            message_roles: Vec::new(),
        }
    }

    /// Extended test helper for the volatile-in-cached-prefix rule.
    /// Defaults `message_roles` to `user` for every index so the rule
    /// reasons about user/tool mid-history placements. Tests that
    /// specifically exercise system-block behavior should pass custom
    /// roles via `snap_with_volatile_and_roles`.
    fn snap_with_volatile(
        turn: u32,
        round: u32,
        provider: &str,
        model: &str,
        msg_cc: &[u32],
        volatile_indices: &[u32],
        message_count: u32,
    ) -> RoundSnapshot {
        snap_with_volatile_and_roles(
            turn,
            round,
            provider,
            model,
            msg_cc,
            volatile_indices,
            message_count,
            &vec!["user"; message_count as usize],
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn snap_with_volatile_and_roles(
        turn: u32,
        round: u32,
        provider: &str,
        model: &str,
        msg_cc: &[u32],
        volatile_indices: &[u32],
        message_count: u32,
        roles: &[&str],
    ) -> RoundSnapshot {
        RoundSnapshot {
            turn,
            round,
            provider: provider.into(),
            model: model.into(),
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_count: 0,
            tool_cc_index: None,
            message_cc_indices: msg_cc.to_vec(),
            volatile_msg_indices: volatile_indices.to_vec(),
            message_count,
            message_roles: roles.iter().map(|&s| s.to_string()).collect(),
        }
    }

    // ── Rule 1: cc_marker_frozen ────────────────────────────────────────
    #[test]
    fn cc_marker_frozen_fires_on_three_identical_rounds() {
        // Mimics session d0640d3d t6: cc frozen at [0, 8, 10] across
        // 14 rounds. Three rounds is our minimum for triggering.
        let rs = vec![
            snap(6, 0, "bedrock", 11312, 2814, &[0, 8, 10], 21, Some(19)),
            snap(6, 1, "bedrock", 11312, 2958, &[0, 8, 10], 21, Some(19)),
            snap(6, 2, "bedrock", 11312, 3048, &[0, 8, 10], 21, Some(19)),
        ];
        let findings = evaluate_all(&rs);
        let frozen = findings.iter().find(|f| f.rule_id == "cc_marker_frozen");
        assert!(frozen.is_some(), "rule must fire; findings={findings:?}");
        let f = frozen.unwrap();
        assert_eq!(f.severity, Severity::Critical);
        assert_eq!(f.triggered_on.len(), 3);
    }

    #[test]
    fn cc_marker_frozen_does_not_fire_when_indices_advance() {
        // Healthy rolling: indices advance between rounds.
        let rs = vec![
            snap(6, 0, "bedrock", 11312, 2814, &[0, 4], 21, Some(19)),
            snap(6, 1, "bedrock", 11312, 2958, &[0, 4, 6], 21, Some(19)),
            snap(6, 2, "bedrock", 11312, 3048, &[0, 6, 8], 21, Some(19)),
        ];
        let findings = evaluate_all(&rs);
        assert!(
            !findings.iter().any(|f| f.rule_id == "cc_marker_frozen"),
            "rule must NOT fire on advancing indices; findings={findings:?}",
        );
    }

    #[test]
    fn cc_marker_frozen_requires_three_consecutive_rounds() {
        // Only 2 rounds with matching cc — not enough.
        let rs = vec![
            snap(6, 0, "bedrock", 11312, 2814, &[0, 8, 10], 21, Some(19)),
            snap(6, 1, "bedrock", 11312, 2958, &[0, 8, 10], 21, Some(19)),
        ];
        let findings = evaluate_all(&rs);
        assert!(
            !findings.iter().any(|f| f.rule_id == "cc_marker_frozen"),
            "2 rounds shouldn't trigger; need >=3. findings={findings:?}",
        );
    }

    #[test]
    fn cc_marker_frozen_ignores_openai_providers() {
        // OpenAI-compatible providers don't populate message_cc_indices;
        // the rule must not fire on empty sets.
        let rs = vec![
            snap(6, 0, "openai", 0, 0, &[], 21, None),
            snap(6, 1, "openai", 0, 0, &[], 21, None),
            snap(6, 2, "openai", 0, 0, &[], 21, None),
        ];
        assert!(evaluate_all(&rs).is_empty());
    }

    // ── Rule 2: tool_marker_not_on_tail ─────────────────────────────────
    #[test]
    fn tool_marker_not_on_tail_fires_when_marker_before_last() {
        // Session d0640d3d t3: 21 tools, marker on idx 19 (skill),
        // last idx is 20 (web_search).
        let rs = vec![snap(3, 0, "anthropic", 2432, 0, &[0, 2, 4], 21, Some(19))];
        let findings = evaluate_all(&rs);
        let f = findings
            .iter()
            .find(|f| f.rule_id == "tool_marker_not_on_tail")
            .expect("rule must fire");
        assert_eq!(f.severity, Severity::Warn);
        assert!(
            f.narrative.contains("index 19") && f.narrative.contains("of 21"),
            "narrative should name the gap: {}",
            f.narrative,
        );
    }

    #[test]
    fn tool_marker_not_on_tail_silent_when_marker_on_last() {
        // Healthy: 21 tools, cc on idx 20.
        let rs = vec![snap(3, 0, "anthropic", 2432, 0, &[0, 2, 4], 21, Some(20))];
        assert!(!evaluate_all(&rs)
            .iter()
            .any(|f| f.rule_id == "tool_marker_not_on_tail"),);
    }

    #[test]
    fn tool_marker_not_on_tail_silent_when_no_tool_marker() {
        // openai-compat: tool_cc_index is None. Rule is Anthropic-only.
        let rs = vec![snap(3, 0, "openai", 0, 0, &[], 21, None)];
        assert!(evaluate_all(&rs).is_empty());
    }

    // ── Rule 3: cache_read_collapsed ────────────────────────────────────
    #[test]
    fn cache_read_collapsed_fires_on_50pct_drop() {
        let rs = vec![
            snap(3, 0, "anthropic", 10000, 0, &[], 21, Some(20)),
            snap(3, 1, "anthropic", 2432, 100, &[], 21, Some(20)),
        ];
        let findings = evaluate_all(&rs);
        let f = findings
            .iter()
            .find(|f| f.rule_id == "cache_read_collapsed")
            .expect("rule must fire on >50% drop");
        assert_eq!(f.severity, Severity::Critical);
    }

    #[test]
    fn cache_read_collapsed_silent_on_modest_variance() {
        // 10% drop is within normal tokenizer variance.
        let rs = vec![
            snap(3, 0, "anthropic", 10000, 0, &[], 21, Some(20)),
            snap(3, 1, "anthropic", 9000, 100, &[], 21, Some(20)),
        ];
        assert!(!evaluate_all(&rs)
            .iter()
            .any(|f| f.rule_id == "cache_read_collapsed"),);
    }

    #[test]
    fn cache_read_collapsed_skips_cold_starts() {
        // First round has tiny cache_read — dropping to 0 next round
        // isn't a regression, just cache warmup noise.
        let rs = vec![
            snap(3, 0, "anthropic", 500, 9000, &[], 21, Some(20)),
            snap(3, 1, "anthropic", 0, 100, &[], 21, Some(20)),
        ];
        assert!(!evaluate_all(&rs)
            .iter()
            .any(|f| f.rule_id == "cache_read_collapsed"),);
    }

    #[test]
    fn cache_read_collapsed_ignores_cross_provider_jumps() {
        // Switching providers legitimately resets cache — not a regression.
        let rs = vec![
            snap(3, 0, "anthropic", 10000, 0, &[], 21, Some(20)),
            snap(3, 1, "bedrock", 0, 9000, &[], 21, Some(20)),
        ];
        assert!(!evaluate_all(&rs)
            .iter()
            .any(|f| f.rule_id == "cache_read_collapsed"),);
    }

    // ── Rule 4: cache_creation_waste ────────────────────────────────────
    #[test]
    fn cache_creation_waste_fires_on_sustained_churn() {
        // 3+ rounds, and AFTER dropping the first round the ratio is
        // still 0.4 — that's real post-amortization waste. Mirrors
        // session d0640d3d t6's 14-round pattern collapsed to 3.
        let rs = vec![
            // Round 0: inherent first-round creation. Dropped by the rule.
            snap(6, 0, "bedrock", 0, 100_000, &[0, 8, 10], 21, Some(19)),
            snap(6, 1, "bedrock", 50_000, 20_000, &[0, 8, 10], 21, Some(19)),
            snap(6, 2, "bedrock", 50_000, 20_000, &[0, 8, 10], 21, Some(19)),
        ];
        let findings = evaluate_all(&rs);
        let f = findings
            .iter()
            .find(|f| f.rule_id == "cache_creation_waste")
            .expect("rule must fire on sustained post-first-round churn");
        assert_eq!(f.severity, Severity::Warn);
        assert!(
            f.narrative.contains("post-first"),
            "narrative should clarify that first-round was excluded: {}",
            f.narrative,
        );
    }

    /// Regression from session bc5764b6: 2-round bedrock turn where
    /// round 0 creates a lot (fill) and round 1 reads. The old rule
    /// lumped both rounds together and reported 134% ratio — a false
    /// positive because the creation was the inherent first-round cost.
    /// With the updated rule (drop round 0, require >=3 rounds total),
    /// this 2-round session stays silent.
    #[test]
    fn cache_creation_waste_silent_on_first_round_heavy_short_session() {
        let rs = vec![
            // Round 0 fills cache (high creation, zero read). Dropped.
            snap(5, 0, "bedrock", 8, 10_628, &[0, 6, 8], 21, Some(20)),
            // Round 1 reads most of it back. Healthy churn.
            snap(5, 1, "bedrock", 8_519, 807, &[0, 12], 21, Some(20)),
        ];
        assert!(
            !evaluate_all(&rs)
                .iter()
                .any(|f| f.rule_id == "cache_creation_waste"),
            "rule must not fire on 2-round sessions with first-round-heavy \
             creation (session bc5764b6 regression)",
        );
    }

    #[test]
    fn cache_creation_waste_silent_at_healthy_ratio() {
        // 3 rounds, low creation throughout.
        let rs = vec![
            snap(6, 0, "bedrock", 0, 10_000, &[0, 4], 21, Some(20)),
            snap(6, 1, "bedrock", 50_000, 500, &[0, 4, 6], 21, Some(20)),
            snap(6, 2, "bedrock", 50_000, 500, &[0, 6, 8], 21, Some(20)),
        ];
        assert!(!evaluate_all(&rs)
            .iter()
            .any(|f| f.rule_id == "cache_creation_waste"),);
    }

    #[test]
    fn cache_creation_waste_requires_multiple_rounds() {
        // A single round with ANY cache_creation doesn't imply waste —
        // the first round of any turn must create.
        let rs = vec![snap(6, 0, "bedrock", 0, 10_000, &[0], 21, Some(20))];
        assert!(!evaluate_all(&rs)
            .iter()
            .any(|f| f.rule_id == "cache_creation_waste"),);
    }

    #[test]
    fn cache_creation_waste_requires_three_rounds_not_just_two() {
        // 2 rounds with sustained waste: old rule would fire. New rule
        // (>=3 required) stays silent to avoid over-eager alerts on
        // short sessions.
        let rs = vec![
            snap(6, 0, "bedrock", 50_000, 20_000, &[0, 4], 21, Some(20)),
            snap(6, 1, "bedrock", 50_000, 20_000, &[0, 4, 6], 21, Some(20)),
        ];
        assert!(
            !evaluate_all(&rs)
                .iter()
                .any(|f| f.rule_id == "cache_creation_waste"),
            "rule requires 3+ rounds after the updated threshold (need >=2 \
             post-first rounds to establish a trend)",
        );
    }

    // ── evaluate_all dispatch ──────────────────────────────────────────
    #[test]
    fn evaluate_all_returns_findings_in_stable_order() {
        // Two rules fire; rule_id ascending order must be deterministic.
        let rs = vec![
            snap(6, 0, "bedrock", 11312, 2814, &[0, 8, 10], 21, Some(19)),
            snap(6, 1, "bedrock", 11312, 2958, &[0, 8, 10], 21, Some(19)),
            snap(6, 2, "bedrock", 11312, 3048, &[0, 8, 10], 21, Some(19)),
        ];
        let findings = evaluate_all(&rs);
        let ids: Vec<&str> = findings.iter().map(|f| f.rule_id).collect();
        assert_eq!(ids, ["cc_marker_frozen", "tool_marker_not_on_tail"],);
    }

    #[test]
    fn evaluate_all_empty_on_clean_session() {
        // A healthy anthropic session with rolling cc and marker on last tool.
        let rs = vec![
            snap(1, 0, "anthropic", 10_000, 500, &[0, 4], 21, Some(20)),
            snap(1, 1, "anthropic", 10_500, 500, &[0, 4, 6], 21, Some(20)),
        ];
        let findings = evaluate_all(&rs);
        assert!(findings.is_empty(), "healthy session: {findings:?}");
    }

    // ── snapshot_from_capture_json parser ──────────────────────────────

    #[test]
    fn snapshot_from_capture_reads_canonical_fields() {
        let v = serde_json::json!({
            "turn": 3,
            "round": 0,
            "provider": "anthropic",
            "model": "claude-test",
            "request": {
                "message_count": 4,
                "tool_count": 21,
                "messages": [
                    {"role": "system", "content": [
                        {"type": "text", "text": "sys"},
                        {"type": "text", "text": "ctx", "cache_control": {"type": "ephemeral"}}
                    ]},
                    {"role": "user", "content": "hi"},
                    {"role": "assistant", "content": [
                        {"type": "text", "text": "ok", "cache_control": {"type": "ephemeral"}}
                    ]},
                    {"role": "user", "content": "again"}
                ],
                "tools": [
                    {"function": {"name": "bash"}},
                    {"function": {"name": "skill"}, "cache_control": {"type": "ephemeral"}},
                    {"function": {"name": "web_search"}}
                ]
            },
            "response": {
                "usage": {
                    "cached_input_tokens": 2432,
                    "cache_creation_tokens": 100,
                    "input_tokens": 500,
                    "output_tokens": 40
                }
            }
        });
        let snap = snapshot_from_capture_json(&v);
        assert_eq!(snap.turn, 3);
        assert_eq!(snap.round, 0);
        assert_eq!(snap.provider, "anthropic");
        assert_eq!(snap.cache_read_tokens, 2432);
        assert_eq!(snap.cache_creation_tokens, 100);
        assert_eq!(snap.tool_count, 21);
        // cc on tool[1]=skill — we want the LAST tool carrying cc (would
        // catch a multi-cc misconfiguration).
        assert_eq!(snap.tool_cc_index, Some(1));
        // message-level cc: system (idx 0) and assistant (idx 2).
        assert_eq!(snap.message_cc_indices, vec![0, 2]);
    }

    #[test]
    fn snapshot_from_capture_degrades_on_missing_fields() {
        // Only the bare minimum — everything else should default cleanly.
        let v = serde_json::json!({});
        let snap = snapshot_from_capture_json(&v);
        assert_eq!(snap.turn, 0);
        assert_eq!(snap.cache_read_tokens, 0);
        assert_eq!(snap.tool_cc_index, None);
        assert!(snap.message_cc_indices.is_empty());
    }

    // ── load_session_captures loader ───────────────────────────────────

    #[test]
    fn load_session_captures_returns_empty_when_dir_missing() {
        let result = load_session_captures(std::path::Path::new(
            "/tmp/definitely-does-not-exist-astra-cache-test",
        ))
        .expect("missing dir should be Ok(empty), not Err");
        assert!(result.is_empty());
    }

    // ── render_findings_markdown ───────────────────────────────────────

    #[test]
    fn render_findings_explains_when_no_data_available() {
        let out = render_findings_markdown(&[], &[]);
        assert!(out.contains("full_llm_capture"), "got: {out}");
        assert!(
            out.contains("No per-round cache snapshots"),
            "must say why no data: {out}",
        );
    }

    #[test]
    fn render_findings_shows_clean_session() {
        let rs = vec![
            snap(1, 0, "anthropic", 10_000, 500, &[0, 4], 21, Some(20)),
            snap(1, 1, "anthropic", 10_500, 500, &[0, 4, 6], 21, Some(20)),
        ];
        let findings = evaluate_all(&rs);
        let out = render_findings_markdown(&rs, &findings);
        assert!(out.contains("No regressions detected"), "got:\n{out}");
        assert!(out.contains("anthropic"), "rounds table must appear: {out}");
        // The turn/round grid should reference the actual (turn=1, round=0) row.
        assert!(out.contains("|    1 |     0 |"), "round row missing: {out}");
    }

    #[test]
    fn render_findings_lists_critical_and_warn() {
        let rs = vec![
            snap(6, 0, "bedrock", 11312, 2814, &[0, 8, 10], 21, Some(19)),
            snap(6, 1, "bedrock", 11312, 2958, &[0, 8, 10], 21, Some(19)),
            snap(6, 2, "bedrock", 11312, 3048, &[0, 8, 10], 21, Some(19)),
        ];
        let findings = evaluate_all(&rs);
        let out = render_findings_markdown(&rs, &findings);
        // Both rules should be named.
        assert!(out.contains("cc_marker_frozen"), "got:\n{out}");
        assert!(out.contains("tool_marker_not_on_tail"), "got:\n{out}");
        // Severity icons surface.
        assert!(out.contains("🔴"), "critical icon missing: {out}");
        assert!(out.contains("⚠"), "warn icon missing: {out}");
        // Actionable fix text included.
        assert!(
            out.contains("tail breakpoint") || out.contains("tail"),
            "fix text should mention the tail breakpoint: {out}",
        );
    }

    // ── Rule 5: volatile_in_cached_prefix ──────────────────────────────

    #[test]
    fn volatile_rule_fires_on_minimax_tool_loop_round() {
        // Session 986a553e fingerprint: MiniMax tool-loop round 1+ with
        // `## Self-Awareness` injected at a mid-history user message.
        let rs = vec![snap_with_volatile(
            4,
            1,
            "openai",
            "MiniMax-M2.7",
            /* msg_cc */ &[],
            /* volatile */ &[7],
            /* message_count */ 11,
        )];
        let findings = evaluate_all(&rs);
        let f = findings
            .iter()
            .find(|f| f.rule_id == "volatile_in_cached_prefix")
            .expect("rule must fire on MiniMax tool-loop round >0");
        assert_eq!(f.severity, Severity::Critical);
        assert!(
            f.narrative.contains("strict-history"),
            "narrative must explain why: {}",
            f.narrative,
        );
    }

    #[test]
    fn volatile_rule_fires_on_minimax_round_zero_too() {
        // Updated contract (see CurrentUserOnly docs): even round 0
        // volatile injection on MiniMax is a cache-miss trigger,
        // because round 1+ won't have it and bytes at msg[1] differ.
        let rs = vec![snap_with_volatile(
            4,
            0,
            "openai",
            "MiniMax-M2.7",
            &[],
            &[7],
            8,
        )];
        let findings = evaluate_all(&rs);
        assert!(
            findings
                .iter()
                .any(|f| f.rule_id == "volatile_in_cached_prefix"),
            "rule must fire even on round 0 for strict-history providers; got {findings:?}",
        );
    }

    #[test]
    fn volatile_rule_fires_on_anthropic_when_volatile_inside_cached_prefix() {
        // Anthropic: cc_indices=[0, 10] (system marker + tail marker);
        // volatile at msg[8] sits BEFORE the tail marker → inside
        // the cached prefix → rule fires.
        let rs = vec![snap_with_volatile(
            6,
            0,
            "anthropic",
            "claude-sonnet-4",
            &[0, 10],
            &[8],
            12,
        )];
        let findings = evaluate_all(&rs);
        let f = findings
            .iter()
            .find(|f| f.rule_id == "volatile_in_cached_prefix")
            .expect("rule must fire when volatile < cc");
        assert_eq!(f.severity, Severity::Critical);
        assert!(
            f.narrative.contains("cache_control"),
            "got: {}",
            f.narrative
        );
    }

    #[test]
    fn volatile_rule_silent_on_anthropic_when_volatile_after_last_cc() {
        // Volatile at msg[11] AFTER the last cc at msg[10] → healthy.
        let rs = vec![snap_with_volatile(
            6,
            0,
            "anthropic",
            "claude-sonnet-4",
            &[0, 10],
            &[11],
            12,
        )];
        assert!(!evaluate_all(&rs)
            .iter()
            .any(|f| f.rule_id == "volatile_in_cached_prefix"),);
    }

    #[test]
    fn volatile_rule_fires_on_openai_when_volatile_mid_history() {
        // OpenAI TailSuffix: volatile must be on the LAST message
        // (msg[count-1]). msg[5] of 8 is mid-history → fires.
        let rs = vec![snap_with_volatile(
            2,
            0,
            "openai",
            "gpt-4o",
            /* msg_cc */ &[],
            /* volatile */ &[5],
            /* count */ 8,
        )];
        let findings = evaluate_all(&rs);
        let f = findings
            .iter()
            .find(|f| f.rule_id == "volatile_in_cached_prefix")
            .expect("rule must fire when volatile < tail for OpenAI");
        assert_eq!(f.severity, Severity::Critical);
        assert!(
            f.narrative.contains("auto-prefix"),
            "narrative explains OpenAI mechanism: {}",
            f.narrative,
        );
    }

    #[test]
    fn volatile_rule_silent_on_openai_when_volatile_on_tail() {
        let rs = vec![snap_with_volatile(2, 0, "openai", "gpt-4o", &[], &[7], 8)];
        assert!(!evaluate_all(&rs)
            .iter()
            .any(|f| f.rule_id == "volatile_in_cached_prefix"),);
    }

    #[test]
    fn volatile_rule_silent_when_no_volatile_content_tracked() {
        // Empty volatile_msg_indices → nothing to evaluate.
        let rs = vec![snap(6, 2, "openai", 1000, 0, &[], 21, None)];
        assert!(!evaluate_all(&rs)
            .iter()
            .any(|f| f.rule_id == "volatile_in_cached_prefix"),);
    }

    /// Regression from session bc5764b6 — the system message renders
    /// both (a) a cache_control-marked block and (b) a Self-Awareness
    /// block placed AFTER it in the same `content` array. Message-level
    /// cc index is [0] and volatile index is [0] (same msg). Our
    /// simple index-based check would fire, but the runtime has
    /// placed the volatile block AFTER the cc within the message, so
    /// block-level layout is safe. Rule trusts the runtime for system
    /// messages and stays silent.
    #[test]
    fn volatile_rule_trusts_system_block_layout() {
        // Single-message system with both cc and volatile at msg[0].
        // Plus a trailing user msg at idx 1 so message_count > 0.
        let rs = vec![snap_with_volatile_and_roles(
            5,
            0,
            "bedrock",
            "us.anthropic.claude-sonnet-4-6",
            /* msg_cc */ &[0],
            /* volatile */ &[0],
            /* count */ 2,
            &["system", "user"],
        )];
        let findings = evaluate_all(&rs);
        assert!(
            !findings
                .iter()
                .any(|f| f.rule_id == "volatile_in_cached_prefix"),
            "rule must trust runtime block-level layout inside a system \
             message. got findings={findings:?}",
        );
    }

    /// The negation: if volatile appears at a USER (mid-history)
    /// position, with cc markers at/after it, the rule fires as before.
    /// This is the bug we're actually guarding against.
    #[test]
    fn volatile_rule_still_fires_on_user_mid_history() {
        let rs = vec![snap_with_volatile_and_roles(
            5,
            0,
            "bedrock",
            "us.anthropic.claude-sonnet-4-6",
            /* msg_cc */ &[0, 10],
            /* volatile at user msg[8] */ &[8],
            /* count */ 11,
            &[
                "system",
                "user",
                "assistant",
                "user",
                "assistant",
                "user",
                "assistant",
                "user",
                "user",
                "assistant",
                "user",
            ],
        )];
        let findings = evaluate_all(&rs);
        assert!(
            findings
                .iter()
                .any(|f| f.rule_id == "volatile_in_cached_prefix"),
            "rule must still fire when volatile sits in a user/assistant \
             msg mid-history with cc markers at/after. got findings={findings:?}",
        );
    }

    #[test]
    fn volatile_rule_silent_on_unknown_provider() {
        // Unknown provider → VolatilePlacement::Free → rule doesn't apply.
        let rs = vec![snap_with_volatile(
            1,
            3,
            "some-new-vendor",
            "model-xyz",
            &[],
            &[5],
            10,
        )];
        assert!(!evaluate_all(&rs)
            .iter()
            .any(|f| f.rule_id == "volatile_in_cached_prefix"),);
    }

    // ── content-pattern detection (parser layer) ───────────────────────

    #[test]
    fn contains_volatile_pattern_catches_known_markers() {
        // Each pattern needs its co-occurring structural sibling.
        assert!(contains_volatile_pattern(
            "## Self-Awareness\nTurn: 5 | Tokens: 1234/80000"
        ));
        assert!(contains_volatile_pattern(
            "<system-reminder>\n[session-memory:v1]\ngoal: foo"
        ));
    }

    #[test]
    fn contains_volatile_pattern_ignores_unrelated_text() {
        assert!(!contains_volatile_pattern("Hi, what's up?"));
        assert!(!contains_volatile_pattern("```\nlet x = 1;\n```"));
        assert!(!contains_volatile_pattern(
            "I'm using self-awareness techniques"
        ));
    }

    /// Regression from session bc5764b6 — a `tool` message carrying a
    /// `git show` output of the commit that introduced
    /// `## Self-Awareness` handling must NOT be flagged as volatile
    /// injection. The rule's previous substring match fired on any
    /// occurrence of `## Self-Awareness`, misattributing commit
    /// bodies as runtime-injected volatile content.
    #[test]
    fn contains_volatile_pattern_ignores_commit_body_mentioning_marker() {
        let commit_body = "commit d2d6f96acc5018648373db9e8d28de4e521bc884\n\
                           Author: XuPeng-SH <xupeng@matrixorigin.io>\n\
                           Date:   Thu May 8 16:42:12 2026 +0800\n\n\
                           fix(cache): suppress volatile on strict-history bridge path\n\n\
                           The bridge's /chat/turn path was NOT routed through the new\n\
                           CacheCapability API. MiniMax requests kept injecting\n\
                           `## Self-Awareness` every round.";
        assert!(
            !contains_volatile_pattern(commit_body),
            "commit body mentioning '## Self-Awareness' without the live \
             `Turn: N | Tokens: M/K` co-occurrence must not be flagged",
        );
    }

    /// Complement: a tool_result that quotes `[session-memory:v1]` in
    /// an explanatory context (no `goal:` sibling) also mustn't fire.
    #[test]
    fn contains_volatile_pattern_ignores_session_memory_mention_without_goal() {
        let doc = "The `[session-memory:v1]` header is the start of the \
                   session-memory manifest. See module docs for layout.";
        assert!(!contains_volatile_pattern(doc));
    }

    #[test]
    fn snapshot_from_capture_detects_volatile_in_user_preamble() {
        // Mimic the 986a553e msg[7] shape.
        let v = serde_json::json!({
            "turn": 4,
            "round": 1,
            "provider": "openai",
            "model": "MiniMax-M2.7",
            "request": {
                "messages": [
                    {"role": "user", "content": "hi"},
                    {"role": "assistant", "content": "ok"},
                    {"role": "user", "content": "<system-reminder>\n\n\n## Self-Awareness\nTurn: 4 | Tokens: 13433/80000"}
                ],
                "tools": []
            },
            "response": {"usage": {"cached_input_tokens": 0}}
        });
        let snap = snapshot_from_capture_json(&v);
        assert_eq!(snap.volatile_msg_indices, vec![2]);
        assert_eq!(snap.message_count, 3);
    }

    #[test]
    fn load_session_captures_picks_up_both_filename_styles() {
        // Production files look like `llm_capture_t3_r0_bridge_inprocess_success_12345.json`,
        // scrubbed fixture files look like `llm_capture_t3_r0.json` OR even `t3_r0.json`.
        // The loader must recognize the production style at minimum.
        let tmp = std::env::temp_dir().join(format!("astra-cache-test-{}", std::process::id(),));
        std::fs::create_dir_all(&tmp).unwrap();
        // Clean up any prior run in this pid (serial_test isn't wired here).
        for e in std::fs::read_dir(&tmp).unwrap().flatten() {
            let _ = std::fs::remove_file(e.path());
        }
        let prod_style = tmp.join("llm_capture_t1_r0_bridge_inprocess_success_42.json");
        let scrubbed_style = tmp.join("llm_capture_t2_r5_other.json");
        let unrelated = tmp.join("not_a_capture.json");
        for (p, turn, round) in [(&prod_style, 1, 0), (&scrubbed_style, 2, 5)] {
            let body = serde_json::json!({
                "turn": turn,
                "round": round,
                "provider": "anthropic",
                "response": {"usage": {"cached_input_tokens": 100}},
            });
            std::fs::write(p, body.to_string()).unwrap();
        }
        std::fs::write(&unrelated, "{}").unwrap();

        let rows = load_session_captures(&tmp).unwrap();
        let mut positions: Vec<(u32, u32)> = rows.iter().map(|r| (r.turn, r.round)).collect();
        positions.sort();
        assert_eq!(
            positions,
            vec![(1, 0), (2, 5)],
            "loader must pick up both capture styles and sort by (turn, round)",
        );

        // cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
