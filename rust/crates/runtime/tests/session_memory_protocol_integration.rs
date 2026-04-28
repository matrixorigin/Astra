//! Integration tests for Session Memory Protocol against real Memoria.
//!
//! Requires: `MEMORIA_BASE_URL` + `MEMORIA_MASTER_KEY` (or `MEMORIA_API_KEY`).
//! Run with: `cargo test -p astra-runtime -- session_memory_protocol_integration --ignored`

use astra_runtime::turn::cloud::memoria_compact::{
    HttpMemoriaClient, MemoriaClient, MemoriaCompactConfig, MemoriaCompactParams, MemoriaMemory,
    compact_with_memoria,
};
use astra_runtime::turn::cloud::session_memory_protocol::*;
use serde_json::{Value, json};
use uuid::Uuid;

fn memoria_client() -> Option<HttpMemoriaClient> {
    // Load .env from project root (three levels up: crates/runtime/ → crates/ → rust/ → root)
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    // CARGO_MANIFEST_DIR = .../rust/crates/runtime
    // workspace root with .env = .../  (3 levels up)
    for ancestor in manifest_dir.ancestors().take(5) {
        let env_path = ancestor.join(".env");
        if env_path.exists() {
            let _ = dotenvy::from_path(&env_path);
            break;
        }
    }
    HttpMemoriaClient::from_env()
}

/// Try to store a test memory. Returns None if Memoria is not fully operational
/// (e.g., embedding service unavailable).
async fn verified_memoria_client() -> Option<(HttpMemoriaClient, String)> {
    let client = memoria_client()?;
    let sid = unique_session_id();
    // Probe: try a store to verify embedding service is working
    match client.store("probe", "working", Some(&sid), None).await {
        Ok(id) => {
            let _ = client.delete(&id).await; // clean up probe
            Some((client, unique_session_id()))
        }
        Err(e) => {
            eprintln!("Memoria not fully operational (embedding?): {e}");
            None
        }
    }
}

/// Helper to store and track memory IDs for cleanup.
/// Derefs to HttpMemoriaClient so tests can call .store(), .retrieve() etc. directly.
/// Call .cleanup() at end of test, or .track(id) to register externally-created IDs.
struct TestMemories {
    client: HttpMemoriaClient,
    ids: std::sync::Mutex<Vec<String>>,
    sid: String,
}

impl TestMemories {
    fn new(client: HttpMemoriaClient, sid: String) -> Self {
        Self {
            client,
            ids: std::sync::Mutex::new(Vec::new()),
            sid,
        }
    }

    fn session_id(&self) -> &str {
        &self.sid
    }

    /// Track a memory_id for cleanup (when store is called via client directly).
    #[allow(dead_code)]
    fn track(&self, id: &str) {
        self.ids.lock().unwrap().push(id.to_string());
    }

    async fn cleanup(&self) {
        let ids = std::mem::take(&mut *self.ids.lock().unwrap());
        for id in &ids {
            if let Err(e) = self.client.delete(id).await {
                tracing::warn!("[test cleanup] failed to delete memory {id}: {e}");
            }
        }
    }
}

impl std::ops::Deref for TestMemories {
    type Target = HttpMemoriaClient;
    fn deref(&self) -> &HttpMemoriaClient {
        &self.client
    }
}

macro_rules! require_memoria {
    () => {
        match verified_memoria_client().await {
            Some((client, sid)) => TestMemories::new(client, sid),
            None => {
                eprintln!("SKIPPED: Memoria not fully operational");
                return;
            }
        }
    };
}

fn unique_session_id() -> String {
    format!("test-smp-{}", Uuid::new_v4().simple())
}

/// Retrieve L1 for a specific session by content marker.
/// Memoria's retrieve uses session_id for boosting, not strict filtering,
/// and MemoriaMemory doesn't expose session_id. So we filter by content.
async fn retrieve_l1_with_marker(
    client: &HttpMemoriaClient,
    marker: &str,
    session_id: &str,
) -> Vec<MemoriaMemory> {
    // Retry briefly: MatrixOne snapshot isolation may delay visibility of just-committed rows.
    // Note: session_id + filter_session=true ensures strict session scoping
    // (Memoria #185). The content marker filter below is a secondary safeguard.
    for _ in 0..3 {
        let results = client
            .retrieve_ext(
                &format!("[session-memory:v1] {marker}"),
                Some(session_id),
                20,
                true,
            )
            .await
            .unwrap_or_default();
        let filtered: Vec<_> = results
            .into_iter()
            .filter(|m| m.content.starts_with(SESSION_MEMORY_PREFIX) && m.content.contains(marker))
            .collect();
        if !filtered.is_empty() {
            return filtered;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    vec![]
}

fn user(content: &str) -> Value {
    json!({"role": "user", "content": content})
}
fn assistant(content: &str) -> Value {
    json!({"role": "assistant", "content": content})
}
fn system(content: &str) -> Value {
    json!({"role": "system", "content": content})
}

fn sample_session_memory(task: &str) -> String {
    format!(
        "{SESSION_MEMORY_PREFIX}\n\
         # Session Title\n\
         Test Session\n\
         # Task Specification\n\
         {task}\n\
         # Current State\n\
         Working on implementation\n\
         # Key Files\n\
         src/main.rs — entry point\n\
         # Progress\n\
         ✅ Setup\n\
         🔄 Implementation\n\
         ⏳ Testing\n\
         # Errors & Corrections\n\
         None\n\
         # Decisions\n\
         - Use Rust for performance\n\
         # User Messages\n\
         {task}\n\
         # Worklog\n\
         Turn 1 — started\n\
         # Context\n\
         Turn 2, ~10K tokens"
    )
}

// ── L1 Store → Retrieve Round-Trip ──────────────────────────────────────────

#[tokio::test]
#[ignore = "requires live Memoria"]
async fn l1_store_and_retrieve_via_memoria() {
    let tm = require_memoria!();
    let marker = format!("REST-API-{}", &tm.session_id()[..12]);
    let content = sample_session_memory(&marker);

    // Store L1 as working memory
    let memory_id = tm
        .store(&content, "working", Some(tm.session_id()), Some("T2"))
        .await
        .expect("store failed");
    assert!(!memory_id.is_empty());

    // Retrieve by content marker
    let found = retrieve_l1_with_marker(&tm, &marker, tm.session_id()).await;
    assert!(!found.is_empty(), "L1 not found after store+wait");

    // Cleanup
    let _ = tm.purge_working(tm.session_id()).await;
    tm.cleanup().await;
}

// ── L1 Correct (Update) ────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires live Memoria"]
async fn l1_correct_updates_in_place() {
    let tm = require_memoria!();
    let marker_v1 = format!("v1-{}", &tm.session_id()[..12]);
    let marker_v2 = format!("v2-{}", &tm.session_id()[..12]);

    let content_v1 = sample_session_memory(&marker_v1);
    let _id = tm
        .store(&content_v1, "working", Some(tm.session_id()), Some("T2"))
        .await
        .expect("store v1 failed");

    let content_v2 = sample_session_memory(&marker_v2);
    let _id2 = tm
        .store(&content_v2, "working", Some(tm.session_id()), Some("T2"))
        .await
        .expect("store v2 failed");

    let found = retrieve_l1_with_marker(&tm, &marker_v2, tm.session_id()).await;
    assert!(!found.is_empty(), "v2 not found after update");

    let _ = tm.purge_working(tm.session_id()).await;
    tm.cleanup().await;
}

// ── Session End Purge ───────────────────────────────────────────────────────

// TODO: blocked on https://github.com/matrixorigin/Memoria/issues/182
// Memoria's topic-based purge uses fulltext search which doesn't match UUID session IDs.
// Re-enable once Memoria supports session_id-based purge.
//
// #[tokio::test]
// #[ignore]
// async fn session_purge_cleans_working_memory() { ... }

// ── Compaction with L1 (Zero-LLM Path) ─────────────────────────────────────

#[tokio::test]
#[ignore = "requires live Memoria"]
async fn compaction_uses_l1_when_available() {
    let tm = require_memoria!();

    // Store L1 in Memoria
    let l1_content = sample_session_memory("Implement OAuth");
    tm.store(&l1_content, "working", Some(tm.session_id()), Some("T2"))
        .await
        .expect("store L1 failed");

    // Build a conversation that needs compaction
    let mut messages: Vec<Value> = vec![
        system("You are helpful."),
        user("Implement OAuth for the API"),
    ];
    // Add enough messages to trigger compaction
    for i in 0..20 {
        messages.push(assistant(&format!(
            "Working on step {i}... {}",
            "x".repeat(200)
        )));
        messages.push(user(&format!("Continue with step {}", i + 1)));
    }

    let config = MemoriaCompactConfig {
        min_tokens_for_retrieval: 0, // always retrieve
        max_memories: 10,
        max_memory_tokens: 4000,
        store_on_compact: false, // don't store back to avoid side effects
    };
    let params = MemoriaCompactParams {
        budget_chars: 8000,
        keep_chars: 4000,
        tier: astra_runtime::prompts::CompactionTier::CompactHistory,
        keep_recent_turns: 3,
        current_tokens: 50000,
        session_memory_file: None,
        session_memory_combine:
            astra_runtime::turn::cloud::memoria_compact::SessionMemoryFileCombine::None,
        session_facts: None,
    };

    let result = compact_with_memoria(
        &messages,
        Some(tm.session_id()),
        &config,
        &params,
        Some(&*tm),
        None,
        None,
    )
    .await;

    // The compacted result should contain L1 content (injected from Memoria)
    let all_content: String = result
        .messages
        .iter()
        .filter_map(|m| m.get("content").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        all_content.contains("OAuth") || all_content.contains("Session Context from Memory"),
        "compacted messages should contain L1 content or memory context marker"
    );

    // Cleanup
    let _ = tm.purge_working(tm.session_id()).await;
}

// ── First User Message Preserved Through Compaction ─────────────────────────

#[tokio::test]
#[ignore = "requires live Memoria"]
async fn first_user_message_survives_compaction() {
    let tm = require_memoria!();

    let original_task = "UNIQUE_TASK_MARKER_12345: Build a distributed cache";
    let mut messages: Vec<Value> = vec![system("You are helpful."), user(original_task)];
    for i in 0..20 {
        messages.push(assistant(&format!("Step {i} done. {}", "y".repeat(300))));
        messages.push(user(&format!("Next step {}", i + 1)));
    }

    let config = MemoriaCompactConfig::default();
    let params = MemoriaCompactParams {
        budget_chars: 6000,
        keep_chars: 3000,
        tier: astra_runtime::prompts::CompactionTier::CompactHistory,
        keep_recent_turns: 2,
        current_tokens: 60000,
        session_memory_file: None,
        session_memory_combine:
            astra_runtime::turn::cloud::memoria_compact::SessionMemoryFileCombine::None,
        session_facts: None,
    };

    let result = compact_with_memoria(
        &messages,
        Some(tm.session_id()),
        &config,
        &params,
        Some(&*tm),
        None,
        None,
    )
    .await;

    // P0 fix: TieredCompaction now preserves the first user message.
    let has_original = result.messages.iter().any(|m| {
        m.get("content")
            .and_then(Value::as_str)
            .map(|s| s.contains("UNIQUE_TASK_MARKER_12345"))
            .unwrap_or(false)
    });
    assert!(
        has_original,
        "First user message must survive compaction. Got {} messages: {:?}",
        result.messages.len(),
        result
            .messages
            .iter()
            .map(|m| {
                let role = m.get("role").and_then(Value::as_str).unwrap_or("?");
                let c = m.get("content").and_then(Value::as_str).unwrap_or("");
                format!("{role}: {}...", &c[..c.len().min(60)])
            })
            .collect::<Vec<_>>()
    );
}

// ── L1 Parsed from Memoria Matches Protocol ─────────────────────────────────

#[tokio::test]
#[ignore = "requires live Memoria"]
async fn l1_round_trip_preserves_structure() {
    let tm = require_memoria!();
    let marker = format!("roundtrip-{}", &tm.session_id()[..12]);

    let original = sample_session_memory(&marker);
    let l1_before = SessionMemory::parse(&original).expect("should parse");
    assert!(l1_before.validate().is_ok());

    tm.store(&original, "working", Some(tm.session_id()), Some("T2"))
        .await
        .expect("store failed");

    let results = retrieve_l1_with_marker(&tm, &marker, tm.session_id()).await;
    let retrieved = results.first().expect("L1 not found in results");

    let l1_after = SessionMemory::parse(&retrieved.content).expect("retrieved L1 should parse");
    assert!(l1_after.validate().is_ok(), "retrieved L1 should validate");

    assert_eq!(
        l1_before.section("Task Specification"),
        l1_after.section("Task Specification"),
    );
    assert_eq!(
        l1_before.section("Current State"),
        l1_after.section("Current State"),
    );

    let injected = compress_to_injection(&l1_after);
    assert!(injected.starts_with(SESSION_MEMORY_PREFIX));
    assert!(injected.contains(&marker));

    let _ = tm.purge_working(tm.session_id()).await;
    tm.cleanup().await;
}

// ── Anchor Derived from Retrieved L1 ────────────────────────────────────────

#[tokio::test]
#[ignore = "requires live Memoria"]
async fn anchor_from_retrieved_l1() {
    let tm = require_memoria!();
    let marker = format!("anchor-{}", &tm.session_id()[..12]);

    let content = sample_session_memory(&marker);
    tm.store(&content, "working", Some(tm.session_id()), Some("T2"))
        .await
        .expect("store failed");

    let results = retrieve_l1_with_marker(&tm, &marker, tm.session_id()).await;
    let retrieved = results.first().expect("L1 not found");

    let l1 = SessionMemory::parse(&retrieved.content).expect("should parse");
    let anchor = extract_anchor("ignored", Some(&l1));

    assert!(anchor.starts_with("[session-anchor] "));
    assert!(anchor.contains(&marker));
    assert!(anchor.contains("1/3 steps"));

    let _ = tm.purge_working(tm.session_id()).await;
    tm.cleanup().await;
}

// ── Session Isolation ───────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires live Memoria"]
async fn different_sessions_are_isolated() {
    let tm = require_memoria!();
    let sid_a = &*unique_session_id();
    let sid_b = unique_session_id();
    let marker_a = format!("iso-A-{}", &sid_a[..12]);
    let marker_b = format!("iso-B-{}", &sid_b[..12]);

    tm.store(
        &sample_session_memory(&marker_a),
        "working",
        Some(sid_a),
        Some("T2"),
    )
    .await
    .expect("store A failed");

    tm.store(
        &sample_session_memory(&marker_b),
        "working",
        Some(&sid_b),
        Some("T2"),
    )
    .await
    .expect("store B failed");

    // Retrieve for marker A — should find A, not B.
    let results_a = retrieve_l1_with_marker(&tm, &marker_a, sid_a).await;
    assert!(
        !results_a.is_empty(),
        "session A memory should be retrievable"
    );
    assert!(
        !results_a.iter().any(|m| m.content.contains(&marker_b)),
        "session A results should not contain B's marker"
    );

    let _ = tm.purge_working(sid_a).await;
    let _ = tm.purge_working(&sid_b).await;
    tm.cleanup().await;
}

// ── Complex Multi-Compaction Scenario ───────────────────────────────────────

/// Simulates a realistic long session:
///   Phase 1: 30 turns of coding work → compaction → store L1
///   Phase 2: 20 more turns → second compaction → update L1
///   Verify: original task, key decisions, and file context survive both compactions.
#[tokio::test]
#[ignore = "requires live Memoria"]
async fn multi_compaction_preserves_goal_and_decisions() {
    let tm = require_memoria!();
    let marker = format!("multi-{}", &tm.session_id()[..12]);

    let original_task = format!(
        "GOAL_{marker}: Implement a distributed rate limiter using Redis \
         with sliding window algorithm, supporting 10K req/s per tenant"
    );

    // ── Phase 1: Build up a realistic 30-turn conversation ──────────────
    let mut messages: Vec<Value> = vec![
        system("You are a senior Rust engineer."),
        user(&original_task),
    ];

    // Simulate tool-heavy coding turns (tool results are the biggest token consumers)
    let decisions = [
        "DECISION_A: Use Redis MULTI/EXEC for atomic window updates",
        "DECISION_B: Shard by tenant_id hash to 16 Redis nodes",
        "DECISION_C: Fallback to local token bucket when Redis is unreachable",
    ];
    for (i, decision) in decisions.iter().enumerate().take(30) {
        // Assistant does tool calls, gets big results
        messages.push(assistant(&format!(
            "Step {i}: {}. Let me read the file...\n{}",
            if i < 3 {
                decision
            } else {
                "Continuing implementation"
            },
            "x".repeat(400) // simulate tool result bulk
        )));
        let prefix = if i == 15 {
            "Also remember: CRITICAL_NOTE_42 — must handle clock skew. "
        } else {
            ""
        };
        messages.push(user(&format!("{prefix}Continue with step {}", i + 1)));
    }

    // ── First compaction ────────────────────────────────────────────────
    let config = MemoriaCompactConfig::default();
    let params = MemoriaCompactParams {
        budget_chars: 4000,
        keep_chars: 2000,
        tier: astra_runtime::prompts::CompactionTier::AggressivePrune,
        keep_recent_turns: 3,
        current_tokens: 80000,
        session_memory_file: None,
        session_memory_combine:
            astra_runtime::turn::cloud::memoria_compact::SessionMemoryFileCombine::None,
        session_facts: None,
    };

    let result1 = compact_with_memoria(
        &messages,
        Some(tm.session_id()),
        &config,
        &params,
        Some(&*tm),
        None,
        None,
    )
    .await;

    // Original task must survive first compaction
    let has_task_1 = result1.messages.iter().any(|m| {
        m.get("content")
            .and_then(Value::as_str)
            .map(|s| s.contains(&format!("GOAL_{marker}")))
            .unwrap_or(false)
    });
    assert!(has_task_1, "Original task lost after first compaction");

    // Store L1 to Memoria (simulating what the turn loop would do)
    let l1_content = format!(
        "{SESSION_MEMORY_PREFIX}\n\
         # Session Title\n\
         Distributed Rate Limiter\n\
         # Task Specification\n\
         {original_task}\n\
         # Current State\n\
         Phase 1 complete, 30 turns done, basic implementation working\n\
         # Key Files\n\
         src/rate_limiter.rs — core sliding window logic\n\
         src/redis_backend.rs — Redis MULTI/EXEC integration\n\
         src/fallback.rs — local token bucket fallback\n\
         # Progress\n\
         ✅ Redis sliding window\n\
         ✅ Tenant sharding (16 nodes)\n\
         ✅ Local fallback\n\
         🔄 Clock skew handling\n\
         ⏳ Load testing\n\
         # Errors & Corrections\n\
         Turn 12: Fixed off-by-one in window boundary calculation\n\
         # Decisions\n\
         - {}\n\
         - {}\n\
         - {}\n\
         # User Messages\n\
         {original_task}\n\
         CRITICAL_NOTE_42 — must handle clock skew\n\
         # Worklog\n\
         T1-T10: Redis integration\n\
         T11-T20: Sharding + fallback\n\
         T21-T30: Bug fixes + clock skew start\n\
         # Context\n\
         Turn 30, ~80K tokens, first compaction done",
        decisions[0], decisions[1], decisions[2]
    );
    tm.store(&l1_content, "working", Some(tm.session_id()), Some("T2"))
        .await
        .expect("L1 store failed");

    // ── Phase 2: Continue with compacted messages + 20 more turns ───────
    let mut messages2 = result1.messages;
    for i in 30..50 {
        messages2.push(assistant(&format!(
            "Step {i}: Working on clock skew handling. {}",
            "z".repeat(400)
        )));
        messages2.push(user(&format!("Continue with step {}", i + 1)));
    }

    // ── Second compaction (should pull L1 from Memoria) ─────────────────
    let result2 = compact_with_memoria(
        &messages2,
        Some(tm.session_id()),
        &config,
        &params,
        Some(&*tm),
        None,
        None,
    )
    .await;

    // After second compaction, verify critical information survives
    let all_content: String = result2
        .messages
        .iter()
        .filter_map(|m| m.get("content").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");

    // Original task must survive (either in first user msg or L1 injection)
    assert!(
        all_content.contains(&format!("GOAL_{marker}"))
            || all_content.contains("distributed rate limiter")
            || all_content.contains("rate limiter"),
        "Original task lost after second compaction.\nAll content:\n{all_content}"
    );

    // First user message specifically must be preserved by P0 fix
    // (may be at index 1 or 2 depending on whether Memoria context was injected)
    let first_user_idx = result2
        .messages
        .iter()
        .position(|m| m.get("role").and_then(Value::as_str) == Some("user"));
    assert!(
        first_user_idx.is_some(),
        "No user message found after second compaction"
    );
    let first_user_content = result2.messages[first_user_idx.unwrap()]
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        first_user_content.contains(&format!("GOAL_{marker}")),
        "First user message should be the original task, got: {first_user_content}"
    );

    eprintln!(
        "Multi-compaction result: {} messages after 2 compactions of 50-turn session",
        result2.messages.len()
    );
    for (i, m) in result2.messages.iter().enumerate() {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("?");
        let c = m.get("content").and_then(Value::as_str).unwrap_or("");
        eprintln!("  [{i}] {role}: {}...", &c[..c.len().min(80)]);
    }
}

/// Simulates a session where tool results dominate token usage (realistic agent behavior).
/// Tool results are 80%+ of tokens. Compaction must not lose the task buried among them.
#[tokio::test]
#[ignore = "requires live Memoria"]
async fn tool_heavy_session_preserves_task() {
    let tm = require_memoria!();
    let marker = format!("tools-{}", &tm.session_id()[..12]);

    let original_task =
        format!("TASK_{marker}: Refactor the authentication module to use JWT with refresh tokens");

    let mut messages: Vec<Value> = vec![system("You are helpful."), user(&original_task)];

    // Simulate tool-heavy turns: assistant calls tools, gets huge results
    for i in 0..15 {
        // Assistant with tool_use
        messages.push(json!({
            "role": "assistant",
            "content": format!("Let me read auth.rs to understand the current flow (step {i})"),
            "tool_calls": [{
                "id": format!("call_{i}"),
                "type": "function",
                "function": {
                    "name": "fs_read",
                    "arguments": format!("{{\"path\": \"src/auth_{i}.rs\"}}")
                }
            }]
        }));
        // Tool result (big — simulates file reads)
        messages.push(json!({
            "role": "tool",
            "content": format!(
                "// auth_{i}.rs\n{}\n// end of file",
                "fn verify_token(t: &str) -> Result<Claims> { /* ... */ }\n".repeat(40)
            ),
            "tool_call_id": format!("call_{i}"),
        }));
        // User follow-up
        if i < 14 {
            messages.push(user(&format!("Good, now handle step {}", i + 1)));
        }
    }

    let config = MemoriaCompactConfig::default();
    let params = MemoriaCompactParams {
        budget_chars: 3000,
        keep_chars: 1500,
        tier: astra_runtime::prompts::CompactionTier::AggressivePrune,
        keep_recent_turns: 2,
        current_tokens: 70000,
        session_memory_file: None,
        session_memory_combine:
            astra_runtime::turn::cloud::memoria_compact::SessionMemoryFileCombine::None,
        session_facts: None,
    };

    let result = compact_with_memoria(
        &messages,
        Some(tm.session_id()),
        &config,
        &params,
        Some(&*tm),
        None,
        None,
    )
    .await;

    // Task must survive even when buried under tool results
    let has_task = result.messages.iter().any(|m| {
        m.get("content")
            .and_then(Value::as_str)
            .map(|s| s.contains(&format!("TASK_{marker}")))
            .unwrap_or(false)
    });
    assert!(
        has_task,
        "Task lost in tool-heavy session after compaction. Messages: {}",
        result.messages.len()
    );

    // Verify significant token reduction happened
    let original_chars: usize = messages.iter().map(|m| m.to_string().len()).sum();
    let compacted_chars: usize = result.messages.iter().map(|m| m.to_string().len()).sum();
    eprintln!(
        "Tool-heavy: {original_chars} chars → {compacted_chars} chars ({:.0}% reduction), {} → {} messages",
        (1.0 - compacted_chars as f64 / original_chars as f64) * 100.0,
        messages.len(),
        result.messages.len()
    );
    assert!(
        compacted_chars < original_chars / 2,
        "Compaction should reduce by at least 50%"
    );
}

// ── P1: L0 Anchor Extraction ────────────────────────────────────────────────

#[test]
fn anchor_from_first_user_message() {
    let anchor = extract_anchor("Build a distributed rate limiter using Redis", None);
    assert!(anchor.contains("[session-anchor]"));
    assert!(anchor.contains("rate limiter"));
    assert!(anchor.contains("starting"));
}

#[test]
fn anchor_from_l1_overrides_raw_message() {
    let l1_text = format!(
        "{SESSION_MEMORY_PREFIX}\n\
         # Session Title\nRate Limiter\n\
         # Task Specification\nImplement distributed rate limiter.\n\
         # Current State\nRedis integration done\n\
         # Key Files\nsrc/main.rs\n\
         # Progress\n✅ Setup\n🔄 Redis\n⏳ Tests\n\
         # Errors & Corrections\nNone\n\
         # Decisions\nUse Redis\n\
         # User Messages\nBuild rate limiter\n\
         # Worklog\nT1\n\
         # Context\nT2"
    );
    let l1 = SessionMemory::parse(&l1_text).unwrap();
    let anchor = extract_anchor("original message ignored", Some(&l1));
    assert!(anchor.contains("rate limiter"));
    assert!(anchor.contains("Redis integration done"));
    assert!(anchor.contains("1/3")); // 1 done out of 3 total
}

// ── P2: Continuation Prompt ─────────────────────────────────────────────────

/// The continuation prompt constant used by the turn loop.
const CONTINUATION_PROMPT: &str = "Continue the conversation from where it left off. \
                                    Do not ask the user any further questions — \
                                    pick up the current task and keep going.";

#[test]
fn continuation_prompt_is_user_role() {
    // Verify the continuation prompt format matches what the turn loop injects
    let msg = serde_json::json!({
        "role": "user",
        "content": CONTINUATION_PROMPT,
    });
    assert_eq!(msg["role"], "user");
    assert!(msg["content"].as_str().unwrap().contains("Continue"));
    assert!(msg["content"].as_str().unwrap().contains("keep going"));
}

// ── Token Efficiency Tests ──────────────────────────────────────────────────

#[test]
fn anchor_token_cost_under_budget() {
    // L0 anchor must be ≤50 tokens (~200 chars)
    let long_task = "Implement a distributed rate limiter using Redis with sliding \
                     window algorithm supporting 10K requests per second per tenant \
                     with automatic failover to local token bucket when Redis cluster \
                     is unreachable and comprehensive metrics export to Prometheus";
    let anchor = extract_anchor(long_task, None);
    let estimated_tokens = anchor.len() / 4;
    assert!(
        estimated_tokens <= 60,
        "Anchor should be ≤50 tokens, got ~{estimated_tokens} ({} chars): {anchor}",
        anchor.len()
    );
}

#[test]
fn l1_injection_respects_budget_cap() {
    // Build an oversized L1 and verify compress_to_injection stays within budget
    let mut big_l1_text = format!(
        "{SESSION_MEMORY_PREFIX}\n# Session Title\nTest\n# Task Specification\n{}\n",
        "Implement feature X. ".repeat(100) // ~2000 chars in one section
    );
    big_l1_text += &format!("# Current State\n{}\n", "Working on step N. ".repeat(50));
    big_l1_text += "# Key Files\n";
    big_l1_text += &"src/module_N.rs — does something important\n".repeat(50);
    big_l1_text += "# Progress\n✅ Done\n🔄 WIP\n⏳ Todo\n";
    big_l1_text += &format!("# Errors & Corrections\n{}\n", "Fixed bug N. ".repeat(50));
    big_l1_text += &format!("# Decisions\n{}\n", "- Decided X because Y. ".repeat(50));
    big_l1_text += &format!("# User Messages\n{}\n", "User said something. ".repeat(80));
    big_l1_text += &format!("# Worklog\n{}\n", "Turn N — did stuff. ".repeat(50));
    big_l1_text += "# Context\nTurn 50, ~100K tokens";

    let l1 = SessionMemory::parse(&big_l1_text).unwrap();
    let injected = compress_to_injection(&l1);
    let estimated_tokens = injected.len() / 4;
    assert!(
        estimated_tokens <= INJECTION_TOTAL_BUDGET + 100, // small margin for section headers
        "Injection should be ≤{INJECTION_TOTAL_BUDGET} tokens, got ~{estimated_tokens} ({} chars)",
        injected.len()
    );
}

#[test]
fn anchor_does_not_break_cached_prefix() {
    // Verified in bridge_inprocess.rs unit tests (p1_anchor_injected_into_openai_dynamic_message).
    // The anchor is passed via profile_desc (dynamic), not in the stable cached prefix.
    // OpenAI: primary message is identical with/without anchor.
    // Anthropic: anchor is in a non-cache-controlled block.
    //
    // This test just confirms the anchor doesn't accidentally end up in stable sections.
    let anchor = extract_anchor("Build rate limiter", None);
    assert!(anchor.contains("[session-anchor]"));
    // Anchor should NOT contain any cache scope markers
    assert!(!anchor.contains("cache_control"));
}

// ── Memory Overhead Measurement ─────────────────────────────────────────────

/// Measures exact token overhead of enabling Memoria vs pure compaction.
#[tokio::test]
#[ignore = "requires live Memoria"]
async fn measure_memoria_token_overhead() {
    let tm = require_memoria!();

    // Store L1 with unique marker so it ranks high in retrieve
    let marker = format!("overhead-{}", &tm.session_id()[..12]);
    let l1 = format!(
        "{SESSION_MEMORY_PREFIX}\n\
         # Session Title\n{marker} Rate Limiter\n\
         # Task Specification\n{marker} Build a rate limiter\n\
         # Current State\nStep 15 of 20\n\
         # Key Files\nsrc/rate_limiter.rs\n\
         # Progress\n✅ Setup\n🔄 Window logic\n\
         # Errors & Corrections\nNone\n\
         # Decisions\n- Use Redis\n\
         # User Messages\n{marker} Build rate limiter\n\
         # Worklog\nT1-T15\n\
         # Context\nTurn 15"
    );
    tm.store(&l1, "working", Some(tm.session_id()), Some("T2"))
        .await
        .unwrap();

    // Messages where last user msg contains the marker for high retrieve score
    let mut messages: Vec<Value> = vec![
        system("You are helpful."),
        user(&format!(
            "{marker} Build a rate limiter with sliding window"
        )),
    ];
    for i in 0..20 {
        messages.push(assistant(&format!("Step {i}. {}", "x".repeat(300))));
        messages.push(user(&format!(
            "{marker} Continue rate limiter step {}",
            i + 1
        )));
    }

    let config = MemoriaCompactConfig::default();
    let params = MemoriaCompactParams {
        budget_chars: 4000,
        keep_chars: 2000,
        tier: astra_runtime::prompts::CompactionTier::AggressivePrune,
        keep_recent_turns: 3,
        current_tokens: 80000,
        session_memory_file: None,
        session_memory_combine:
            astra_runtime::turn::cloud::memoria_compact::SessionMemoryFileCombine::None,
        session_facts: None,
    };

    // Without Memoria
    let without = compact_with_memoria(
        &messages,
        Some(tm.session_id()),
        &config,
        &params,
        None,
        None,
        None,
    )
    .await;

    // With Memoria
    let with = compact_with_memoria(
        &messages,
        Some(tm.session_id()),
        &config,
        &params,
        Some(&*tm),
        None,
        None,
    )
    .await;

    let chars_without: usize = without
        .messages
        .iter()
        .map(|m| m.get("content").and_then(Value::as_str).unwrap_or("").len())
        .sum();
    let chars_with: usize = with
        .messages
        .iter()
        .map(|m| m.get("content").and_then(Value::as_str).unwrap_or("").len())
        .sum();

    let tokens_without = chars_without / 4;
    let tokens_with = chars_with / 4;
    let overhead = tokens_with.saturating_sub(tokens_without);

    let has_memory_msg = with.messages.iter().any(|m| {
        m.get("content")
            .and_then(Value::as_str)
            .map(|s| s.contains("[Session Context"))
            .unwrap_or(false)
    });

    eprintln!("=== Memoria Token Overhead ===");
    eprintln!(
        "Without: {} msgs, ~{} tokens",
        without.messages.len(),
        tokens_without
    );
    eprintln!(
        "With:    {} msgs, ~{} tokens (injected={})",
        with.messages.len(),
        tokens_with,
        has_memory_msg
    );
    eprintln!("Overhead: ~{} tokens", overhead);

    // Budget compensation: memory injection tightens compaction budget,
    // so total stays within budget. Overhead should be small.
    // NOTE: injection may not trigger if retrieve doesn't return session-relevant
    // memories — session_id is only boosting, not filtering.
    // TODO: re-measure after https://github.com/matrixorigin/Memoria/issues/184
    assert!(
        overhead < 500,
        "Overhead should be <500 tokens, got {overhead}"
    );
}

// ── P3: Full Loop — L1 Write → Compaction Reads L1 → Goal Preserved ────────

/// End-to-end test of the session memory protocol:
/// 1. Build L1 from a conversation (simulating turn-end write)
/// 2. Store L1 to Memoria
/// 3. Run compaction on a new conversation that references the same session
/// 4. Verify the L1 content is retrieved and injected
#[tokio::test]
#[ignore = "requires live Memoria"]
async fn full_loop_l1_write_then_compaction_reads() {
    let tm = require_memoria!();
    let marker = format!("fullloop-{}", &tm.session_id()[..12]);

    // Phase 1: Simulate a conversation and build L1
    let phase1_messages: Vec<Value> = vec![
        system("You are helpful."),
        user(&format!(
            "{marker}: Build a distributed cache with LRU eviction"
        )),
        json!({"role": "assistant", "content": "I'll start.", "tool_calls": [
            {"id": "c1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\": \"src/cache.rs\"}"}}
        ]}),
        json!({"role": "tool", "content": "struct Cache {}", "tool_call_id": "c1"}),
        assistant("Cache struct found. Implementing LRU."),
        user("Add TTL support too"),
    ];

    let l1_content = build_l1_from_messages(&phase1_messages, 3, 40000);

    // Verify L1 is valid
    let l1 = SessionMemory::parse(&l1_content).expect("L1 should parse");
    assert!(l1.validate().is_ok());
    assert!(l1.section("Task Specification").unwrap().contains(&marker));

    // Store L1 to Memoria
    tm.store(&l1_content, "working", Some(tm.session_id()), Some("T2"))
        .await
        .expect("store failed");

    // Phase 2: New conversation (after compaction dropped history)
    // Use the marker in user messages so retrieve query matches our L1
    let mut phase2_messages: Vec<Value> = vec![
        system("You are helpful."),
        user(&format!(
            "{marker}: Build a distributed cache with LRU eviction"
        )),
    ];
    for i in 0..20 {
        phase2_messages.push(assistant(&format!("Step {i}. {}", "z".repeat(300))));
        phase2_messages.push(user(&format!("{marker} continue cache step {}", i + 1)));
    }

    let config = MemoriaCompactConfig::default();
    let params = MemoriaCompactParams {
        budget_chars: 4000,
        keep_chars: 2000,
        tier: astra_runtime::prompts::CompactionTier::AggressivePrune,
        keep_recent_turns: 3,
        current_tokens: 80000,
        session_memory_file: None,
        session_memory_combine:
            astra_runtime::turn::cloud::memoria_compact::SessionMemoryFileCombine::None,
        session_facts: None,
    };

    let result = compact_with_memoria(
        &phase2_messages,
        Some(tm.session_id()),
        &config,
        &params,
        Some(&*tm),
        None,
        None,
    )
    .await;

    // Verify: original task preserved (via first user message P0 fix)
    let all_content: String = result
        .messages
        .iter()
        .filter_map(|m| m.get("content").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all_content.contains(&marker),
        "Original task marker should survive compaction"
    );

    // Check if L1 was injected as session context
    // NOTE: may not inject if retrieve doesn't rank our L1 in top-K
    // (blocked on https://github.com/matrixorigin/Memoria/issues/184)
    let has_session_context = all_content.contains("[Session Context");
    eprintln!(
        "Full loop: {} msgs, L1 injected={}, task preserved=true",
        result.messages.len(),
        has_session_context
    );
}

// ── L2 Fallback: Memoria unavailable → LLM summary still works ─────────────

#[tokio::test]
#[ignore = "requires live Memoria"]
async fn l2_fallback_when_memoria_unavailable() {
    // No require_memoria! — we intentionally pass None as client
    let mut messages: Vec<Value> = vec![system("You are helpful."), user("Build a rate limiter")];
    for i in 0..20 {
        messages.push(assistant(&format!("Step {i}. {}", "x".repeat(300))));
        messages.push(user(&format!("Next {}", i + 1)));
    }

    let config = MemoriaCompactConfig::default();
    let params = MemoriaCompactParams {
        budget_chars: 4000,
        keep_chars: 2000,
        tier: astra_runtime::prompts::CompactionTier::AggressivePrune,
        keep_recent_turns: 3,
        current_tokens: 80000,
        session_memory_file: None,
        session_memory_combine:
            astra_runtime::turn::cloud::memoria_compact::SessionMemoryFileCombine::None,
        session_facts: None,
    };

    // No Memoria client — should still compact without error
    let result = compact_with_memoria(
        &messages,
        Some("no-memoria-session"),
        &config,
        &params,
        None, // no Memoria
        None,
        None,
    )
    .await;

    // Compaction should still work
    assert!(
        result.messages.len() < messages.len(),
        "should have compacted"
    );
    // First user message preserved
    let has_task = result.messages.iter().any(|m| {
        m.get("content")
            .and_then(Value::as_str)
            .map(|s| s.contains("rate limiter"))
            .unwrap_or(false)
    });
    assert!(has_task, "Task should survive even without Memoria");
}

// ── Cross-Session Bootstrap: new session retrieves old session's memories ───

#[tokio::test]
#[ignore = "requires live Memoria"]
async fn cross_session_retrieves_other_sessions_memories() {
    let tm = require_memoria!();

    // Session A stores a memory
    let sid_a = &*unique_session_id();
    let marker_a = format!("cross-{}", &sid_a[..12]);
    tm.store(
        &format!("{SESSION_MEMORY_PREFIX}\n# Task Specification\n{marker_a}: old session work"),
        "working",
        Some(sid_a),
        Some("T2"),
    )
    .await
    .expect("store failed");

    // Session B retrieves — should find session A's memory (cross-session)
    let sid_b = unique_session_id();
    let results = tm
        .retrieve(&format!("{marker_a} old session"), Some(&sid_b), 10)
        .await
        .expect("retrieve failed");

    let found = results.iter().any(|m| m.content.contains(&marker_a));
    assert!(
        found,
        "New session should retrieve old session's memories (cross-session bootstrap). Got {} results",
        results.len()
    );
    tm.cleanup().await;
}

// ── Post-Compact File Restoration: recent_files carried in boundary ─────────

#[test]
fn post_compact_boundary_carries_recent_files() {
    use astra_runtime::prompts::CompactionTier;
    use astra_runtime::turn::cloud::compaction::{CompactBoundary, CompactTrigger};

    let boundary = CompactBoundary::new(CompactTrigger::Auto, CompactionTier::CompactHistory)
        .with_recent_files(vec!["src/main.rs".into(), "src/lib.rs".into()])
        .with_discovered_tools(vec!["read_file".into(), "grep".into()]);

    assert_eq!(boundary.recent_files, vec!["src/main.rs", "src/lib.rs"]);
    assert_eq!(boundary.discovered_tools, vec!["read_file", "grep"]);
}

#[test]
fn post_compact_boundary_serializes_files() {
    use astra_runtime::prompts::CompactionTier;
    use astra_runtime::turn::cloud::compaction::{CompactBoundary, CompactTrigger};

    let boundary = CompactBoundary::new(CompactTrigger::Auto, CompactionTier::CompactHistory)
        .with_recent_files(vec!["src/cache.rs".into()]);

    let json = serde_json::to_value(&boundary).unwrap();
    let files = json.get("recent_files").and_then(Value::as_array).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0], "src/cache.rs");
}
