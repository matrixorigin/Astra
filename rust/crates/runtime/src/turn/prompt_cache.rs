//! Prompt caching utilities for LLM system messages.
//!
//! Provides provider-aware system message construction with cache_control annotations
//! for Anthropic and stable-prefix splitting for OpenAI. Used by both the bridge proxy
//! and `ServerAgenticLoopHost`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};

use crate::prompts;
use astra_turn_core::context_serializer::serialize_prompt_sections;
use astra_turn_core::pipeline_config::ProviderCachePolicy;

const DEFAULT_CACHE_EDIT_PIN_KEY: &str = "__default__";
const MAX_PINNED_CACHE_EDIT_SESSIONS: usize = 1024;
const MAX_PINNED_CACHE_EDITS_PER_SESSION: usize = 256;

// ── PromptCacheConfig ────────────────────────────────────────────────────────

/// Configuration for provider-specific prompt caching.
pub struct PromptCacheConfig {
    /// Whether cache_control annotations are enabled for Anthropic.
    pub cache_enabled: bool,
    /// Whether the model should use Anthropic-style internal cache markers.
    ///
    /// This includes direct Anthropic models plus Bedrock-hosted Claude models,
    /// which reuse the same stable-prefix strategy and are translated to
    /// Bedrock-native `cachePoint` blocks at request-build time.
    pub is_anthropic: bool,
}

impl PromptCacheConfig {
    /// Latch config from environment and provider info. Call once at session start.
    pub fn latch(provider: &str, model_name: &str) -> Self {
        let cache_enabled = !std::env::var("ASTRA_TEST_PROMPT_CACHE_DISABLED")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        let provider_strategy =
            astra_turn_core::microcompact::ProviderCacheStrategy::from_provider_and_model(
                Some(provider),
                Some(model_name),
            );
        let is_anthropic = provider_strategy.prompt_cache_protocol
            == astra_turn_core::microcompact::PromptCacheProtocol::AnthropicCacheControl;
        Self {
            cache_enabled,
            is_anthropic,
        }
    }

    /// Convenience: should we emit cache_control annotations?
    pub fn should_annotate(&self) -> bool {
        self.cache_enabled && self.is_anthropic
    }
}

impl Default for PromptCacheConfig {
    fn default() -> Self {
        Self {
            cache_enabled: true,
            is_anthropic: false,
        }
    }
}

// ── Section Cache ────────────────────────────────────────────────────────────
// Two-level cache for static/dynamic prompt boundary:
// - Global+Session sections are cached by (tool_names, task_type, confidence) — stable within a session
// - Per-turn profile_desc is NOT cached (changes every turn with skills/memory/environment)

/// Cached prompt sections split by cache scope.
struct CachedSections {
    /// Concatenated text of Global+Session sections (for non-Anthropic providers).
    text: String,
    /// Individual stable sections with scope metadata (for Anthropic cache_control).
    sections: Vec<prompts::PromptSection>,
    /// Dynamic sections (CacheScope::None) from the prompt builder — output style, etc.
    dynamic_sections: Vec<prompts::PromptSection>,
}

fn section_cache() -> &'static Mutex<HashMap<u64, CachedSections>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, CachedSections>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pinned_cache_edits() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static PINS: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    PINS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn section_cache_key(
    tool_names: &[&str],
    task_type: Option<&str>,
    confidence: f64,
) -> u64 {
    section_cache_key_with_customization(tool_names, task_type, confidence, 0, 0)
}

fn section_cache_key_with_customization(
    tool_names: &[&str],
    task_type: Option<&str>,
    confidence: f64,
    overrides_fingerprint: u64,
    output_style_fingerprint: u64,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for name in tool_names {
        name.hash(&mut hasher);
    }
    task_type.unwrap_or("none").hash(&mut hasher);
    let bucket = if confidence < 0.3 { "low" } else { "normal" };
    bucket.hash(&mut hasher);
    overrides_fingerprint.hash(&mut hasher);
    output_style_fingerprint.hash(&mut hasher);
    hasher.finish()
}

fn output_style_fingerprint(
    output_style: Option<&astra_text_utils::output_style::OutputStyle>,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Some(style) = output_style {
        style.name.hash(&mut hasher);
        style.prompt.hash(&mut hasher);
    }
    hasher.finish()
}

fn prompt_overrides_fingerprint(overrides: &prompts::PromptOverrides) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut entries: Vec<_> = overrides.iter().collect();
    entries.sort_by_key(|(left, _)| *left);
    for (name, text) in entries {
        name.hash(&mut hasher);
        text.hash(&mut hasher);
    }
    hasher.finish()
}

// ── build_system_message ─────────────────────────────────────────────────────

/// Build the system message(s) for the LLM API.
///
/// Returns `(primary, dynamic, sections)`:
/// - **Anthropic**: `primary` is a multi-block content array with `cache_control` on stable
///   sections and dynamic profile appended without cache markers. `dynamic` is `None`.
/// - **OpenAI / other**: `primary` contains only the **stable** text (cacheable prefix).
///   `dynamic` holds a second system message with the per-turn profile/hints, or `None`
///   if there is nothing dynamic. This split enables OpenAI's automatic prefix caching:
///   the stable message stays identical across turns so the provider can reuse the KV cache.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn build_system_message(
    tool_names: &[&str],
    profile_desc: &str,
    confidence: f64,
    task_type: Option<&str>,
    cache_cfg: &PromptCacheConfig,
) -> (Value, Option<Value>, Vec<prompts::PromptSection>) {
    let dynamic_sections = if profile_desc.is_empty() {
        Vec::new()
    } else {
        vec![prompts::PromptSection::dynamic(
            profile_desc.to_string(),
            prompts::PromptTokenBucket::Environment,
        )]
    };
    build_system_message_with_dynamic_sections(
        tool_names,
        &dynamic_sections,
        confidence,
        task_type,
        cache_cfg,
    )
}

pub(crate) fn build_system_message_with_dynamic_sections(
    tool_names: &[&str],
    dynamic_sections: &[prompts::PromptSection],
    confidence: f64,
    task_type: Option<&str>,
    cache_cfg: &PromptCacheConfig,
) -> (Value, Option<Value>, Vec<prompts::PromptSection>) {
    let output_style = astra_text_utils::output_style::current_output_style();
    let overrides = prompts::load_overrides(&prompts::default_overrides_dir());
    let key = section_cache_key_with_customization(
        tool_names,
        task_type,
        confidence,
        prompt_overrides_fingerprint(&overrides),
        output_style_fingerprint(output_style),
    );

    // Try cache for the stable (Global + Session) + dynamic (None-scoped) sections
    let cached = if let Ok(cache) = section_cache().lock() {
        cache.get(&key).map(|c| {
            (
                c.text.clone(),
                c.sections.clone(),
                c.dynamic_sections.clone(),
            )
        })
    } else {
        None
    };

    let (stable_text, sections, runtime_dynamic_sections) = cached.unwrap_or_else(|| {
        // Build all sections (profile_desc is "" for cache — we'll append it separately)
        let mut all = prompts::build_system_prompt_sections_with_style(
            tool_names,
            "",
            confidence,
            task_type,
            output_style,
        );
        prompts::apply_overrides(&mut all, &overrides);
        let mut stable = Vec::new();
        let mut dynamic = Vec::new();
        for s in all {
            if s.scope != prompts::CacheScope::None {
                stable.push(s);
            } else if !s.text.is_empty() {
                dynamic.push(s);
            }
        }
        let text = stable
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("");

        if let Ok(mut cache) = section_cache().lock() {
            if cache.len() > 32 {
                cache.clear();
            }
            cache.insert(
                key,
                CachedSections {
                    text: text.clone(),
                    sections: stable.clone(),
                    dynamic_sections: dynamic.clone(),
                },
            );
        }
        (text, stable, dynamic)
    });

    let is_anthropic = cache_cfg.is_anthropic;
    let mut all_dynamic_sections =
        Vec::with_capacity(runtime_dynamic_sections.len() + dynamic_sections.len());
    all_dynamic_sections.extend(runtime_dynamic_sections);
    all_dynamic_sections.extend(dynamic_sections.iter().cloned());
    let dynamic_text = prompts::sections_to_string(&all_dynamic_sections);

    // Build complete sections list (stable + dynamic) for trace.
    let append_dynamic = |mut secs: Vec<prompts::PromptSection>| -> Vec<prompts::PromptSection> {
        secs.extend(all_dynamic_sections.iter().cloned());
        secs
    };

    if is_anthropic {
        let policy = provider_policy_for_prompt_cache(cache_cfg);
        let serialized = serialize_prompt_sections(&sections, &policy);
        let mut blocks: Vec<Value> =
            Vec::with_capacity(serialized.system_blocks.len() + all_dynamic_sections.len());
        for section in serialized.system_blocks {
            let mut block = json!({
                "type": "text",
                "text": section.text,
            });
            if let Some(cc) = section.cache_control {
                block["cache_control"] = cc;
            }
            blocks.push(block);
        }
        // Dynamic section (profile + per-turn hints) — no cache_control
        for section in &all_dynamic_sections {
            blocks.push(json!({
                "type": "text",
                "text": section.text,
            }));
        }
        // Anthropic: everything in one message (cache_control breakpoints handle caching)
        (
            json!({
                "role": "system",
                "content": blocks,
            }),
            None,
            append_dynamic(sections),
        )
    } else {
        // OpenAI-compatible: split stable / dynamic into separate system messages
        // so the stable prefix is identical across turns and the provider can reuse
        // its automatic KV cache.
        let primary = json!({
            "role": "system",
            "content": stable_text,
        });
        let dynamic = if dynamic_text.is_empty() {
            None
        } else {
            Some(json!({
                "role": "system",
                "content": dynamic_text,
            }))
        };
        (primary, dynamic, append_dynamic(sections))
    }
}

fn provider_policy_for_prompt_cache(cache_cfg: &PromptCacheConfig) -> ProviderCachePolicy {
    if cache_cfg.cache_enabled {
        ProviderCachePolicy::anthropic()
    } else {
        ProviderCachePolicy {
            max_markers: 0,
            ..ProviderCachePolicy::anthropic()
        }
    }
}

/// Add `cache_control` to a tool schema for Anthropic caching.
///
/// Anthropic allows up to 4 cache_control breakpoints per request. Our allocation:
/// - System prompt: up to 2 breakpoints (global scope + session scope)
/// - Tools: 1 breakpoint at the end of the STATIC (pinned) prefix — keeps the
///   static lib cached even when dynamic tools churn per turn
/// - Messages: 1 breakpoint on the last message
///
/// `pinned_names` identifies tools that are guaranteed present every turn
/// (static lib). The marker goes on the last pinned tool, so subsequent
/// dynamic tools sitting after it don't invalidate the cached prefix. If no
/// pinned tools are present (e.g. caller opted into full-dynamic), falls
/// back to the last tool.
pub(crate) fn annotate_tool_schemas_for_caching(
    tools: &mut [Value],
    cache_cfg: &PromptCacheConfig,
) {
    annotate_tool_schemas_for_caching_with_pinned(tools, cache_cfg, &default_pinned_tool_names());
}

/// Variant of [`annotate_tool_schemas_for_caching`] that takes an explicit
/// pinned set — used by tests and callers that need to override the default.
pub(crate) fn annotate_tool_schemas_for_caching_with_pinned(
    tools: &mut [Value],
    cache_cfg: &PromptCacheConfig,
    pinned_names: &std::collections::HashSet<String>,
) {
    if !cache_cfg.should_annotate() || tools.is_empty() {
        return;
    }
    let marker_idx = last_pinned_tool_index(tools, pinned_names).unwrap_or(tools.len() - 1);
    tools[marker_idx]["cache_control"] = json!({"type": "ephemeral"});
}

fn last_pinned_tool_index(
    tools: &[Value],
    pinned_names: &std::collections::HashSet<String>,
) -> Option<usize> {
    if pinned_names.is_empty() {
        return None;
    }
    tools.iter().enumerate().rev().find_map(|(idx, t)| {
        let name = t
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)?;
        if pinned_names.contains(name) {
            Some(idx)
        } else {
            None
        }
    })
}

/// Default pinned tool names — the static-lib set that should appear in every
/// turn of every session. Derived from `TOOL_CATALOG` + schemas that are
/// auto-pinned via `ToolRegistry::upsert_schema` (skill, spawn_agent,
/// get_agent_result, send_message, etc.).
///
/// Returning a fresh `HashSet` per call keeps the API safe across threads
/// without a static — the set is small (~15 entries) so this is cheap.
pub(crate) fn default_pinned_tool_names() -> std::collections::HashSet<String> {
    use astra_turn_core::tool_registry_meta::TOOL_CATALOG;
    let mut out: std::collections::HashSet<String> = TOOL_CATALOG
        .iter()
        .filter(|t| t.pinned)
        .map(|t| t.name.to_string())
        .collect();
    // Auto-injected via ToolRegistry::upsert_schema (see sse_loop mod.rs +
    // agentic_loop_lifecycle.rs). These aren't in TOOL_CATALOG but are
    // structurally part of the static lib — include them so the cache
    // marker sits at the real static-prefix boundary.
    for name in ["skill", "spawn_agent", "get_agent_result", "send_message"] {
        out.insert(name.to_string());
    }
    out
}

/// Add a cache breakpoint on the last conversation message for Anthropic.
/// This enables turn-to-turn KV cache reuse for the conversation prefix.
pub(crate) fn add_message_cache_breakpoint(messages: &mut [Value], cache_cfg: &PromptCacheConfig) {
    if !cache_cfg.should_annotate() || messages.is_empty() {
        return;
    }
    // Find the last non-system message and add cache_control to it
    if let Some(last) = messages.iter_mut().rev().find(|m| {
        m.get("role")
            .and_then(Value::as_str)
            .is_some_and(|r| r != "system")
    }) {
        // If content is a string, convert to array format for cache_control
        if last.get("content").is_some_and(Value::is_string) {
            let text = last["content"].as_str().unwrap_or_default().to_string();
            last["content"] = json!([{
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral"},
            }]);
        } else if let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut) {
            // Content is already an array — add cache_control to last element
            if let Some(last_block) = arr.last_mut() {
                last_block["cache_control"] = json!({"type": "ephemeral"});
            }
        }
    }
}

/// Add Anthropic protocol-level cache metadata for cached micro-compaction.
///
/// This mirrors Claude Code's API-layer approach: request messages are annotated
/// with `cache_reference` / `cache_edits` while the persisted local conversation
/// remains unchanged. Existing `cache_control` placement is preserved at exactly
/// one message-level breakpoint.
pub(crate) fn apply_anthropic_cache_metadata(
    messages: &mut [Value],
    cache_cfg: &PromptCacheConfig,
    session_id: &str,
) {
    if !cache_cfg.should_annotate() || messages.is_empty() {
        return;
    }

    add_message_cache_breakpoint(messages, cache_cfg);

    let new_deletes = collect_cleared_tool_result_refs(messages);
    let pinned_deletes = pin_and_merge_cache_edits(session_id, &new_deletes);
    insert_cache_edits_block(messages, &pinned_deletes);
    add_tool_result_cache_references(messages);
}

fn collect_cleared_tool_result_refs(messages: &[Value]) -> Vec<String> {
    let mut refs = Vec::new();
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let Some(tool_call_id) = msg.get("tool_call_id").and_then(Value::as_str) else {
            continue;
        };
        let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
        if content == crate::turn::cloud::analytics::MICRO_COMPACT_STUB
            || astra_turn_core::microcompact::is_cleared_content(content)
        {
            refs.push(tool_call_id.to_string());
        }
    }
    refs.sort();
    refs.dedup();
    refs
}

fn pin_and_merge_cache_edits(session_id: &str, new_deletes: &[String]) -> Vec<String> {
    let key = if session_id.is_empty() {
        DEFAULT_CACHE_EDIT_PIN_KEY
    } else {
        session_id
    };
    let Ok(mut pins) = pinned_cache_edits().lock() else {
        return new_deletes.to_vec();
    };
    if !pins.contains_key(key)
        && pins.len() >= MAX_PINNED_CACHE_EDIT_SESSIONS
        && let Some(evict_key) = pins
            .keys()
            .find(|existing| existing.as_str() != key)
            .cloned()
    {
        pins.remove(&evict_key);
    }
    let entry = pins.entry(key.to_string()).or_default();
    for delete_ref in new_deletes {
        if !entry.contains(delete_ref) {
            entry.push(delete_ref.clone());
        }
    }
    entry.sort();
    entry.dedup();
    if entry.len() > MAX_PINNED_CACHE_EDITS_PER_SESSION {
        let excess = entry.len() - MAX_PINNED_CACHE_EDITS_PER_SESSION;
        entry.drain(0..excess);
    }
    entry.clone()
}

fn insert_cache_edits_block(messages: &mut [Value], delete_refs: &[String]) {
    if delete_refs.is_empty() {
        return;
    }
    let Some(last_user) = messages
        .iter_mut()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
    else {
        return;
    };
    ensure_content_array(last_user);
    let Some(content) = last_user.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    content.retain(|block| block.get("type").and_then(Value::as_str) != Some("cache_edits"));
    content.push(json!({
        "type": "cache_edits",
        "edits": delete_refs
            .iter()
            .map(|cache_reference| json!({
                "type": "delete",
                "cache_reference": cache_reference,
            }))
            .collect::<Vec<_>>(),
    }));
}

fn add_tool_result_cache_references(messages: &mut [Value]) {
    let Some(last_cc_idx) = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, msg)| message_has_cache_control(msg))
        .map(|(idx, _)| idx)
    else {
        return;
    };

    for msg in messages.iter_mut().take(last_cc_idx) {
        if msg.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        if let Some(tool_call_id) = msg.get("tool_call_id").and_then(Value::as_str) {
            msg["cache_reference"] = Value::String(tool_call_id.to_string());
        }
    }
}

fn ensure_content_array(msg: &mut Value) {
    if msg.get("content").is_some_and(Value::is_array) {
        return;
    }
    let text = msg
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    msg["content"] = json!([{ "type": "text", "text": text }]);
}

fn message_has_cache_control(msg: &Value) -> bool {
    if msg.get("cache_control").is_some() {
        return true;
    }
    msg.get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block
                    .get("cache_control")
                    .map(|cc| !cc.is_null())
                    .unwrap_or(false)
            })
        })
}

#[cfg(test)]
fn clear_anthropic_cache_edit_pins_for_tests(session_id: &str) {
    let key = if session_id.is_empty() {
        DEFAULT_CACHE_EDIT_PIN_KEY
    } else {
        session_id
    };
    if let Ok(mut pins) = pinned_cache_edits().lock() {
        pins.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static CACHE_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Safe wrapper for `std::env::set_var` in single-threaded tests guarded by `CACHE_ENV_MUTEX`.
    fn set_test_env(key: &str, val: &str) {
        // SAFETY: all tests that mutate env vars hold CACHE_ENV_MUTEX and run with
        // `-- --test-threads=1` or the mutex serialises access within this module.
        unsafe { std::env::set_var(key, val) }
    }

    /// Safe wrapper for `std::env::remove_var` in single-threaded tests.
    fn remove_test_env(key: &str) {
        unsafe { std::env::remove_var(key) }
    }

    #[test]
    fn section_cache_key_varies_by_tools_and_task() {
        let key1 = section_cache_key(&["bash"], Some("implementation"), 0.8);
        let key2 = section_cache_key(&["bash", "read_file"], Some("implementation"), 0.8);
        let key3 = section_cache_key(&["bash"], Some("debugging"), 0.8);
        let key4 = section_cache_key(&["bash"], Some("implementation"), 0.2);
        assert_ne!(key1, key2, "different tools should differ");
        assert_ne!(key1, key3, "different task types should differ");
        assert_ne!(key1, key4, "different confidence buckets should differ");
    }

    #[test]
    fn section_cache_key_differs_for_different_tools() {
        let k1 = section_cache_key(&["read_file"], None, 1.0);
        let k2 = section_cache_key(&["bash"], None, 1.0);
        assert_ne!(k1, k2);
    }

    #[test]
    fn section_cache_key_low_confidence_bucketed() {
        let k_low = section_cache_key(&["bash"], None, 0.2);
        let k_normal = section_cache_key(&["bash"], None, 0.5);
        assert_ne!(k_low, k_normal);
        // Both in low bucket should match
        let k_low2 = section_cache_key(&["bash"], None, 0.1);
        assert_eq!(k_low, k_low2);
    }

    #[test]
    fn structured_prompt_includes_runtime_style_and_prompt_overrides() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        let home = tempfile::tempdir().expect("temp home");
        let prompts_dir = home.path().join(".astra").join("prompts");
        std::fs::create_dir_all(&prompts_dir).expect("prompts dir");
        std::fs::write(
            prompts_dir.join("core_rules.txt"),
            "\n## Core Rules Override\nOVERRIDE_SENTINEL\n",
        )
        .expect("override file");
        if let Ok(mut cache) = section_cache().lock() {
            cache.clear();
        }
        set_test_env("HOME", home.path().to_str().unwrap());
        set_test_env("ASTRA_OUTPUT_STYLE", "concise");

        let (msg, dynamic_msg, sections) = build_system_message(
            &["prompt_cache_test_tool"],
            "\n\n# Project Profile\ncwd: /tmp/prompt-cache-test",
            0.8,
            Some("prompt-cache-style-override"),
            &PromptCacheConfig {
                cache_enabled: true,
                is_anthropic: false,
            },
        );

        let stable = msg["content"].as_str().expect("stable system text");
        assert!(
            stable.contains("OVERRIDE_SENTINEL"),
            "structured stable prompt must include prompt overrides: {stable}"
        );
        let dynamic = dynamic_msg
            .as_ref()
            .and_then(|m| m["content"].as_str())
            .expect("dynamic system text");
        assert!(
            dynamic.contains("# Output Style: Concise"),
            "structured dynamic prompt must include active output style: {dynamic}"
        );
        assert!(
            sections
                .iter()
                .any(|section| section.text.contains("# Output Style: Concise")),
            "trace sections must include the same output style sent to the provider"
        );

        remove_test_env("ASTRA_OUTPUT_STYLE");
    }

    #[test]
    fn structured_prompt_cache_key_tracks_prompt_override_changes() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        let home = tempfile::tempdir().expect("temp home");
        let prompts_dir = home.path().join(".astra").join("prompts");
        std::fs::create_dir_all(&prompts_dir).expect("prompts dir");
        let override_path = prompts_dir.join("core_rules.txt");
        set_test_env("HOME", home.path().to_str().unwrap());
        remove_test_env("ASTRA_OUTPUT_STYLE");
        if let Ok(mut cache) = section_cache().lock() {
            cache.clear();
        }

        std::fs::write(&override_path, "\nFIRST_OVERRIDE_SENTINEL\n").expect("first override");
        let (first, _, _) = build_system_message(
            &["prompt_cache_override_reload_tool"],
            "",
            0.8,
            Some("prompt-cache-override-reload"),
            &PromptCacheConfig::default(),
        );
        assert!(
            first["content"]
                .as_str()
                .unwrap()
                .contains("FIRST_OVERRIDE_SENTINEL")
        );

        std::fs::write(&override_path, "\nSECOND_OVERRIDE_SENTINEL\n").expect("second override");
        let (second, _, _) = build_system_message(
            &["prompt_cache_override_reload_tool"],
            "",
            0.8,
            Some("prompt-cache-override-reload"),
            &PromptCacheConfig::default(),
        );
        let stable = second["content"].as_str().unwrap();
        assert!(
            stable.contains("SECOND_OVERRIDE_SENTINEL")
                && !stable.contains("FIRST_OVERRIDE_SENTINEL"),
            "stable prompt cache must invalidate when override files change: {stable}"
        );
    }

    #[test]
    fn structured_prompt_cache_key_tracks_output_style_changes() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        let home = tempfile::tempdir().expect("temp home");
        set_test_env("HOME", home.path().to_str().unwrap());
        if let Ok(mut cache) = section_cache().lock() {
            cache.clear();
        }

        set_test_env("ASTRA_OUTPUT_STYLE", "concise");
        let (_, first_dynamic, _) = build_system_message(
            &["prompt_cache_style_reload_tool"],
            "",
            0.8,
            Some("prompt-cache-style-reload"),
            &PromptCacheConfig::default(),
        );
        let first = first_dynamic
            .as_ref()
            .and_then(|m| m["content"].as_str())
            .expect("first dynamic prompt");
        assert!(first.contains("# Output Style: Concise"), "{first}");

        set_test_env("ASTRA_OUTPUT_STYLE", "verbose");
        let (_, second_dynamic, _) = build_system_message(
            &["prompt_cache_style_reload_tool"],
            "",
            0.8,
            Some("prompt-cache-style-reload"),
            &PromptCacheConfig::default(),
        );
        let second = second_dynamic
            .as_ref()
            .and_then(|m| m["content"].as_str())
            .expect("second dynamic prompt");
        assert!(
            second.contains("# Output Style: Verbose")
                && !second.contains("# Output Style: Concise"),
            "dynamic prompt cache must invalidate when output style changes: {second}"
        );

        remove_test_env("ASTRA_OUTPUT_STYLE");
    }

    #[test]
    fn prompt_cache_latch_prefers_provider_over_claude_named_model() {
        let openai_proxy = PromptCacheConfig::latch("openai", "claude-sonnet-4");
        assert!(!openai_proxy.is_anthropic);

        let anthropic_provider = PromptCacheConfig::latch("anthropic", "gpt-4o");
        assert!(anthropic_provider.is_anthropic);
    }

    #[test]
    fn build_system_message_anthropic_has_cache_control() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        remove_test_env("ASTRA_TEST_PROMPT_CACHE_DISABLED");

        let (msg, _, _) = build_system_message(
            &["bash", "read_file"],
            "profile text",
            0.8,
            Some("implementation"),
            &PromptCacheConfig {
                cache_enabled: true,
                is_anthropic: true,
            },
        );
        let content = msg.get("content").unwrap().as_array().unwrap();
        assert!(
            content.iter().any(|b| b.get("cache_control").is_some()),
            "Anthropic should have cache_control blocks"
        );
        let cache_controls: Vec<_> = content
            .iter()
            .filter_map(|block| block.get("cache_control"))
            .collect();
        assert!(
            cache_controls
                .iter()
                .all(|cc| **cc == json!({"type": "ephemeral"})),
            "cache markers should be simple ephemeral (Bedrock-compatible, no beta header): {cache_controls:?}"
        );
    }

    #[test]
    fn build_system_message_openai_has_string_content() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        let (msg, dyn_msg, _) = build_system_message(
            &["bash"],
            "profile",
            0.8,
            None,
            &PromptCacheConfig {
                cache_enabled: true,
                is_anthropic: false,
            },
        );
        assert!(
            msg.get("content").unwrap().is_string(),
            "OpenAI primary should be string"
        );
        assert!(
            dyn_msg.is_some(),
            "Dynamic message should exist for non-empty profile"
        );
    }

    #[test]
    fn build_system_message_cache_disabled_env() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        set_test_env("ASTRA_TEST_PROMPT_CACHE_DISABLED", "1");
        let (msg, _, _) = build_system_message(
            &["bash"],
            "profile",
            0.8,
            None,
            &PromptCacheConfig {
                cache_enabled: false,
                is_anthropic: true,
            },
        );
        let content = msg.get("content").unwrap().as_array().unwrap();
        assert!(
            content.iter().all(|b| b.get("cache_control").is_none()),
            "cache disabled should not annotate"
        );
        remove_test_env("ASTRA_TEST_PROMPT_CACHE_DISABLED");
    }

    #[test]
    fn build_system_message_returns_non_empty_sections() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        let (_, _, sections) = build_system_message(
            &["bash"],
            "profile",
            0.8,
            None,
            &PromptCacheConfig::default(),
        );
        assert!(!sections.is_empty(), "should return non-empty sections");
    }

    #[test]
    fn annotate_tool_schemas_for_caching_adds_cache_control() {
        // With unknown (non-pinned) names, fall back to the last tool — the
        // historical behavior. Covers custom-tool pipelines that don't go
        // through TOOL_CATALOG.
        let mut tools = vec![
            json!({"type": "function", "function": {"name": "a"}}),
            json!({"type": "function", "function": {"name": "b"}}),
        ];
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig {
                cache_enabled: true,
                is_anthropic: true,
            },
        );
        assert!(
            tools[0].get("cache_control").is_none(),
            "first tool should NOT have cache_control"
        );
        assert!(
            tools[1].get("cache_control").is_some(),
            "last tool should have cache_control (fallback — no pinned tools present)"
        );
    }

    /// Cache marker must sit at the end of the STATIC (pinned) prefix, not
    /// after dynamic tools. Otherwise churn in the dynamic segment invalidates
    /// the cached prefix every turn.
    #[test]
    fn annotate_tool_schemas_marks_end_of_pinned_prefix_not_last_tool() {
        let mut tools = vec![
            json!({"type": "function", "function": {"name": "bash"}}),         // pinned
            json!({"type": "function", "function": {"name": "read_file"}}),    // pinned
            json!({"type": "function", "function": {"name": "git_status"}}),   // pinned (new)
            json!({"type": "function", "function": {"name": "git_log"}}),      // dynamic
            json!({"type": "function", "function": {"name": "mo_branch"}}),    // dynamic
        ];
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig {
                cache_enabled: true,
                is_anthropic: true,
            },
        );

        // Marker on last pinned (git_status at idx 2).
        assert!(tools[0].get("cache_control").is_none());
        assert!(tools[1].get("cache_control").is_none());
        assert!(
            tools[2].get("cache_control").is_some(),
            "marker belongs on the last pinned tool (static-lib boundary)"
        );
        assert!(
            tools[3].get("cache_control").is_none(),
            "dynamic tool must NOT receive the marker — its churn would invalidate cache"
        );
        assert!(tools[4].get("cache_control").is_none());
    }

    /// When dynamic tools are interleaved (shouldn't happen in production but
    /// could via custom pipelines), the marker goes on the LAST pinned tool —
    /// guaranteeing the pinned prefix is fully cached.
    #[test]
    fn annotate_tool_schemas_handles_interleaved_tools() {
        let mut tools = vec![
            json!({"type": "function", "function": {"name": "bash"}}),       // pinned
            json!({"type": "function", "function": {"name": "lsp"}}),        // dynamic
            json!({"type": "function", "function": {"name": "memory_store"}}), // pinned
            json!({"type": "function", "function": {"name": "git_log"}}),   // dynamic
        ];
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig {
                cache_enabled: true,
                is_anthropic: true,
            },
        );
        assert!(tools[2].get("cache_control").is_some());
        assert!(tools[3].get("cache_control").is_none());
    }

    #[test]
    fn tool_schemas_empty_list_noop() {
        let mut tools: Vec<Value> = vec![];
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig {
                cache_enabled: true,
                is_anthropic: true,
            },
        );
        assert!(tools.is_empty());
    }

    #[test]
    fn message_breakpoint_skips_system_only() {
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let mut messages = vec![json!({"role": "system", "content": "system prompt"})];
        let original = messages.clone();
        add_message_cache_breakpoint(&mut messages, &cfg);
        assert_eq!(
            messages, original,
            "system-only messages should not be modified"
        );
    }

    #[test]
    fn message_breakpoint_empty_messages_noop() {
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let mut messages: Vec<Value> = vec![];
        add_message_cache_breakpoint(&mut messages, &cfg);
        assert!(messages.is_empty());
    }

    #[test]
    fn message_breakpoint_array_content_appends_to_last_block() {
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let mut messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": [{"type": "text", "text": "hi"}]}),
        ];
        add_message_cache_breakpoint(&mut messages, &cfg);
        let content = messages[1].get("content").unwrap().as_array().unwrap();
        assert!(content[0].get("cache_control").is_some());
    }

    #[test]
    fn add_message_cache_breakpoint_noop_for_openai() {
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: false,
        };
        let mut messages = vec![json!({"role": "user", "content": "hello"})];
        let original = messages.clone();
        add_message_cache_breakpoint(&mut messages, &cfg);
        assert_eq!(messages, original, "OpenAI should not be annotated");
    }

    #[test]
    fn latch_enables_anthropic_style_cache_for_bedrock_claude() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        remove_test_env("ASTRA_TEST_PROMPT_CACHE_DISABLED");
        let cfg = PromptCacheConfig::latch("bedrock", "anthropic.claude-sonnet-4-20250514-v1:0");
        assert!(cfg.cache_enabled);
        assert!(cfg.is_anthropic);
    }

    #[test]
    fn latch_keeps_non_claude_bedrock_on_openai_style_cache() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        remove_test_env("ASTRA_TEST_PROMPT_CACHE_DISABLED");
        let cfg = PromptCacheConfig::latch("bedrock", "us.amazon.nova-micro-v1:0");
        assert!(cfg.cache_enabled);
        assert!(!cfg.is_anthropic);
    }

    #[test]
    fn anthropic_cache_metadata_inserts_deduped_cache_edits_and_references() {
        clear_anthropic_cache_edit_pins_for_tests("session-a");
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let mut messages = vec![
            json!({"role": "user", "content": "first"}),
            json!({
                "role": "tool",
                "tool_call_id": "tool-1",
                "content": "full cached tool output"
            }),
            json!({
                "role": "tool",
                "tool_call_id": "tool-2",
                "content": crate::turn::cloud::analytics::MICRO_COMPACT_STUB
            }),
            json!({"role": "user", "content": "continue"}),
        ];

        let original_tool_content = messages[1]["content"].clone();
        apply_anthropic_cache_metadata(&mut messages, &cfg, "session-a");

        assert_eq!(
            messages[1]["content"], original_tool_content,
            "request annotation must not rewrite full local tool content"
        );
        assert_eq!(messages[1]["cache_reference"], "tool-1");
        assert_eq!(messages[2]["cache_reference"], "tool-2");

        let user_blocks = messages[3]["content"]
            .as_array()
            .expect("user content blocks");
        let cache_edits = user_blocks
            .iter()
            .find(|block| block.get("type").and_then(Value::as_str) == Some("cache_edits"))
            .expect("cache_edits block");
        assert_eq!(
            cache_edits["edits"],
            json!([{ "type": "delete", "cache_reference": "tool-2" }])
        );
        let cache_control_blocks = user_blocks
            .iter()
            .filter(|block| block.get("cache_control").is_some())
            .count();
        assert_eq!(
            cache_control_blocks, 1,
            "there must be exactly one message-level cache_control marker"
        );
    }

    #[test]
    fn anthropic_cache_edits_are_pinned_across_requests_for_session() {
        clear_anthropic_cache_edit_pins_for_tests("session-pinned");
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let mut first = vec![
            json!({
                "role": "tool",
                "tool_call_id": "tool-1",
                "content": crate::turn::cloud::analytics::MICRO_COMPACT_STUB
            }),
            json!({"role": "user", "content": "continue"}),
        ];
        apply_anthropic_cache_metadata(&mut first, &cfg, "session-pinned");

        let mut second = vec![json!({"role": "user", "content": "later"})];
        apply_anthropic_cache_metadata(&mut second, &cfg, "session-pinned");

        let blocks = second[0]["content"].as_array().expect("content blocks");
        let cache_edits = blocks
            .iter()
            .find(|block| block.get("type").and_then(Value::as_str) == Some("cache_edits"))
            .expect("pinned cache_edits block");
        assert_eq!(
            cache_edits["edits"],
            json!([{ "type": "delete", "cache_reference": "tool-1" }])
        );
    }

    #[test]
    fn anthropic_cache_metadata_noop_for_openai() {
        clear_anthropic_cache_edit_pins_for_tests("session-openai");
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: false,
        };
        let mut messages = vec![
            json!({
                "role": "tool",
                "tool_call_id": "tool-1",
                "content": crate::turn::cloud::analytics::MICRO_COMPACT_STUB
            }),
            json!({"role": "user", "content": "continue"}),
        ];
        let original = messages.clone();
        apply_anthropic_cache_metadata(&mut messages, &cfg, "session-openai");
        assert_eq!(messages, original);
    }
}

// ── Cache-stability regression tests ────────────────────────────────────────
//
// These guard the "static-lib + dynamic-lib" invariant that makes prompt cache
// hits possible:
//   1. pinned tools appear first, byte-identical across calls;
//   2. the cache marker sits at the end of the pinned prefix;
//   3. any churn in the dynamic suffix leaves the prefix bytes intact.
//
// If a future refactor re-sorts the combined tool list, introduces HashMap
// iter into pinned assembly, or moves the marker back to "last tool", one of
// these tests will fail before the live cache hit rate silently collapses.
#[cfg(test)]
mod cache_stability_regression {
    use super::*;
    use crate::turn::llm_client::build_provider_request_body;
    use astra_turn_core::thinking_config::ThinkingConfig;
    use serde_json::json;
    use std::collections::HashSet;

    /// Synthetic schema factory — deterministic bytes keyed only by `name`.
    fn schema(name: &str) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": format!("Test fixture for {name}"),
                "parameters": {"type": "object", "properties": {}}
            }
        })
    }

    /// The tool list for these tests intentionally uses names that overlap
    /// `default_pinned_tool_names()` so the marker-placement logic exercises
    /// the real pinned set, not a local override.
    fn pinned_prefix_fixture() -> Vec<Value> {
        vec![
            schema("bash"),
            schema("read_file"),
            schema("write_file"),
            schema("str_replace"),
            schema("list_dir"),
            schema("grep"),
            schema("glob"),
            schema("git_status"),
            schema("git_diff"),
            schema("memory_store"),
            schema("memory_retrieve"),
            schema("memory_purge"),
            schema("memory_correct"),
            schema("skill"),
        ]
    }

    fn cfg_anthropic() -> PromptCacheConfig {
        PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        }
    }

    /// Core invariant: adding, removing, or reordering tools AFTER the pinned
    /// prefix must leave the pinned prefix bytes completely unchanged and keep
    /// the cache marker on the same pinned tool.
    #[test]
    fn pinned_prefix_bytes_survive_dynamic_churn() {
        // Turn A: 3 dynamic tools in one order.
        let mut a = pinned_prefix_fixture();
        a.extend([schema("git_log"), schema("mo_branch"), schema("github_list_prs")]);
        annotate_tool_schemas_for_caching(&mut a, &cfg_anthropic());

        // Turn B: different dynamic tools in different order, different count.
        let mut b = pinned_prefix_fixture();
        b.extend([
            schema("git_show"),
            schema("github_get_pr"),
            schema("web_fetch"),
            schema("mo_query"),
        ]);
        annotate_tool_schemas_for_caching(&mut b, &cfg_anthropic());

        // 1. Pinned prefix (positions 0..=13) is byte-identical.
        let pinned_count = pinned_prefix_fixture().len();
        for i in 0..pinned_count - 1 {
            // All but the last pinned — the marker is on the last one, so
            // compare it structurally below.
            assert_eq!(
                a[i], b[i],
                "pinned tool at idx {i} must be byte-identical across turns"
            );
        }

        // 2. Marker lands on the same tool name (bytes may differ from the raw
        //    fixture because of the added `cache_control`, but the *host* tool
        //    is the same one).
        let marker_a_name = a[pinned_count - 1]
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str);
        let marker_b_name = b[pinned_count - 1]
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str);
        assert_eq!(
            marker_a_name, marker_b_name,
            "cache marker must host on the same pinned tool across turns"
        );
        assert!(
            a[pinned_count - 1].get("cache_control").is_some(),
            "last pinned tool on turn A must carry cache_control"
        );
        assert!(
            b[pinned_count - 1].get("cache_control").is_some(),
            "last pinned tool on turn B must carry cache_control"
        );

        // 3. No dynamic tool carries cache_control — if one did, a churn would
        //    wipe its cache every turn.
        for (i, tool) in a.iter().enumerate().skip(pinned_count) {
            assert!(
                tool.get("cache_control").is_none(),
                "dynamic tool at idx {i} on turn A must NOT carry cache_control"
            );
        }
        for (i, tool) in b.iter().enumerate().skip(pinned_count) {
            assert!(
                tool.get("cache_control").is_none(),
                "dynamic tool at idx {i} on turn B must NOT carry cache_control"
            );
        }
    }

    /// The marker always lands on the LAST pinned tool — even if the pinned
    /// count shrinks or dynamic tools are interleaved by a buggy caller.
    #[test]
    fn marker_position_equals_last_pinned_index() {
        let mut tools = vec![
            schema("bash"),        // pinned
            schema("git_log"),     // dynamic (interleaved — shouldn't happen in production)
            schema("read_file"),   // pinned
            schema("mo_branch"),   // dynamic
            schema("memory_store"), // pinned
            schema("web_fetch"),   // dynamic
        ];
        annotate_tool_schemas_for_caching(&mut tools, &cfg_anthropic());

        // Marker on memory_store (last pinned, idx 4) — not on web_fetch (last tool, idx 5).
        assert!(
            tools[4].get("cache_control").is_some(),
            "marker should land on last pinned tool (memory_store at idx 4)"
        );
        assert!(
            tools[5].get("cache_control").is_none(),
            "dynamic tool after last pinned must NOT carry marker"
        );
    }

    /// Default pinned set must contain the static-lib tools — if someone
    /// demotes one, cache hit rate drops proportional to its token cost.
    #[test]
    fn default_pinned_set_contains_static_lib() {
        let pinned = default_pinned_tool_names();
        // TOOL_CATALOG-declared pinned tools
        for name in [
            "bash",
            "read_file",
            "write_file",
            "str_replace",
            "list_dir",
            "grep",
            "glob",
            "git_status",
            "git_diff",
            "memory_store",
            "memory_retrieve",
            "memory_purge",
            "memory_correct",
        ] {
            assert!(
                pinned.contains(name),
                "{name} must stay in default pinned set (static-lib guarantee)"
            );
        }
        // Auto-pinned via upsert_schema — not in TOOL_CATALOG but structurally part of the static lib.
        for name in ["skill", "spawn_agent", "get_agent_result", "send_message"] {
            assert!(
                pinned.contains(name),
                "{name} is auto-pinned at runtime; default set must mirror that"
            );
        }
    }

    /// `default_pinned_tool_names()` must return the same set across calls —
    /// downstream logic caches the handle per request, but new callers assume
    /// it's stable.
    #[test]
    fn default_pinned_set_is_deterministic() {
        let first = default_pinned_tool_names();
        for _ in 0..20 {
            assert_eq!(default_pinned_tool_names(), first);
        }
    }

    /// Bedrock path: tools get translated to `toolSpec` blocks + a trailing
    /// `cachePoint`. The cachePoint must sit AT THE END OF THE PINNED PREFIX,
    /// not at the end of the full tool list.
    #[test]
    fn bedrock_request_body_places_cachepoint_at_pinned_boundary() {
        let mut tools = pinned_prefix_fixture();
        tools.extend([schema("git_log"), schema("mo_branch")]);
        annotate_tool_schemas_for_caching(&mut tools, &cfg_anthropic());

        let body = build_provider_request_body(
            &[json!({"role": "user", "content": "hi"})],
            &tools,
            "anthropic.claude-sonnet-4-20250514-v1:0",
            "bedrock",
            Some(256),
            None,
            false,
            &ThinkingConfig::Off,
        );

        let out_tools = body["toolConfig"]["tools"]
            .as_array()
            .expect("bedrock toolConfig.tools");

        // Find the cachePoint. The index should be pinned_count (since each
        // pinned tool maps to one toolSpec, the cachePoint gets inserted right
        // after the last pinned tool).
        let cp_positions: Vec<usize> = out_tools
            .iter()
            .enumerate()
            .filter_map(|(i, t)| t.get("cachePoint").map(|_| i))
            .collect();
        assert_eq!(
            cp_positions.len(),
            1,
            "exactly one cachePoint expected in Bedrock tool list, got {cp_positions:?}"
        );
        let pinned_count = pinned_prefix_fixture().len();
        assert_eq!(
            cp_positions[0], pinned_count,
            "cachePoint must sit immediately after the pinned prefix \
             (pinned_count={pinned_count}), got {}",
            cp_positions[0]
        );

        // No cachePoint after the dynamic tools — they're explicitly post-cache.
        for (i, t) in out_tools.iter().enumerate().skip(pinned_count + 1) {
            assert!(
                t.get("cachePoint").is_none(),
                "dynamic tool at idx {i} must not carry cachePoint"
            );
        }
    }

    /// Direct Anthropic path: tools are rewritten to `{name, input_schema}`
    /// blocks with `cache_control` preserved. The marker must survive the
    /// rewrite and land on the correct (last pinned) tool.
    #[test]
    fn anthropic_direct_request_preserves_cache_control_on_last_pinned() {
        let mut tools = pinned_prefix_fixture();
        tools.extend([schema("git_log"), schema("mo_branch")]);
        annotate_tool_schemas_for_caching(&mut tools, &cfg_anthropic());

        let body = build_provider_request_body(
            &[json!({"role": "user", "content": "hi"})],
            &tools,
            "claude-sonnet-4-5-20250929",
            "anthropic",
            Some(256),
            None,
            false,
            &ThinkingConfig::Off,
        );

        let out_tools = body["tools"]
            .as_array()
            .expect("anthropic tools field must be an array");

        // Exactly one tool carries cache_control.
        let marked: Vec<usize> = out_tools
            .iter()
            .enumerate()
            .filter_map(|(i, t)| t.get("cache_control").map(|_| i))
            .collect();
        assert_eq!(
            marked.len(),
            1,
            "exactly one cache_control expected on anthropic tools, got {marked:?}"
        );

        // The marked tool is the last pinned (skill, idx 13 in our fixture).
        let pinned_count = pinned_prefix_fixture().len();
        assert_eq!(
            marked[0],
            pinned_count - 1,
            "cache_control must land on last pinned tool (idx {}), got {}",
            pinned_count - 1,
            marked[0]
        );

        // cache_control is simple ephemeral — no ttl/scope (Bedrock compat +
        // no beta header dependence).
        let cc = &out_tools[marked[0]]["cache_control"];
        assert_eq!(cc["type"].as_str(), Some("ephemeral"));
        assert!(cc.get("ttl").is_none());
        assert!(cc.get("scope").is_none());
    }

    /// Direct Anthropic path, identical assembly twice — request bodies must
    /// be byte-identical up to the cache_control host. This is the test that
    /// would catch HashMap iter drift, non-deterministic serialization, and
    /// any future bug that silently reshuffles the pinned prefix.
    #[test]
    fn anthropic_direct_request_pinned_bytes_identical_across_calls() {
        let build_once = || {
            let mut tools = pinned_prefix_fixture();
            // Deliberately DIFFERENT dynamic tools each call — the test
            // asserts the pinned portion is unaffected.
            tools.extend([schema("git_log"), schema("mo_branch")]);
            annotate_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
            build_provider_request_body(
                &[json!({"role": "user", "content": "hi"})],
                &tools,
                "claude-sonnet-4-5-20250929",
                "anthropic",
                Some(256),
                None,
                false,
                &ThinkingConfig::Off,
            )
        };
        let a = build_once();
        let b_tools_churned = {
            let mut tools = pinned_prefix_fixture();
            tools.extend([schema("web_fetch"), schema("github_list_prs"), schema("mo_query")]);
            annotate_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
            build_provider_request_body(
                &[json!({"role": "user", "content": "hi"})],
                &tools,
                "claude-sonnet-4-5-20250929",
                "anthropic",
                Some(256),
                None,
                false,
                &ThinkingConfig::Off,
            )
        };

        let a_tools = a["tools"].as_array().unwrap();
        let b_tools = b_tools_churned["tools"].as_array().unwrap();
        let pinned_count = pinned_prefix_fixture().len();

        for i in 0..pinned_count {
            let sa = serde_json::to_string(&a_tools[i]).unwrap();
            let sb = serde_json::to_string(&b_tools[i]).unwrap();
            assert_eq!(
                sa, sb,
                "anthropic pinned tool at idx {i} must be byte-identical across calls"
            );
        }
    }

    /// Bedrock path parallel to the anthropic direct test — two calls with
    /// different dynamic tools must produce byte-identical bytes up to (and
    /// including) the cachePoint.
    #[test]
    fn bedrock_request_pinned_bytes_identical_across_calls() {
        let build = |extra: Vec<Value>| {
            let mut tools = pinned_prefix_fixture();
            tools.extend(extra);
            annotate_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
            build_provider_request_body(
                &[json!({"role": "user", "content": "hi"})],
                &tools,
                "anthropic.claude-sonnet-4-20250514-v1:0",
                "bedrock",
                Some(256),
                None,
                false,
                &ThinkingConfig::Off,
            )
        };
        let a = build(vec![schema("git_log"), schema("mo_branch")]);
        let b = build(vec![schema("web_fetch")]);

        let a_tools = a["toolConfig"]["tools"].as_array().unwrap();
        let b_tools = b["toolConfig"]["tools"].as_array().unwrap();
        // pinned_count tools + 1 cachePoint block = pinned_count + 1 entries
        // that must match.
        let pinned_count = pinned_prefix_fixture().len();
        for i in 0..=pinned_count {
            let sa = serde_json::to_string(&a_tools[i]).unwrap();
            let sb = serde_json::to_string(&b_tools[i]).unwrap();
            assert_eq!(
                sa, sb,
                "bedrock tool[{i}] must be byte-identical across calls \
                 (pinned prefix {pinned_count} + cachePoint)"
            );
        }
    }

    /// OpenAI-compatible providers (DeepSeek, Qwen, MiniMax, vanilla OpenAI)
    /// don't consume `cache_control` — the field should still be present
    /// in the outgoing body (server-side caches like DeepSeek auto-dedupe
    /// on prefix, and extra keys are ignored), AND the pinned prefix bytes
    /// must be stable across calls for auto-prefix-cache to hit.
    #[test]
    fn openai_compatible_pinned_bytes_identical_across_calls() {
        let build = |extra: Vec<Value>| {
            let mut tools = pinned_prefix_fixture();
            tools.extend(extra);
            annotate_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
            build_provider_request_body(
                &[json!({"role": "user", "content": "hi"})],
                &tools,
                "deepseek-chat",
                "openai",
                Some(256),
                None,
                false,
                &ThinkingConfig::Off,
            )
        };
        let a = build(vec![schema("git_log"), schema("mo_branch")]);
        let b = build(vec![schema("web_fetch")]);

        let a_tools = a["tools"].as_array().unwrap();
        let b_tools = b["tools"].as_array().unwrap();
        let pinned_count = pinned_prefix_fixture().len();
        for i in 0..pinned_count {
            let sa = serde_json::to_string(&a_tools[i]).unwrap();
            let sb = serde_json::to_string(&b_tools[i]).unwrap();
            assert_eq!(
                sa, sb,
                "openai pinned tool at idx {i} must be byte-identical across calls \
                 (needed for auto-prefix-cache on DeepSeek/etc.)"
            );
        }
    }

    /// User-defined tools: schemas registered at session start flow through
    /// `inject_schema_pinned(s, true)` and must therefore land INSIDE the
    /// cacheable pinned segment. We simulate this by directly inserting
    /// into the default pinned set and verifying the marker moves to after
    /// the user-added tool.
    #[test]
    fn user_registered_pinned_tool_joins_static_prefix() {
        let mut tools = pinned_prefix_fixture();
        // Simulate a user-defined tool registered via upsert_schema before
        // the session starts — it lives AFTER the catalog-declared pinned
        // tools but BEFORE any dynamic selection.
        tools.push(schema("my_custom_db_tool"));
        // Tell the annotator this name counts as pinned (mirrors what
        // upsert_schema=true achieves at registry level).
        let mut pinned: HashSet<String> = default_pinned_tool_names();
        pinned.insert("my_custom_db_tool".into());
        // Add a dynamic tail.
        tools.push(schema("git_log"));
        tools.push(schema("mo_branch"));
        annotate_tool_schemas_for_caching_with_pinned(&mut tools, &cfg_anthropic(), &pinned);

        let last_pinned_idx = pinned_prefix_fixture().len(); // == 14 (my_custom_db_tool at idx 14)
        assert!(
            tools[last_pinned_idx].get("cache_control").is_some(),
            "user-registered pinned tool must host the cache marker \
             (at idx {last_pinned_idx})"
        );
        assert!(
            tools[last_pinned_idx + 1].get("cache_control").is_none(),
            "dynamic tool right after user-pinned must not carry marker"
        );
    }

    /// Runtime-discovered dynamic tool/skill (e.g. via MCP tool-list-changed
    /// or discover_skills): these enter the dynamic segment. Cache on the
    /// pinned prefix must remain untouched when they come and go.
    #[test]
    fn runtime_dynamic_addition_does_not_touch_pinned_cache() {
        let mut without = pinned_prefix_fixture();
        without.push(schema("git_log"));
        annotate_tool_schemas_for_caching(&mut without, &cfg_anthropic());

        let mut with_new_mcp = pinned_prefix_fixture();
        with_new_mcp.push(schema("git_log"));
        with_new_mcp.push(schema("mcp_new_runtime_tool")); // discovered mid-session
        annotate_tool_schemas_for_caching(&mut with_new_mcp, &cfg_anthropic());

        let pinned_count = pinned_prefix_fixture().len();
        for i in 0..pinned_count {
            assert_eq!(
                without[i], with_new_mcp[i],
                "pinned tool at idx {i} must survive runtime dynamic-tool addition"
            );
        }
        // Marker stays on the same pinned tool, bytes match.
        assert_eq!(
            without[pinned_count - 1], with_new_mcp[pinned_count - 1],
            "pinned tool hosting the marker must be byte-identical \
             (pinned prefix cache hits regardless of MCP churn)"
        );
    }
}
