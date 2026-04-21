//! Prompt caching utilities for LLM system messages.
//!
//! Provides provider-aware system message construction with cache_control annotations
//! for Anthropic and stable-prefix splitting for OpenAI. Used by both the bridge proxy
//! and `ServerAgenticLoopHost`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};

use crate::prompts;

// ── PromptCacheConfig ────────────────────────────────────────────────────────

/// Configuration for provider-specific prompt caching.
pub struct PromptCacheConfig {
    /// Whether cache_control annotations are enabled for Anthropic.
    pub cache_enabled: bool,
    /// Whether the provider supports cache_control (Anthropic/Claude).
    pub is_anthropic: bool,
}

impl PromptCacheConfig {
    /// Latch config from environment and provider info. Call once at session start.
    pub fn latch(provider: &str, model_name: &str) -> Self {
        let cache_enabled = !std::env::var("MO_PROMPT_CACHE_DISABLED")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        let is_anthropic = provider == "anthropic" || model_name.contains("claude");
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

/// Cached prompt sections (Global + Session scoped).
struct CachedSections {
    /// Concatenated text of Global+Session sections (for non-Anthropic providers).
    text: String,
    /// Individual sections with scope metadata (for Anthropic cache_control).
    sections: Vec<prompts::PromptSection>,
}

fn section_cache() -> &'static Mutex<HashMap<u64, CachedSections>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, CachedSections>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn section_cache_key(
    tool_names: &[&str],
    task_type: Option<&str>,
    confidence: f64,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for name in tool_names {
        name.hash(&mut hasher);
    }
    task_type.unwrap_or("none").hash(&mut hasher);
    let bucket = if confidence < 0.3 { "low" } else { "normal" };
    bucket.hash(&mut hasher);
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
    let key = section_cache_key(tool_names, task_type, confidence);

    // Try cache for the stable (Global + Session) sections
    let cached = if let Ok(cache) = section_cache().lock() {
        cache
            .get(&key)
            .map(|c| (c.text.clone(), c.sections.clone()))
    } else {
        None
    };

    let (stable_text, sections) = cached.unwrap_or_else(|| {
        // Build all sections (profile_desc is "" for cache — we'll append it separately)
        let all = prompts::build_system_prompt_sections(tool_names, "", confidence, task_type);
        // Only cache Global + Session sections (not None-scoped profile)
        let stable: Vec<prompts::PromptSection> = all
            .into_iter()
            .filter(|s| s.scope != prompts::CacheScope::None)
            .collect();
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
                },
            );
        }
        (text, stable)
    });

    let is_anthropic = cache_cfg.is_anthropic;
    let dynamic_text = prompts::sections_to_string(dynamic_sections);

    // Build complete sections list (stable + dynamic) for trace.
    let append_dynamic = |mut secs: Vec<prompts::PromptSection>| -> Vec<prompts::PromptSection> {
        secs.extend(dynamic_sections.iter().cloned());
        secs
    };

    if is_anthropic {
        // Anthropic: multi-block content with cache_control on stable sections.
        //
        // Cache strategy:
        //   Place cache_control on the LAST block of each scope group.
        //   Anthropic allows up to 4 breakpoints per request — we use at most 2
        //   (last Global, last Session). The provider caches the prefix up to
        //   each breakpoint.
        //
        //   Global  → scope:"global" + ttl:"1h"  (shared across all sessions/orgs)
        //   Session → ttl:"1h"                    (stable within a session)
        //   None    → no cache_control             (changes every turn)
        let cache_disabled = !cache_cfg.cache_enabled;

        // Find the last index of each scope group for breakpoint placement
        let last_global = sections
            .iter()
            .rposition(|s| s.scope == prompts::CacheScope::Global);
        let last_session = sections
            .iter()
            .rposition(|s| s.scope == prompts::CacheScope::Session);

        let mut blocks: Vec<Value> = Vec::with_capacity(sections.len() + 1);
        for (i, section) in sections.iter().enumerate() {
            let cc = if cache_disabled {
                None
            } else if Some(i) == last_global {
                Some(json!({"type": "ephemeral", "scope": "global", "ttl": "1h"}))
            } else if Some(i) == last_session {
                Some(json!({"type": "ephemeral", "ttl": "1h"}))
            } else {
                None
            };
            let mut block = json!({
                "type": "text",
                "text": section.text,
            });
            if let Some(cc) = cc {
                block["cache_control"] = cc;
            }
            blocks.push(block);
        }
        // Dynamic section (profile + per-turn hints) — no cache_control
        for section in dynamic_sections {
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

/// Add `cache_control` to the last tool schema for Anthropic caching.
///
/// Anthropic allows only 4 cache_control breakpoints per request. Our allocation:
/// - System prompt: 2 breakpoints (global scope + session scope)
/// - Tools: 1 breakpoint (last tool only)
/// - Messages: 1 breakpoint (last message)
pub(crate) fn annotate_tool_schemas_for_caching(
    tools: &mut [Value],
    cache_cfg: &PromptCacheConfig,
) {
    if !cache_cfg.should_annotate() || tools.is_empty() {
        return;
    }
    // Mark only the last tool — this creates a single cache covering all tools.
    let last_idx = tools.len() - 1;
    tools[last_idx]["cache_control"] = json!({"type": "ephemeral", "ttl": "1h"});
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

#[cfg(test)]
mod tests {
    use super::*;

    static CACHE_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    fn build_system_message_anthropic_has_cache_control() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

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
        unsafe {
            std::env::set_var("MO_PROMPT_CACHE_DISABLED", "1");
        }
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
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }
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
            "last tool should have cache_control"
        );
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
}
