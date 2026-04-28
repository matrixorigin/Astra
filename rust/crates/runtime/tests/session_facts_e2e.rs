//! End-to-end integration tests for the hybrid session memory protocol (P1–P7).
//!
//! These tests exercise the full pipeline with real Memoria (no mocks, no LLM).
//! Requires: `MEMORIA_BASE_URL` + `MEMORIA_MASTER_KEY` (or `MEMORIA_API_KEY`).
//! Run with: `cargo test -p astra-runtime -- session_facts_e2e --ignored`

use astra_runtime::prompts::CompactionTier;
use astra_runtime::turn::cloud::memoria_compact::{
    HttpMemoriaClient, MemoriaClient, MemoriaCompactConfig, MemoriaCompactParams,
    SessionMemoryFileCombine, compact_with_memoria,
};
use astra_runtime::turn::cloud::session_end_governance::*;
use astra_turn_types::session_facts::*;
use astra_turn_core::cloud_session_memory_extract::*;
use astra_runtime::turn::cloud::session_memory_protocol::*;
use astra_turn_core::microcompact::*;
use astra_services::session_journal::{JournalEvent, JournalEventType, ToolCallRecord};
use serde_json::{Value, json};
use uuid::Uuid;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn memoria_client() -> Option<HttpMemoriaClient> {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for ancestor in manifest_dir.ancestors().take(5) {
        let env_path = ancestor.join(".env");
        if env_path.exists() {
            let _ = dotenvy::from_path(&env_path);
            break;
        }
    }
    HttpMemoriaClient::from_env()
}

async fn verified_client() -> Option<(HttpMemoriaClient, String)> {
    let client = memoria_client()?;
    let sid = format!("test-facts-{}", Uuid::new_v4().simple());
    match client.store("probe", "working", Some(&sid), None).await {
        Ok(id) => {
            let _ = client.delete(&id).await;
            Some((client, format!("test-facts-{}", Uuid::new_v4().simple())))
        }
        Err(e) => {
            eprintln!("Memoria not operational: {e}");
            None
        }
    }
}

struct Ctx {
    client: HttpMemoriaClient,
    sid: String,
    ids: Vec<String>,
}

impl Ctx {
    fn new(client: HttpMemoriaClient, sid: String) -> Self {
        Self {
            client,
            sid,
            ids: vec![],
        }
    }
    fn track(&mut self, id: String) {
        self.ids.push(id);
    }
    async fn cleanup(&self) {
        let _ = self.client.purge_working(&self.sid).await;
        for id in &self.ids {
            let _ = self.client.delete(id).await;
        }
    }
}

macro_rules! require_memoria {
    () => {
        match verified_client().await {
            Some((client, sid)) => Ctx::new(client, sid),
            None => {
                eprintln!("SKIPPED: Memoria not operational");
                return;
            }
        }
    };
}

fn make_tc(name: &str, ok: bool, file_path: Option<&str>) -> ToolCallRecord {
    ToolCallRecord {
        name: name.to_string(),
        ok,
        ms: 100,
        error: None,
        input_bytes: None,
        output_bytes: None,
        args_preview: None,
        result_preview: None,
        file_path: file_path.map(String::from),
        surgically_removed: None,
        original_tool_name: None,
        ..Default::default()
    }
}

fn make_event(
    turn: u32,
    tcs: Vec<ToolCallRecord>,
    tokens: u64,
    error: Option<&str>,
) -> JournalEvent {
    let mut e = JournalEvent::base_public(JournalEventType::Turn, None);
    e.turn = Some(turn);
    e.tokens_in = Some(tokens);
    e.tool_calls = Some(tcs);
    e.error = error.map(String::from);
    e
}

fn user(c: &str) -> Value {
    json!({"role": "user", "content": c})
}
fn assistant(c: &str) -> Value {
    json!({"role": "assistant", "content": c})
}
#[allow(dead_code)]
fn tool_result(call_id: &str, name: &str, content: &str) -> Value {
    json!({"role": "tool", "tool_call_id": call_id, "name": name, "content": content})
}
fn assistant_with_tool_call(call_id: &str, tool_name: &str) -> Value {
    json!({
        "role": "assistant",
        "content": "",
        "tool_calls": [{"id": call_id, "function": {"name": tool_name}}]
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// E2E: Full session lifecycle (happy path)
// ═══════════════════════════════════════════════════════════════════════════

/// Simulates a 10-turn coding session: build facts incrementally, persist L1
/// narrative to Memoria, compact with facts-first, then run session-end governance.
#[tokio::test]
#[ignore = "requires live Memoria"]
async fn e2e_full_session_lifecycle() {
    let mut ctx = require_memoria!();

    // ── Phase 1: Build SessionFacts over 10 turns ──
    let mut facts = SessionFacts::default();
    let turns = vec![
        make_event(
            1,
            vec![make_tc("read_file", true, Some("src/main.rs"))],
            2000,
            None,
        ),
        make_event(
            2,
            vec![make_tc("read_file", true, Some("src/lib.rs"))],
            2000,
            None,
        ),
        make_event(
            3,
            vec![
                make_tc("str_replace", true, Some("src/main.rs")),
                make_tc("read_file", true, Some("Cargo.toml")),
            ],
            3000,
            None,
        ),
        make_event(
            4,
            vec![make_tc("bash", false, None)],
            2000,
            Some("compile error: missing semicolon"),
        ),
        make_event(
            5,
            vec![make_tc("str_replace", true, Some("src/main.rs"))],
            2000,
            None,
        ),
        make_event(
            6,
            vec![make_tc("read_file", true, Some("tests/test.rs"))],
            2000,
            None,
        ),
        make_event(7, vec![make_tc("bash", true, None)], 2000, None),
        make_event(
            8,
            vec![make_tc("read_file", true, Some("src/auth.rs"))],
            3000,
            None,
        ),
        make_event(
            9,
            vec![make_tc("str_replace", true, Some("src/auth.rs"))],
            2000,
            None,
        ),
        make_event(10, vec![make_tc("bash", true, None)], 2000, None),
    ];
    for event in &turns {
        astra_turn_core::cloud_session_facts::update_from_journal_event(&mut facts, event);
    }
    facts.set_plan_state(Some(PlanFact {
        goal: "Add OAuth support".to_string(),
        completed: 7,
        total: 10,
        current_subtask: Some("token refresh".to_string()),
    }));
    facts.set_blocked_tools(vec!["web_fetch".to_string()]);

    // Verify facts state
    assert_eq!(facts.turn, 10);
    assert_eq!(facts.estimated_tokens, 22000);
    assert!(
        facts
            .active_files
            .iter()
            .any(|f| f.path == "src/main.rs" && f.last_action == "write")
    );
    assert!(
        facts
            .active_files
            .iter()
            .any(|f| f.path == "src/auth.rs" && f.last_action == "write")
    );
    assert_eq!(facts.error_state.total_errors, 1);
    assert!(
        facts
            .error_state
            .last_error
            .as_ref()
            .unwrap()
            .contains("semicolon")
    );

    // ── Phase 2: Store L1 narrative in Memoria ──
    let narrative_text = format!(
        "{}\n# Session Title\nOAuth Implementation\n\
         # Task Specification\nAdd OAuth support with JWT tokens to the API\n\
         # User Corrections\n- Use RS256 not HS256\n- Don't use rm -rf\n\
         # Learnings\n- CJK needs char_indices\n- floor_char_boundary for truncation\n- axum 0.8 changed Router API\n\
         # Decisions\n- Use axum framework\n- Use sqlx for database\n- JWT with RS256\n\
         # User Messages\nAdd OAuth support\nUse RS256\nFix the compile error",
        SESSION_MEMORY_PREFIX
    );
    let id = ctx
        .client
        .store(&narrative_text, "working", Some(&ctx.sid), Some("T2"))
        .await
        .unwrap();
    ctx.track(id);

    // ── Phase 3: Generate anchor from facts ──
    let narrative = SessionMemory::parse(&narrative_text).unwrap();
    let anchor = extract_anchor_from_facts("Add OAuth support", &facts, Some(&narrative));
    assert!(anchor.contains("Goal:"), "anchor: {anchor}");
    assert!(anchor.contains("7/10 subtasks"), "anchor: {anchor}");
    assert!(anchor.contains("token refresh"), "anchor: {anchor}");
    assert!(anchor.contains("Last error:"), "anchor: {anchor}");
    assert!(anchor.contains("Avoid: web_fetch"), "anchor: {anchor}");

    // ── Phase 4: Build facts-first injection ──
    let injection = build_facts_first_injection(&facts, Some(&narrative));
    assert!(injection.contains("# System State"));
    assert!(injection.contains("Turn 10"));
    assert!(injection.contains("src/auth.rs"));
    assert!(injection.contains("# Task\nAdd OAuth support"));
    assert!(injection.contains("# User Corrections"));
    assert!(injection.contains("RS256"));
    assert!(injection.contains("# Learnings"));
    assert!(injection.contains("# Last Decision"));
    // Cross-validation: plan NOT complete (7/10), so Task should NOT be skipped
    assert!(!injection.contains("⚠️"));

    // ── Phase 5: Compact with facts-first ──
    let messages = vec![
        json!({"role": "system", "content": "You are a helpful assistant."}),
        user("Add OAuth support with JWT tokens"),
        assistant("I'll implement OAuth with JWT."),
        user("Use RS256 not HS256"),
        assistant("Switching to RS256."),
        user("Fix the compile error"),
        assistant("Fixed the missing semicolon."),
        user("Now add token refresh"),
        assistant("Adding token refresh endpoint."),
    ];
    let config = MemoriaCompactConfig {
        min_tokens_for_retrieval: 100,
        store_on_compact: false,
        ..Default::default()
    };
    let params = MemoriaCompactParams {
        budget_chars: 4000,
        keep_chars: 2000,
        tier: CompactionTier::CompactHistory,
        keep_recent_turns: 2,
        current_tokens: 50000,
        session_memory_file: None,
        session_memory_combine: SessionMemoryFileCombine::None,
        session_facts: Some(facts.clone()),
    };
    let result = compact_with_memoria(
        &messages,
        Some(&ctx.sid),
        &config,
        &params,
        Some(&ctx.client),
        None,
        None,
    )
    .await;

    // Verify facts-first injection was used
    let has_system_state = result.messages.iter().any(|m| {
        m.get("content")
            .and_then(Value::as_str)
            .map(|c| c.contains("# System State") && c.contains("src/auth.rs"))
            .unwrap_or(false)
    });
    assert!(
        has_system_state,
        "compaction should inject facts-first context"
    );

    // ── Phase 6: Session-end governance ──
    let report = run_session_end_governance(&facts, Some(&narrative), &ctx.sid, &ctx.client)
        .await
        .unwrap();
    assert!(report.learnings_stored > 0, "should store learnings");

    // Verify knowledge was stored as semantic memory
    let knowledge = ctx
        .client
        .retrieve(
            &format!("[session-knowledge:{}]", ctx.sid),
            Some(&ctx.sid),
            5,
        )
        .await
        .unwrap();
    let found = knowledge
        .iter()
        .any(|m| m.content.contains("RS256") && m.memory_type == "semantic");
    assert!(found, "knowledge should be retrievable from Memoria");

    ctx.cleanup().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// E2E: Cross-validation triggers (plan complete + errors)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires live Memoria"]
async fn e2e_cross_validation_skips_contradicted_narrative() {
    let ctx = require_memoria!();

    let facts = SessionFacts {
        turn: 5,
        estimated_tokens: 20000,
        plan_state: Some(PlanFact {
            goal: "Build API".to_string(),
            completed: 3,
            total: 3,
            current_subtask: None,
        }),
        error_state: ErrorFact {
            total_errors: 2,
            last_error: Some("test failure in auth_test".to_string()),
            last_error_turn: Some(5),
        },
        ..Default::default()
    };

    let narrative_text = format!(
        "{}\n# Session Title\nAPI Build\n\
         # Task Specification\nBuild REST API — all endpoints implemented\n\
         # User Corrections\n- Use RS256 for JWT\n\
         # Learnings\n- Always run tests before committing",
        SESSION_MEMORY_PREFIX
    );
    let narrative = SessionMemory::parse(&narrative_text);

    let injection = build_facts_first_injection(&facts, narrative.as_ref());

    // Task should be SKIPPED (plan complete but errors exist)
    assert!(
        injection.contains("⚠️"),
        "should have cross-validation warning"
    );
    assert!(
        !injection.contains("# Task"),
        "contradicted Task should be omitted"
    );
    // But User Corrections should still be present
    assert!(
        injection.contains("# User Corrections"),
        "corrections must survive"
    );
    assert!(injection.contains("RS256"));
    // Facts should always be present
    assert!(injection.contains("Errors: 2 total"));
    assert!(injection.contains("test failure"));

    ctx.cleanup().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// E2E: Error-triggered extraction
// ═══════════════════════════════════════════════════════════════════════════

#[test]
#[ignore = "requires live Memoria"]
fn e2e_error_trigger_captures_corrections_before_compaction() {
    // Simulate: session at 15K tokens, last extraction at 12K, only 1 tool call since.
    // Normal threshold NOT met. But an error occurred → should trigger.
    let state = SessionMemoryState {
        initialized: true,
        tokens_at_last_extraction: 12_000,
        tool_calls_at_last_extraction: 8,
        last_extraction_time: None,
    };
    let config = SessionMemoryExtractConfig::default();

    // Without error: should NOT extract (insufficient growth)
    assert!(!should_extract_with_error_trigger(
        &state, 15_000, 9, &config, false
    ));

    // With error: SHOULD extract (captures user correction)
    assert!(should_extract_with_error_trigger(
        &state, 15_000, 9, &config, true
    ));

    // Below init gate: should NOT extract even with error
    let fresh_state = SessionMemoryState::default();
    assert!(!should_extract_with_error_trigger(
        &fresh_state,
        5_000,
        0,
        &config,
        true
    ));
}

// ═══════════════════════════════════════════════════════════════════════════
// E2E: State-aware microcompact preserves active files
// ═══════════════════════════════════════════════════════════════════════════

#[test]
#[ignore = "requires live Memoria"]
fn e2e_microcompact_pins_active_files_under_pressure() {
    let mut facts = SessionFacts {
        turn: 20,
        ..Default::default()
    };
    // src/auth.rs is actively being worked on (turn 19)
    facts.active_files.push(FileEntry {
        path: "src/auth.rs".to_string(),
        last_action: "write".to_string(),
        turn: 19,
    });
    // src/old.rs was read long ago (turn 2)
    facts.active_files.push(FileEntry {
        path: "src/old.rs".to_string(),
        last_action: "read".to_string(),
        turn: 2,
    });

    // Build 10 read_file results
    let mut messages: Vec<Value> = Vec::new();
    let files = [
        "src/auth.rs",
        "src/a.rs",
        "src/b.rs",
        "src/c.rs",
        "src/d.rs",
        "src/e.rs",
        "src/f.rs",
        "src/g.rs",
        "src/old.rs",
        "src/h.rs",
    ];
    for (i, file) in files.iter().enumerate() {
        let cid = format!("c{i}");
        messages.push(assistant_with_tool_call(&cid, "read_file"));
        messages.push(json!({
            "role": "tool", "tool_call_id": cid, "name": "read_file",
            "content": format!("{file}\n{}", "x".repeat(3000))
        }));
    }

    // High pressure compaction
    let stats =
        compact_tool_results_state_aware(&mut messages, 0.85, &facts, 5, Default::default());
    assert!(stats.results_compacted > 0, "should compact some results");

    // src/auth.rs (turn 19, within 5-turn window) should be PINNED
    let auth_content = messages
        .iter()
        .find(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|c| c.starts_with("src/auth.rs"))
                .unwrap_or(false)
        })
        .and_then(|m| m.get("content").and_then(Value::as_str));
    assert!(auth_content.is_some(), "auth.rs should still exist");
    assert!(
        !is_cleared_content(auth_content.unwrap()),
        "auth.rs should NOT be cleared"
    );

    // src/old.rs (turn 2, outside 5-turn window from turn 20) should be eligible for compaction
    // It may or may not be compacted depending on count, but it's NOT pinned
}

// ═══════════════════════════════════════════════════════════════════════════
// Unhappy paths
// ═══════════════════════════════════════════════════════════════════════════

/// Memoria is down during compaction — should fall back to truncation, not crash.
#[tokio::test]
#[ignore = "requires live Memoria"]
async fn unhappy_memoria_down_during_compaction() {
    // Use a client pointing to a non-existent URL
    let bad_client = HttpMemoriaClient::new(
        "http://localhost:1/nonexistent".to_string(),
        "bad-key".to_string(),
    );
    let mut facts = SessionFacts {
        turn: 5,
        ..Default::default()
    };
    facts.active_files.push(FileEntry {
        path: "src/main.rs".to_string(),
        last_action: "write".to_string(),
        turn: 5,
    });

    let messages = vec![
        json!({"role": "system", "content": "assistant"}),
        user("hello"),
        assistant("hi"),
    ];
    let config = MemoriaCompactConfig {
        min_tokens_for_retrieval: 100,
        store_on_compact: false,
        ..Default::default()
    };
    let params = MemoriaCompactParams {
        budget_chars: 4000,
        keep_chars: 2000,
        tier: CompactionTier::CompactHistory,
        keep_recent_turns: 2,
        current_tokens: 50000,
        session_memory_file: None,
        session_memory_combine: SessionMemoryFileCombine::None,
        session_facts: Some(facts),
    };

    // Should NOT panic — falls back gracefully
    let result = compact_with_memoria(
        &messages,
        Some("bad-session"),
        &config,
        &params,
        Some(&bad_client),
        None,
        None,
    )
    .await;

    // Should still have messages (truncation fallback)
    assert!(
        !result.messages.is_empty(),
        "should produce output even with Memoria down"
    );
    // Facts-first injection should still work (doesn't need Memoria)
    let has_facts = result.messages.iter().any(|m| {
        m.get("content")
            .and_then(Value::as_str)
            .map(|c| c.contains("# System State"))
            .unwrap_or(false)
    });
    assert!(
        has_facts,
        "facts injection should work even when Memoria is down"
    );
}

/// Session-end governance with Memoria down — should not crash, report 0.
#[tokio::test]
#[ignore = "requires live Memoria"]
async fn unhappy_session_end_memoria_down() {
    let bad_client = HttpMemoriaClient::new(
        "http://localhost:1/nonexistent".to_string(),
        "bad-key".to_string(),
    );
    let facts = SessionFacts::default();
    let narrative_text = format!(
        "{}\n# Session Title\nTest\n# Task Specification\nTest task\n\
         # User Corrections\n- Use RS256\n# Learnings\n- CJK handling",
        SESSION_MEMORY_PREFIX
    );
    let narrative = SessionMemory::parse(&narrative_text);

    // Should NOT panic
    let report =
        run_session_end_governance(&facts, narrative.as_ref(), "bad-session", &bad_client).await;

    // Should return Ok but with 0 stored (store failed)
    assert!(report.is_ok());
    let report = report.unwrap();
    assert_eq!(report.learnings_stored, 0);
    assert_eq!(report.working_purged, 0);
}

/// Empty session — no facts, no narrative, no crash.
#[tokio::test]
#[ignore = "requires live Memoria"]
async fn unhappy_empty_session_compaction() {
    let ctx = require_memoria!();

    let facts = SessionFacts::default(); // completely empty
    let messages = vec![
        json!({"role": "system", "content": "assistant"}),
        user("hello"),
        assistant("hi"),
    ];
    let config = MemoriaCompactConfig {
        min_tokens_for_retrieval: 100,
        store_on_compact: false,
        ..Default::default()
    };
    let params = MemoriaCompactParams {
        budget_chars: 4000,
        keep_chars: 2000,
        tier: CompactionTier::CompactHistory,
        keep_recent_turns: 2,
        current_tokens: 50000,
        session_memory_file: None,
        session_memory_combine: SessionMemoryFileCombine::None,
        session_facts: Some(facts.clone()),
    };

    let result = compact_with_memoria(
        &messages,
        Some(&ctx.sid),
        &config,
        &params,
        Some(&ctx.client),
        None,
        None,
    )
    .await;

    assert!(!result.messages.is_empty());
    // Empty facts should still produce a System State section
    let has_state = result.messages.iter().any(|m| {
        m.get("content")
            .and_then(Value::as_str)
            .map(|c| c.contains("# System State") && c.contains("Turn 0"))
            .unwrap_or(false)
    });
    assert!(has_state, "empty facts should still inject System State");

    // Session-end with empty session
    let report = run_session_end_governance(&facts, None, &ctx.sid, &ctx.client)
        .await
        .unwrap();
    assert_eq!(
        report.learnings_stored, 0,
        "empty session has nothing to store"
    );

    ctx.cleanup().await;
}

/// Malformed narrative in Memoria — should not crash, use facts only.
#[tokio::test]
#[ignore = "requires live Memoria"]
async fn unhappy_malformed_narrative_in_memoria() {
    let mut ctx = require_memoria!();

    // Store garbage as "session memory"
    let garbage = format!(
        "{}\nThis is not valid markdown sections at all!!!",
        SESSION_MEMORY_PREFIX
    );
    let id = ctx
        .client
        .store(&garbage, "working", Some(&ctx.sid), Some("T2"))
        .await
        .unwrap();
    ctx.track(id);

    let facts = SessionFacts {
        turn: 3,
        estimated_tokens: 15000,
        active_files: vec![FileEntry {
            path: "src/main.rs".to_string(),
            last_action: "write".to_string(),
            turn: 3,
        }],
        ..Default::default()
    };

    let messages = vec![
        json!({"role": "system", "content": "assistant"}),
        user("build something"),
        assistant("ok"),
    ];
    let config = MemoriaCompactConfig {
        min_tokens_for_retrieval: 100,
        store_on_compact: false,
        ..Default::default()
    };
    let params = MemoriaCompactParams {
        budget_chars: 8000,
        keep_chars: 4000,
        tier: CompactionTier::CompactHistory,
        keep_recent_turns: 2,
        current_tokens: 50000,
        session_memory_file: None,
        session_memory_combine: SessionMemoryFileCombine::None,
        session_facts: Some(facts),
    };

    let result = compact_with_memoria(
        &messages,
        Some(&ctx.sid),
        &config,
        &params,
        Some(&ctx.client),
        None,
        None,
    )
    .await;

    // Should not crash, should still have facts
    assert!(!result.messages.is_empty());
    let has_facts = result.messages.iter().any(|m| {
        m.get("content")
            .and_then(Value::as_str)
            .map(|c| c.contains("# System State") && c.contains("src/main.rs"))
            .unwrap_or(false)
    });
    assert!(
        has_facts,
        "facts should be injected even with malformed narrative"
    );

    ctx.cleanup().await;
}

/// ToolCallRecord without file_path field (backward compat with old journals).
#[test]
#[ignore = "requires live Memoria"]
fn unhappy_old_journal_without_file_path() {
    // Simulate deserializing an old journal entry without file_path
    let old_json = r#"{
        "name": "read_file", "ok": true, "ms": 100,
        "args_preview": "{\"path\":\"src/lib.rs\"}"
    }"#;
    let tc: ToolCallRecord = serde_json::from_str(old_json).unwrap();
    assert!(
        tc.file_path.is_none(),
        "old records should deserialize with file_path=None"
    );

    // SessionFacts should still extract path from args_preview fallback
    let mut facts = SessionFacts::default();
    let mut event = JournalEvent::base_public(JournalEventType::Turn, None);
    event.turn = Some(1);
    event.tokens_in = Some(1000);
    event.tool_calls = Some(vec![tc]);
    astra_turn_core::cloud_session_facts::update_from_journal_event(&mut facts, &event);

    assert_eq!(facts.active_files.len(), 1);
    assert_eq!(facts.active_files[0].path, "src/lib.rs");
}

/// SessionFacts serialization round-trip (for checkpoint persistence).
#[test]
#[ignore = "requires live Memoria"]
fn e2e_session_facts_serde_roundtrip() {
    let mut facts = SessionFacts {
        turn: 15,
        estimated_tokens: 50000,
        ..Default::default()
    };
    facts.active_files.push(FileEntry {
        path: "src/main.rs".to_string(),
        last_action: "write".to_string(),
        turn: 14,
    });
    facts.plan_state = Some(PlanFact {
        goal: "Build OAuth".to_string(),
        completed: 5,
        total: 8,
        current_subtask: Some("refresh token".to_string()),
    });
    facts.error_state = ErrorFact {
        total_errors: 2,
        last_error: Some("compile error".to_string()),
        last_error_turn: Some(10),
    };
    facts.blocked_tools = vec!["web_fetch".to_string()];

    let json = serde_json::to_string(&facts).unwrap();
    let restored: SessionFacts = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.turn, 15);
    assert_eq!(restored.estimated_tokens, 50000);
    assert_eq!(restored.active_files.len(), 1);
    assert_eq!(restored.active_files[0].path, "src/main.rs");
    assert_eq!(restored.plan_state.as_ref().unwrap().completed, 5);
    assert_eq!(restored.error_state.total_errors, 2);
    assert_eq!(restored.blocked_tools, vec!["web_fetch"]);
}

/// Concurrent facts updates don't lose data.
#[test]
#[ignore = "requires live Memoria"]
fn e2e_rapid_facts_updates_no_data_loss() {
    let mut facts = SessionFacts::default();

    // Simulate 50 rapid turns with various tools
    for i in 1..=50 {
        let tcs = vec![
            make_tc("read_file", true, Some(&format!("file_{}.rs", i % 25))),
            make_tc("bash", i % 5 != 0, None), // every 5th bash fails
        ];
        let error = if i % 5 == 0 {
            Some("bash failed")
        } else {
            None
        };
        let event = make_event(i, tcs, 1000, error);
        astra_turn_core::cloud_session_facts::update_from_journal_event(&mut facts, &event);
    }

    assert_eq!(facts.turn, 50);
    assert_eq!(facts.estimated_tokens, 50000);
    // Should have at most MAX_ACTIVE_FILES (20)
    assert!(facts.active_files.len() <= 20);
    // Should have at most MAX_RECENT_TOOLS (10)
    assert!(facts.recent_tool_calls.len() <= 10);
    // Should have counted all errors
    assert_eq!(facts.error_state.total_errors, 10); // every 5th of 50
    assert_eq!(facts.error_state.last_error_turn, Some(50));
}
