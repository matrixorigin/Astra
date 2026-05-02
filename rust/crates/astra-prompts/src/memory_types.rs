//! Business-layer memory type abstraction.
//!
//! Maps user-facing memory categories to Memoria API primitives
//! (`memory_type` + `trust_tier`). Content is prefix-encoded so the
//! category survives a V1 store→retrieve roundtrip.
//!
//! When Memoria V2 stabilizes, prefixes migrate to V2 tags (`astra:user`,
//! `astra:feedback`, etc.) and content reverts to plain text.

/// User-facing memory categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryCategory {
    /// User role, preferences, knowledge background.
    User,
    /// Corrections and confirmations about how to approach work.
    Feedback,
    /// Non-derivable project context (deadlines, decisions, people).
    Project,
    /// Pointers to external systems (URLs, project names, dashboards).
    Reference,
    /// Cross-session reusable tool/pattern lessons.
    Lesson,
    /// Session summaries (episodic).
    Episode,
}

impl MemoryCategory {
    pub const ALL: &[MemoryCategory] = &[
        Self::User,
        Self::Feedback,
        Self::Project,
        Self::Reference,
        Self::Lesson,
        Self::Episode,
    ];

    pub fn content_prefix(self) -> &'static str {
        match self {
            Self::User => "[user]",
            Self::Feedback => "[feedback]",
            Self::Project => "[project]",
            Self::Reference => "[ref]",
            Self::Lesson => "[lesson]",
            Self::Episode => "[episode]",
        }
    }

    pub fn memoria_type(self) -> &'static str {
        match self {
            Self::User => "profile",
            Self::Feedback | Self::Project | Self::Lesson => "semantic",
            Self::Reference => "procedural",
            Self::Episode => "episodic",
        }
    }

    pub fn trust_tier(self) -> &'static str {
        match self {
            Self::User => "T1",
            Self::Feedback | Self::Reference => "T2",
            Self::Project | Self::Lesson | Self::Episode => "T3",
        }
    }

    pub fn v2_tag(self) -> &'static str {
        match self {
            Self::User => "astra:user",
            Self::Feedback => "astra:feedback",
            Self::Project => "astra:project",
            Self::Reference => "astra:reference",
            Self::Lesson => "astra:lesson",
            Self::Episode => "astra:episode",
        }
    }

    pub fn from_prefix(prefix: &str) -> Option<Self> {
        match prefix {
            "[user]" => Some(Self::User),
            "[feedback]" => Some(Self::Feedback),
            "[project]" => Some(Self::Project),
            "[ref]" => Some(Self::Reference),
            "[lesson]" => Some(Self::Lesson),
            "[episode]" => Some(Self::Episode),
            _ => None,
        }
    }
}

/// Encode content with category prefix.
pub fn encode(category: MemoryCategory, text: &str) -> String {
    format!("{} {}", category.content_prefix(), text)
}

/// Decode category from prefixed content. Returns `(None, full_text)` for
/// unprefixed legacy content — graceful degradation.
pub fn decode(raw: &str) -> (Option<MemoryCategory>, &str) {
    if let Some(rest) = raw.strip_prefix("[user] ") {
        (Some(MemoryCategory::User), rest)
    } else if let Some(rest) = raw.strip_prefix("[feedback] ") {
        (Some(MemoryCategory::Feedback), rest)
    } else if let Some(rest) = raw.strip_prefix("[project] ") {
        (Some(MemoryCategory::Project), rest)
    } else if let Some(rest) = raw.strip_prefix("[ref] ") {
        (Some(MemoryCategory::Reference), rest)
    } else if let Some(rest) = raw.strip_prefix("[lesson] ") {
        (Some(MemoryCategory::Lesson), rest)
    } else if let Some(rest) = raw.strip_prefix("[episode] ") {
        (Some(MemoryCategory::Episode), rest)
    } else {
        (None, raw)
    }
}

// ── System prompt builder ──────────────────────────────────────────────────

/// Prompt mode determines how much memory guidance to inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPromptMode {
    None,
    Minimal,
    Full,
}

/// Build the memory section for the system prompt.
pub fn build_memory_prompt(mode: MemoryPromptMode) -> String {
    if mode == MemoryPromptMode::None {
        return String::new();
    }

    let mut s = String::with_capacity(3000);
    s.push_str("\n## Memory Rules (check BEFORE reasoning about tools)\n\n");

    if mode == MemoryPromptMode::Minimal {
        s.push_str(MINIMAL_RULES);
        return s;
    }

    s.push_str(TYPES_SECTION);
    s.push('\n');
    s.push_str(WHAT_NOT_TO_SAVE);
    s.push('\n');
    s.push_str(WHEN_TO_ACCESS);
    s.push('\n');
    s.push_str(BEFORE_RECOMMENDING);
    s
}

const MINIMAL_RULES: &str = "\
When user expresses a preference, decision, or fact worth remembering → call memory_store IMMEDIATELY.\n\
- Do NOT ask whether to store — just store, then confirm.\n\
- Do NOT explore codebase for interest expressions.\n";

const TYPES_SECTION: &str = "\
<types>
<type>
    <name>user</name>
    <description>User's role, goals, preferences, and knowledge. Tailor future behavior to the user's perspective — collaborate with a senior engineer differently than a first-time coder.</description>
    <when_to_save>When you learn details about the user's role, preferences, responsibilities, or expertise.</when_to_save>
    <how_to_use>Adapt explanations, tool choices, and communication style to the user's profile.</how_to_use>
    <examples>
    user: I'm a data scientist investigating our logging pipeline
    → store as user memory: user is a data scientist, focused on observability/logging

    user: I've been writing Go for ten years but this is my first time with React
    → store as user memory: deep Go expertise, new to React — frame frontend explanations in backend analogues
    </examples>
</type>
<type>
    <name>feedback</name>
    <description>Corrections AND confirmations about how to approach work. Record from failure AND success — avoid past mistakes while preserving validated approaches.</description>
    <when_to_save>User corrects you (\"don't do X\", \"stop\", \"not that\") OR confirms a non-obvious approach (\"yes exactly\", \"perfect\", accepting an unusual choice). Include WHY so you can judge edge cases.</when_to_save>
    <how_to_use>Follow these rules so the user never has to give the same guidance twice.</how_to_use>
    <examples>
    user: don't mock the database in tests — mocked tests passed but prod migration failed last quarter
    → store as feedback: integration tests must use real DB. Why: mock/prod divergence masked a broken migration

    user: yeah the single bundled PR was the right call here
    → store as feedback: for refactors in this area, prefer one bundled PR over many small ones
    </examples>
</type>
<type>
    <name>project</name>
    <description>Non-derivable project context: deadlines, incidents, decisions, personnel assignments. NOT architecture or code patterns (those are in the code).</description>
    <when_to_save>When you learn who is doing what, why, or by when. Convert relative dates to absolute (\"Thursday\" → \"2026-05-08\").</when_to_save>
    <how_to_use>Understand motivation and constraints behind the user's requests.</how_to_use>
    <examples>
    user: we're freezing merges after Thursday for the mobile release
    → store as project: merge freeze 2026-05-08 for mobile release. Flag non-critical PRs

    user: the auth rewrite is because legal flagged session token storage
    → store as project: auth rewrite driven by compliance, not tech debt — favor compliance over ergonomics
    </examples>
</type>
<type>
    <name>reference</name>
    <description>Pointers to external systems and resources — where to find information outside the codebase.</description>
    <when_to_save>When you learn about external resources and their purpose.</when_to_save>
    <how_to_use>When the user references an external system or needs information that may live outside the project.</how_to_use>
    <examples>
    user: check the Linear project \"INGEST\" for pipeline bug tickets
    → store as reference: pipeline bugs tracked in Linear project \"INGEST\"

    user: grafana.internal/d/api-latency is what oncall watches
    → store as reference: oncall latency dashboard at grafana.internal/d/api-latency — check when editing request-path code
    </examples>
</type>
</types>

### Storage rules
- Do NOT ask whether to store — just store, then confirm.
- Do NOT explore codebase for interest expressions.
- Before storing, check if a similar memory exists. Use memory_correct to update instead of duplicating.
- Negative preferences (\"不喜欢\", \"don't want\", \"stop using\") → store and respect in future decisions.
- If a memory seems outdated, correct it rather than storing a new one.
- '## User Memories' (when present) = user context — check it BEFORE calling any tool.\n";

const WHAT_NOT_TO_SAVE: &str = "\
### What NOT to save
- Code patterns, conventions, architecture, file paths — derivable from the codebase.
- Git history, recent changes — `git log` / `git blame` are authoritative.
- Debugging solutions or fix recipes — the fix is in the code; the commit has context.
- Anything already documented in CLAUDE.md or project rules.
- Ephemeral task details: temporary state, current conversation context.

These exclusions apply even when the user explicitly asks. If they ask to save a PR list, ask what was *surprising* — that is the part worth keeping.\n";

const WHEN_TO_ACCESS: &str = "\
### When to access memories
- When memories seem relevant, or the user references prior-conversation work.
- When the user explicitly asks you to check, recall, or remember.
- If the user says to *ignore* memory: do not apply, cite, or mention memory content.\n";

const BEFORE_RECOMMENDING: &str = "\
### Before recommending from memory
A memory is a claim about what was true *when it was written*. Before acting on it:
- If it names a file path: check the file exists.
- If it names a function or flag: grep for it.
- If a memory conflicts with current state: trust what you observe now.\n";

#[cfg(test)]
mod tests {
    use super::*;

    // ── Encode / Decode ──

    #[test]
    fn encode_roundtrip() {
        for &cat in MemoryCategory::ALL {
            let encoded = encode(cat, "hello world");
            let (decoded_cat, decoded_text) = decode(&encoded);
            assert_eq!(decoded_cat, Some(cat), "roundtrip failed for {cat:?}");
            assert_eq!(decoded_text, "hello world");
        }
    }

    #[test]
    fn decode_legacy_unprefixed() {
        let (cat, text) = decode("plain old memory content");
        assert_eq!(cat, None);
        assert_eq!(text, "plain old memory content");
    }

    #[test]
    fn decode_existing_lesson_prefix() {
        let (cat, text) = decode("💡 LESSON: use rg not grep");
        assert_eq!(cat, None, "legacy LESSON prefix should not match");
        assert_eq!(text, "💡 LESSON: use rg not grep");
    }

    #[test]
    fn decode_partial_prefix_no_space() {
        let (cat, text) = decode("[user]no space after bracket");
        assert_eq!(cat, None, "prefix without trailing space should not match");
        assert_eq!(text, "[user]no space after bracket");
    }

    #[test]
    fn encode_preserves_content() {
        let encoded = encode(MemoryCategory::Feedback, "prefer Rust for CLI tools");
        assert_eq!(encoded, "[feedback] prefer Rust for CLI tools");
    }

    // ── Decode edge cases ──

    #[test]
    fn decode_prefix_typo_treated_as_legacy() {
        let (cat, text) = decode("[userr] text");
        assert_eq!(cat, None);
        assert_eq!(text, "[userr] text");
    }

    #[test]
    fn decode_double_prefix_first_wins() {
        let (cat, text) = decode("[user] [feedback] mixed");
        assert_eq!(cat, Some(MemoryCategory::User));
        assert_eq!(text, "[feedback] mixed");
    }

    #[test]
    fn decode_empty_bracket_treated_as_legacy() {
        let (cat, text) = decode("[] text");
        assert_eq!(cat, None);
        assert_eq!(text, "[] text");
    }

    #[test]
    fn decode_space_inside_bracket_treated_as_legacy() {
        let (cat, text) = decode("[ user] text");
        assert_eq!(cat, None);
        assert_eq!(text, "[ user] text");
    }

    // ── Memoria mapping ──

    #[test]
    fn user_maps_to_profile_t1() {
        assert_eq!(MemoryCategory::User.memoria_type(), "profile");
        assert_eq!(MemoryCategory::User.trust_tier(), "T1");
    }

    #[test]
    fn feedback_maps_to_semantic_t2() {
        assert_eq!(MemoryCategory::Feedback.memoria_type(), "semantic");
        assert_eq!(MemoryCategory::Feedback.trust_tier(), "T2");
    }

    #[test]
    fn project_maps_to_semantic_t3() {
        assert_eq!(MemoryCategory::Project.memoria_type(), "semantic");
        assert_eq!(MemoryCategory::Project.trust_tier(), "T3");
    }

    #[test]
    fn reference_maps_to_procedural_t2() {
        assert_eq!(MemoryCategory::Reference.memoria_type(), "procedural");
        assert_eq!(MemoryCategory::Reference.trust_tier(), "T2");
    }

    #[test]
    fn lesson_maps_to_semantic_t3() {
        assert_eq!(MemoryCategory::Lesson.memoria_type(), "semantic");
        assert_eq!(MemoryCategory::Lesson.trust_tier(), "T3");
    }

    #[test]
    fn episode_maps_to_episodic_t3() {
        assert_eq!(MemoryCategory::Episode.memoria_type(), "episodic");
        assert_eq!(MemoryCategory::Episode.trust_tier(), "T3");
    }

    // ── V2 tags ──

    #[test]
    fn v2_tags_have_astra_prefix() {
        for &cat in MemoryCategory::ALL {
            assert!(
                cat.v2_tag().starts_with("astra:"),
                "{:?} tag missing astra: prefix",
                cat
            );
        }
    }

    // ── from_prefix ──

    #[test]
    fn from_prefix_all_variants() {
        assert_eq!(MemoryCategory::from_prefix("[user]"), Some(MemoryCategory::User));
        assert_eq!(MemoryCategory::from_prefix("[feedback]"), Some(MemoryCategory::Feedback));
        assert_eq!(MemoryCategory::from_prefix("[project]"), Some(MemoryCategory::Project));
        assert_eq!(MemoryCategory::from_prefix("[ref]"), Some(MemoryCategory::Reference));
        assert_eq!(MemoryCategory::from_prefix("[lesson]"), Some(MemoryCategory::Lesson));
        assert_eq!(MemoryCategory::from_prefix("[episode]"), Some(MemoryCategory::Episode));
        assert_eq!(MemoryCategory::from_prefix("[unknown]"), None);
    }

    // ── Prompt builder ──

    #[test]
    fn none_mode_returns_empty() {
        assert!(build_memory_prompt(MemoryPromptMode::None).is_empty());
    }

    #[test]
    fn minimal_mode_has_basic_rules() {
        let prompt = build_memory_prompt(MemoryPromptMode::Minimal);
        assert!(prompt.contains("Memory Rules"));
        assert!(prompt.contains("Do NOT ask"));
        assert!(!prompt.contains("<types>"), "minimal should not include type taxonomy");
    }

    #[test]
    fn full_mode_has_type_taxonomy() {
        let prompt = build_memory_prompt(MemoryPromptMode::Full);
        assert!(prompt.contains("<types>"));
        assert!(prompt.contains("<name>user</name>"));
        assert!(prompt.contains("<name>feedback</name>"));
        assert!(prompt.contains("<name>project</name>"));
        assert!(prompt.contains("<name>reference</name>"));
    }

    #[test]
    fn full_mode_has_what_not_to_save() {
        let prompt = build_memory_prompt(MemoryPromptMode::Full);
        assert!(prompt.contains("What NOT to save"));
        assert!(prompt.contains("derivable from the codebase"));
    }

    #[test]
    fn full_mode_has_when_to_access() {
        let prompt = build_memory_prompt(MemoryPromptMode::Full);
        assert!(prompt.contains("When to access"));
    }

    #[test]
    fn full_mode_has_before_recommending() {
        let prompt = build_memory_prompt(MemoryPromptMode::Full);
        assert!(prompt.contains("Before recommending"));
        assert!(prompt.contains("check the file exists"));
    }

    #[test]
    fn full_mode_no_hardcoded_trigger_keywords() {
        let prompt = build_memory_prompt(MemoryPromptMode::Full);
        assert!(
            !prompt.contains("关注|跟踪|留意"),
            "should not have hardcoded Chinese trigger keywords"
        );
        assert!(
            !prompt.contains("follow|watch|track|interested|prefer|remember"),
            "should not have hardcoded English trigger keyword list"
        );
    }

    #[test]
    fn full_mode_has_examples() {
        let prompt = build_memory_prompt(MemoryPromptMode::Full);
        assert!(prompt.contains("data scientist"));
        assert!(prompt.contains("don't mock the database"));
        assert!(prompt.contains("merge freeze"));
        assert!(prompt.contains("Linear project"));
    }

    #[test]
    fn full_mode_has_deduplication_rule() {
        let prompt = build_memory_prompt(MemoryPromptMode::Full);
        assert!(prompt.contains("memory_correct"));
    }

    #[test]
    fn full_mode_has_negative_preference_rule() {
        let prompt = build_memory_prompt(MemoryPromptMode::Full);
        assert!(prompt.contains("不喜欢") || prompt.contains("don't want"));
    }

    #[test]
    fn all_categories_count() {
        assert_eq!(MemoryCategory::ALL.len(), 6);
    }
}
