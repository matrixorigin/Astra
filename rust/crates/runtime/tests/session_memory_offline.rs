//! Offline cross-module integration coverage for the session memory protocol.
//!
//! Complements the `#[ignore]`-gated live-Memoria tests in `session_facts_e2e.rs`
//! by exercising the same protocol modules (session_facts → session_memory_protocol
//! → memoria_compact → session_end_governance) against a deterministic
//! [`RecordingFake`] that implements the [`MemoriaClient`] trait in-process.
//!
//! These tests focus on *composition* — each individual module already has solid
//! unit coverage; the gap is verifying the pieces glue together across a
//! realistic lifecycle.

use astra_runtime::prompts::CompactionTier;
use astra_runtime::turn::cloud::memoria_compact::{
    MemoriaClient, MemoriaCompactConfig, MemoriaCompactParams, MemoriaMemory,
    SessionMemoryFileCombine, compact_with_memoria,
};
use astra_runtime::turn::cloud::session_end_governance::run_session_end_governance;
use astra_runtime::turn::cloud::session_facts::SessionFacts;
use astra_runtime::turn::cloud::session_memory_protocol::{
    SessionMemory, build_l1_from_messages, persist_l1,
};
use astra_services::session_journal::{JournalEvent, JournalEventType, ToolCallRecord};
use serde_json::{Value, json};
use std::sync::Mutex;

// ── Fake MemoriaClient ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)] // Variant payloads used via matches! patterns.
enum Op {
    Retrieve {
        query: String,
        session_id: Option<String>,
        filter_session: bool,
    },
    Store {
        content: String,
        memory_type: String,
        session_id: Option<String>,
        trust_tier: Option<String>,
    },
    Purge {
        session_id: String,
    },
    Delete {
        memory_id: String,
    },
}

#[derive(Default)]
struct RecordingFake {
    ops: Mutex<Vec<Op>>,
    /// Memories returned by `retrieve_ext`.
    memories: Mutex<Vec<MemoriaMemory>>,
    /// Store responses from front (pop_front). When empty, stores succeed with
    /// auto-generated id.
    store_responses: Mutex<std::collections::VecDeque<Result<String, String>>>,
    /// How many working memories `purge_working` should report.
    purge_count: Mutex<u64>,
}

impl RecordingFake {
    fn new() -> Self {
        Self::default()
    }

    fn with_memories(self, memories: Vec<MemoriaMemory>) -> Self {
        *self.memories.lock().unwrap() = memories;
        self
    }

    fn with_purge_count(self, n: u64) -> Self {
        *self.purge_count.lock().unwrap() = n;
        self
    }

    fn push_store_err(&self, err: &str) {
        self.store_responses
            .lock()
            .unwrap()
            .push_back(Err(err.to_string()));
    }

    fn ops(&self) -> Vec<Op> {
        self.ops.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl MemoriaClient for RecordingFake {
    async fn retrieve_ext(
        &self,
        query: &str,
        session_id: Option<&str>,
        _top_k: usize,
        filter_session: bool,
    ) -> Result<Vec<MemoriaMemory>, String> {
        self.ops.lock().unwrap().push(Op::Retrieve {
            query: query.to_string(),
            session_id: session_id.map(str::to_string),
            filter_session,
        });
        Ok(self.memories.lock().unwrap().clone())
    }

    async fn store(
        &self,
        content: &str,
        memory_type: &str,
        session_id: Option<&str>,
        trust_tier: Option<&str>,
    ) -> Result<String, String> {
        self.ops.lock().unwrap().push(Op::Store {
            content: content.to_string(),
            memory_type: memory_type.to_string(),
            session_id: session_id.map(str::to_string),
            trust_tier: trust_tier.map(str::to_string),
        });
        if let Some(r) = self.store_responses.lock().unwrap().pop_front() {
            return r;
        }
        let id = format!(
            "mem-{}",
            self.ops
                .lock()
                .unwrap()
                .iter()
                .filter(|o| matches!(o, Op::Store { .. }))
                .count()
        );
        Ok(id)
    }

    async fn purge_working(&self, session_id: &str) -> Result<u64, String> {
        self.ops.lock().unwrap().push(Op::Purge {
            session_id: session_id.to_string(),
        });
        Ok(*self.purge_count.lock().unwrap())
    }

    async fn delete(&self, memory_id: &str) -> Result<(), String> {
        self.ops.lock().unwrap().push(Op::Delete {
            memory_id: memory_id.to_string(),
        });
        Ok(())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn user_msg(c: &str) -> Value {
    json!({"role": "user", "content": c})
}

fn assistant_msg(c: &str) -> Value {
    json!({"role": "assistant", "content": c})
}

fn tc(name: &str, ok: bool, file_path: Option<&str>) -> ToolCallRecord {
    ToolCallRecord {
        name: name.to_string(),
        ok,
        ms: 10,
        error: None,
        input_bytes: None,
        output_bytes: None,
        args_preview: None,
        result_preview: None,
        file_path: file_path.map(str::to_string),
        surgically_removed: None,
        original_tool_name: None,
        ..Default::default()
    }
}

fn turn_event(turn: u32, tool_calls: Vec<ToolCallRecord>, tokens: u64) -> JournalEvent {
    let mut e = JournalEvent::base_public(JournalEventType::Turn, None);
    e.turn = Some(turn);
    e.tokens_in = Some(tokens);
    e.tool_calls = Some(tool_calls);
    e
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// End-to-end lifecycle: facts accumulate → L1 built & persisted → compaction
/// retrieves the same L1 → session-end governance stores knowledge and purges.
///
/// Verifies the modules compose correctly across a realistic session. Each
/// module has its own unit tests; this guarantees they glue together.
#[tokio::test]
async fn full_session_memory_lifecycle_glues_modules() {
    let session_id = "offline-lifecycle-1";
    let fake = RecordingFake::new().with_purge_count(3);

    // Phase 1: 3-turn conversation → build SessionFacts from journal events.
    let mut facts = SessionFacts::default();
    facts.update_from_journal_event(&turn_event(
        1,
        vec![tc("read_file", true, Some("src/auth.rs"))],
        2_000,
    ));
    facts.update_from_journal_event(&turn_event(
        2,
        vec![tc("str_replace", true, Some("src/auth.rs"))],
        2_200,
    ));
    facts.update_from_journal_event(&turn_event(3, vec![tc("run_tests", false, None)], 1_800));
    assert_eq!(facts.turn, 3);
    assert_eq!(facts.active_files.len(), 1, "single file touched twice");
    assert!(facts.recent_tool_calls.iter().any(|t| !t.ok));

    // Phase 2: Build L1 narrative from conversation + persist.
    let messages = vec![
        user_msg("implement OAuth for the API"),
        assistant_msg("I'll scaffold an OAuth2 flow"),
        user_msg("prefer PKCE"),
        assistant_msg("switching to PKCE"),
    ];
    let l1 = build_l1_from_messages(&messages, 3, 6_000);
    assert!(!l1.is_empty());
    persist_l1(&fake, &l1, session_id)
        .await
        .expect("persist_l1 should succeed against fake");

    // After persist: we expect exactly [purge, store(working, T2)] ordering.
    let after_persist = fake.ops();
    assert!(
        matches!(after_persist.first(), Some(Op::Purge { session_id: s }) if s == session_id),
        "persist_l1 should purge first: {after_persist:?}"
    );
    assert!(
        matches!(
            after_persist.get(1),
            Some(Op::Store { memory_type, trust_tier, session_id: Some(s), .. })
                if memory_type == "working" && trust_tier.as_deref() == Some("T2") && s == session_id
        ),
        "persist_l1 should store working memory with T2 tier: {after_persist:?}"
    );

    // Phase 3: Compaction under pressure — the fake now returns the L1 narrative
    //          we just stored, simulating retrieval in a follow-up session turn.
    let narrative_memory = MemoriaMemory {
        memory_id: "mem-1".into(),
        content: l1.clone(),
        memory_type: "working".into(),
        retrieval_score: Some(0.9),
    };
    let compact_fake = RecordingFake::new().with_memories(vec![narrative_memory]);

    let config = MemoriaCompactConfig {
        min_tokens_for_retrieval: 1_000,
        max_memories: 5,
        max_memory_tokens: 2_000,
        store_on_compact: false,
    };
    let params = MemoriaCompactParams {
        budget_chars: 10_000,
        keep_chars: 2_000,
        tier: CompactionTier::CompactHistory,
        keep_recent_turns: 2,
        current_tokens: 8_000,
        session_memory_file: None,
        session_memory_combine: SessionMemoryFileCombine::None,
        session_facts: Some(facts.clone()),
    };
    let result = compact_with_memoria(
        &messages,
        Some(session_id),
        &config,
        &params,
        Some(&compact_fake),
        None,
        None,
    )
    .await;

    // Retrieval path fired with session-scoped filter.
    let compact_ops = compact_fake.ops();
    assert!(
        compact_ops.iter().any(|o| matches!(
            o,
            Op::Retrieve {
                session_id: Some(s),
                filter_session: true,
                ..
            } if s == session_id
        )),
        "compact_with_memoria should retrieve with filter_session=true: {compact_ops:?}"
    );

    // Facts-first injection surfaces active file and error state.
    let injected_system = result
        .messages
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("system"))
        .expect("facts-first injection should insert a system message");
    let injected = injected_system
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        injected.contains("src/auth.rs"),
        "injection should surface active files: {injected}"
    );

    // Phase 4: Session-end governance — extract learnings + purge.
    let narrative = SessionMemory::parse(&l1);
    // Augment narrative with a User Corrections section so governance finds
    // something worth persisting (build_l1_from_messages omits this by default).
    let narrative_with_correction = {
        let mut raw = l1.clone();
        raw.push_str("\n# User Corrections\n- prefer PKCE over implicit flow\n");
        SessionMemory::parse(&raw)
    };
    assert!(
        narrative.is_some() || narrative_with_correction.is_some(),
        "L1 should be parseable as SessionMemory"
    );

    let govern_fake = RecordingFake::new().with_purge_count(5);
    let report = run_session_end_governance(
        &facts,
        narrative_with_correction.as_ref(),
        session_id,
        &govern_fake,
    )
    .await
    .expect("governance should succeed against fake");
    assert!(
        report.learnings_stored >= 1,
        "should record at least the PKCE correction"
    );
    assert_eq!(report.working_purged, 5, "report reflects fake purge count");

    let govern_ops = govern_fake.ops();
    // One semantic store, one purge.
    assert!(
        govern_ops.iter().any(|o| matches!(
            o,
            Op::Store { memory_type, session_id: Some(s), .. }
                if memory_type == "semantic" && s == session_id
        )),
        "governance should store semantic memory: {govern_ops:?}"
    );
    assert!(
        govern_ops
            .iter()
            .any(|o| matches!(o, Op::Purge { session_id: s } if s == session_id)),
        "governance should purge working memory: {govern_ops:?}"
    );
}

/// Governance must still purge working memory even when the semantic store call
/// fails — working-memory cleanup is the safety-net half of session-end and
/// cannot be starved by a transient storage error.
#[tokio::test]
async fn session_end_governance_purges_even_when_store_fails() {
    let session_id = "offline-govern-storefail";
    let fake = RecordingFake::new().with_purge_count(2);
    // Make the semantic store fail, but leave purge healthy.
    fake.push_store_err("simulated memoria unavailable");

    // Narrative with corrections so format_knowledge_for_storage emits content
    // and triggers the store attempt.
    let narrative_raw =
        "[session-memory:v1]\n# User Corrections\n- avoid implicit OAuth flow\n".to_string();
    let narrative = SessionMemory::parse(&narrative_raw).expect("narrative should parse");
    let facts = SessionFacts::default();

    let report = run_session_end_governance(&facts, Some(&narrative), session_id, &fake)
        .await
        .expect("governance returns Ok even when store fails");

    assert_eq!(
        report.learnings_stored, 0,
        "failed store should leave learnings_stored at 0"
    );
    assert_eq!(
        report.working_purged, 2,
        "purge must still run despite store failure"
    );

    let ops = fake.ops();
    let store_count = ops.iter().filter(|o| matches!(o, Op::Store { .. })).count();
    let purge_count = ops.iter().filter(|o| matches!(o, Op::Purge { .. })).count();
    assert_eq!(store_count, 1, "store attempted exactly once");
    assert_eq!(purge_count, 1, "purge attempted exactly once");
}

/// When Memoria retrieval fails mid-compaction the system MUST degrade to
/// tier-based truncation without panicking or losing messages — this is the
/// single most important unhappy-path guarantee for session memory.
#[tokio::test]
async fn compaction_degrades_gracefully_when_retrieve_errors() {
    struct ErrFake;

    #[async_trait::async_trait]
    impl MemoriaClient for ErrFake {
        async fn retrieve_ext(
            &self,
            _: &str,
            _: Option<&str>,
            _: usize,
            _: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            Err("memoria: 503 service unavailable".into())
        }
        async fn store(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<String, String> {
            Err("memoria: 503 service unavailable".into())
        }
        async fn purge_working(&self, _: &str) -> Result<u64, String> {
            Ok(0)
        }
    }

    let messages: Vec<Value> = (0..20)
        .map(|i| {
            if i % 2 == 0 {
                user_msg(&format!("user says {i}"))
            } else {
                assistant_msg(&format!("assistant replies {i}"))
            }
        })
        .collect();

    let config = MemoriaCompactConfig {
        min_tokens_for_retrieval: 1_000,
        store_on_compact: true,
        ..Default::default()
    };
    let params = MemoriaCompactParams {
        budget_chars: 2_000,
        keep_chars: 500,
        tier: CompactionTier::CompactHistory,
        keep_recent_turns: 2,
        current_tokens: 8_000,
        session_memory_file: None,
        session_memory_combine: SessionMemoryFileCombine::None,
        session_facts: None,
    };

    let result = compact_with_memoria(
        &messages,
        Some("degraded-sess"),
        &config,
        &params,
        Some(&ErrFake),
        None,
        None,
    )
    .await;

    // Must not panic, must not return zero messages: truncation fallback still runs.
    assert!(
        !result.messages.is_empty(),
        "compaction must preserve messages despite Memoria retrieve failure"
    );
    // No system message should have been injected since retrieval failed.
    let had_system_injection = result
        .messages
        .iter()
        .any(|m| m.get("role").and_then(Value::as_str) == Some("system"));
    assert!(
        !had_system_injection,
        "failed retrieve should skip memory_context injection"
    );
}
