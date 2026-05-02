//! End-to-end verification of the memory business type system.
//!
//! Tests the full data loop WITHOUT live Memoria or LLM:
//! 1. Business type → encode → Memoria payload structure
//! 2. Memoria response → decode → category identification
//! 3. Extraction response → parse → encode → batch payload
//! 4. System prompt includes type taxonomy when memory tools present
//! 5. Quality gate filters extraction output correctly

use astra_prompts::memory_types::{self, MemoryCategory, MemoryPromptMode};

// ── 1. Encode → Memoria payload roundtrip ─────────────────────────────────

#[test]
fn encode_produces_valid_memoria_payload() {
    let cat = MemoryCategory::Feedback;
    let text = "always use RS256 for JWT, not HS256";
    let encoded = memory_types::encode(cat, text);
    assert_eq!(encoded, "[feedback] always use RS256 for JWT, not HS256");

    let payload = serde_json::json!({
        "content": encoded,
        "memory_type": cat.memoria_type(),
        "trust_tier": cat.trust_tier(),
        "session_id": "sess-42",
    });
    assert_eq!(payload["memory_type"], "semantic");
    assert_eq!(payload["trust_tier"], "T2");
    assert!(payload["content"].as_str().unwrap().starts_with("[feedback]"));
}

#[test]
fn all_categories_produce_valid_payloads() {
    let test_cases = [
        (MemoryCategory::User, "profile", "T1"),
        (MemoryCategory::Feedback, "semantic", "T2"),
        (MemoryCategory::Project, "semantic", "T3"),
        (MemoryCategory::Reference, "procedural", "T2"),
        (MemoryCategory::Lesson, "semantic", "T3"),
        (MemoryCategory::Episode, "episodic", "T3"),
    ];
    for (cat, expected_type, expected_tier) in test_cases {
        let encoded = memory_types::encode(cat, "test content");
        let (decoded_cat, decoded_text) = memory_types::decode(&encoded);

        assert_eq!(decoded_cat, Some(cat), "roundtrip for {cat:?}");
        assert_eq!(decoded_text, "test content");
        assert_eq!(cat.memoria_type(), expected_type, "type for {cat:?}");
        assert_eq!(cat.trust_tier(), expected_tier, "tier for {cat:?}");
    }
}

// ── 2. Memoria response → decode → category ──────────────────────────────

#[test]
fn decode_mixed_legacy_and_typed_memories() {
    let memories = [
        "[user] senior Rust engineer, prefers CLI tools",
        "[feedback] don't mock the database in integration tests",
        "[project] merge freeze starts 2026-05-08 for mobile release",
        "[ref] pipeline bugs tracked in Linear project INGEST",
        "💡 LESSON: use rg not grep in this monorepo",
        "plain old memory with no prefix",
    ];

    let decoded: Vec<_> = memories.iter().map(|m| memory_types::decode(m)).collect();

    assert_eq!(decoded[0].0, Some(MemoryCategory::User));
    assert_eq!(decoded[0].1, "senior Rust engineer, prefers CLI tools");

    assert_eq!(decoded[1].0, Some(MemoryCategory::Feedback));
    assert_eq!(decoded[2].0, Some(MemoryCategory::Project));
    assert_eq!(decoded[3].0, Some(MemoryCategory::Reference));

    // Legacy content: no business type prefix → None
    assert_eq!(decoded[4].0, None);
    assert!(decoded[4].1.starts_with("💡 LESSON:"));
    assert_eq!(decoded[5].0, None);
    assert_eq!(decoded[5].1, "plain old memory with no prefix");
}

// ── 2b. Forward compatibility: unknown prefix from future version ─────────

#[test]
fn decode_unknown_future_prefix_graceful_degradation() {
    let future_content = "[v2_new_type] some content from a future Memoria version";
    let (cat, text) = memory_types::decode(future_content);
    assert_eq!(cat, None, "unknown prefix must degrade to None");
    assert_eq!(text, future_content, "full text preserved on unknown prefix");
}

#[test]
fn decode_empty_string() {
    let (cat, text) = memory_types::decode("");
    assert_eq!(cat, None);
    assert_eq!(text, "");
}

// ── 3. Extraction response → parse → encode → batch payload ──────────────

#[test]
fn extraction_to_batch_payload_full_pipeline() {
    use astra_prompts::memory_types::{encode, MemoryCategory};

    let llm_response = r#"[
        {"type": "feedback", "content": "prefers compact JSON output"},
        {"type": "user", "content": "data scientist with Python background"},
        {"type": "project", "content": "API deadline is June 15th"},
        {"type": "ref", "content": "dashboards at grafana.internal/d/api-latency"}
    ]"#;

    // Step 1: parse extraction response
    let parsed: Vec<serde_json::Value> = serde_json::from_str(llm_response).unwrap();
    let memories: Vec<(MemoryCategory, String)> = parsed
        .iter()
        .filter_map(|v| {
            let type_str = v.get("type")?.as_str()?;
            let content = v.get("content")?.as_str()?.to_string();
            let cat = match type_str {
                "user" => MemoryCategory::User,
                "feedback" => MemoryCategory::Feedback,
                "project" => MemoryCategory::Project,
                "ref" => MemoryCategory::Reference,
                _ => return None,
            };
            Some((cat, content))
        })
        .collect();

    assert_eq!(memories.len(), 4);

    // Step 2: encode into batch payload
    let batch: Vec<serde_json::Value> = memories
        .iter()
        .map(|(cat, text)| {
            serde_json::json!({
                "content": encode(*cat, text),
                "memory_type": cat.memoria_type(),
                "trust_tier": cat.trust_tier(),
                "source": {"agent": "extraction"},
            })
        })
        .collect();

    // Step 3: verify batch structure matches Memoria V1 /v1/memories/batch contract
    assert_eq!(batch.len(), 4);

    assert_eq!(
        batch[0]["content"].as_str().unwrap(),
        "[feedback] prefers compact JSON output"
    );
    assert_eq!(batch[0]["memory_type"], "semantic");
    assert_eq!(batch[0]["trust_tier"], "T2");
    assert_eq!(batch[0]["source"]["agent"], "extraction");

    assert_eq!(batch[1]["memory_type"], "profile");
    assert_eq!(batch[1]["trust_tier"], "T1");

    assert_eq!(batch[2]["memory_type"], "semantic");
    assert_eq!(batch[2]["trust_tier"], "T3");

    assert_eq!(batch[3]["memory_type"], "procedural");
    assert_eq!(batch[3]["trust_tier"], "T2");
}

// ── 4. System prompt type taxonomy contract ──────────────────────────────

#[test]
fn system_prompt_full_mode_exercises_all_business_types() {
    let prompt = astra_runtime::prompts::build_main_system_prompt(
        &["memory_store", "memory_search", "memory_correct"],
        "",
        1.0,
        None,
    );

    // All 4 user-facing types present in taxonomy
    assert!(prompt.contains("<name>user</name>"));
    assert!(prompt.contains("<name>feedback</name>"));
    assert!(prompt.contains("<name>project</name>"));
    assert!(prompt.contains("<name>reference</name>"));

    // Concrete examples (not vague instructions)
    assert!(prompt.contains("data scientist"));
    assert!(prompt.contains("don't mock the database"));
    assert!(prompt.contains("merge freeze"));
    assert!(prompt.contains("Linear project"));

    // Anti-requirements: NO hardcoded trigger keywords
    assert!(!prompt.contains("### Triggers:"));
    assert!(!prompt.contains("关注|跟踪|留意"));

    // What NOT to save
    assert!(prompt.contains("derivable from the codebase"));
    assert!(prompt.contains("git log"));

    // Deduplication + staleness
    assert!(prompt.contains("memory_correct"));
    assert!(prompt.contains("outdated"));
}

#[test]
fn system_prompt_minimal_mode_omits_taxonomy() {
    let prompt = astra_runtime::prompts::build_main_system_prompt(
        &["memory_store"],
        "",
        1.0,
        None,
    );

    assert!(prompt.contains("Memory Rules"));
    assert!(!prompt.contains("<types>"));
    assert!(!prompt.contains("What NOT to save"));
}

#[test]
fn system_prompt_no_memory_tools_omits_everything() {
    let prompt = astra_runtime::prompts::build_main_system_prompt(
        &["bash", "read_file"],
        "",
        1.0,
        None,
    );

    assert!(!prompt.contains("Memory Rules"));
    assert!(!prompt.contains("<types>"));
}

// ── 5. Quality gate integration ──────────────────────────────────────────

#[test]
fn quality_gate_filters_extraction_output() {
    use astra_runtime::lesson_synthesizer::is_high_quality_lesson;

    // These should PASS the gate (10+ chars, no hedging)
    assert!(is_high_quality_lesson(
        "always use RS256 for JWT signing, not HS256"
    ));
    assert!(is_high_quality_lesson(
        "prefers compact JSON output without pretty-printing"
    ));

    // These should FAIL the gate
    assert!(!is_high_quality_lesson("maybe X"));
    assert!(!is_high_quality_lesson("not sure about this approach"));
    assert!(!is_high_quality_lesson("hi")); // too short
}

// ── 6. Prompt builder vs memory_types consistency ────────────────────────

#[test]
fn prompt_builder_modes_match_memory_types_module() {
    let none = memory_types::build_memory_prompt(MemoryPromptMode::None);
    let minimal = memory_types::build_memory_prompt(MemoryPromptMode::Minimal);
    let full = memory_types::build_memory_prompt(MemoryPromptMode::Full);

    assert!(none.is_empty());
    assert!(minimal.len() < full.len());
    assert!(full.contains("<types>"));
    assert!(!minimal.contains("<types>"));
}

// ── 7. V2 readiness ─────────────────────────────────────────────────────

#[test]
fn v2_tags_defined_for_all_categories() {
    for &cat in MemoryCategory::ALL {
        let tag = cat.v2_tag();
        assert!(tag.starts_with("astra:"), "{cat:?} missing astra: prefix");
        assert!(tag.len() > 6, "{cat:?} tag too short: {tag}");
    }
}

#[test]
fn v2_tag_names_are_unique() {
    let tags: Vec<&str> = MemoryCategory::ALL.iter().map(|c| c.v2_tag()).collect();
    let unique: std::collections::HashSet<&&str> = tags.iter().collect();
    assert_eq!(tags.len(), unique.len(), "V2 tags must be unique");
}

// ── 8. Journal event structure ──────────────────────────────────────────

#[test]
fn memory_extraction_journal_event_structure() {
    let evt = astra_services::session_journal::JournalEvent::memory_extraction(
        Some("sess-42"),
        3,
        "extracted",
        2,
        &["feedback".into(), "project".into()],
        1200,
    );

    assert_eq!(
        evt.event_type,
        astra_services::session_journal::JournalEventType::MemoryExtraction
    );
    assert_eq!(evt.turn, Some(3));
    assert_eq!(evt.duration_ms, Some(1200));
    assert_eq!(evt.session_id.as_deref(), Some("sess-42"));

    let meta = evt.metadata.as_ref().unwrap();
    assert_eq!(meta["outcome"], "extracted");
    assert_eq!(meta["memories_saved"], 2);
    let cats = meta["categories"].as_array().unwrap();
    assert_eq!(cats.len(), 2);
}

#[test]
fn memory_extraction_journal_event_skipped() {
    let evt = astra_services::session_journal::JournalEvent::memory_extraction(
        Some("sess-42"),
        5,
        "skipped_main_wrote",
        0,
        &[],
        0,
    );

    let meta = evt.metadata.as_ref().unwrap();
    assert_eq!(meta["outcome"], "skipped_main_wrote");
    assert_eq!(meta["memories_saved"], 0);
}
