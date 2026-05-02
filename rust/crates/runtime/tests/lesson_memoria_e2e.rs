//! Live Memoria E2E: verify the lesson write/read pipeline against a
//! real Memoria instance.
//!
//! ```text
//! ASTRA_TEST_MEMORIA_E2E=1 cargo test -p astra-runtime --test lesson_memoria_e2e -- --ignored
//! ```
//!
//! Requires:
//!   - Memoria running at MEMORIA_BASE_URL (default: http://127.0.0.1:8100)
//!   - MEMORIA_MASTER_KEY set
//!
//! These tests verify the FULL lesson lifecycle against live Memoria:
//!   1. Store semantic lesson → retrieve it back
//!   2. Batch store (session-end pattern) → retrieve all
//!   3. Retrieve returns direct array with correct fields
//!   4. Trust tier mapping (T2/T3) produces correct confidence

fn require_memoria_env() -> (String, String) {
    assert_eq!(
        std::env::var("ASTRA_TEST_MEMORIA_E2E").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_MEMORIA_E2E=1 to run Memoria E2E tests"
    );
    dotenvy::dotenv().ok();
    let base = std::env::var("MEMORIA_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8100".into());
    let key = std::env::var("MEMORIA_MASTER_KEY").expect("MEMORIA_MASTER_KEY must be set");
    (base, key)
}

fn unique_user_id() -> String {
    format!("test-lesson-e2e-{}", uuid::Uuid::new_v4())
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .no_proxy()
        .build()
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn store_and_retrieve_semantic_lesson() {
    let (base, key) = require_memoria_env();
    let user_id = unique_user_id();
    let client = client();

    // Store
    let resp = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {key}"))
        .header("X-User-Id", &user_id)
        .json(&serde_json::json!({
            "content": "💡 LESSON: Use rg --glob '!node_modules' instead of grep -r",
            "memory_type": "semantic",
            "trust_tier": "T3",
        }))
        .send()
        .await
        .expect("store request");
    assert!(
        resp.status().is_success(),
        "store must succeed: {}",
        resp.status()
    );
    let stored: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(stored["memory_type"], "semantic");
    assert_eq!(stored["trust_tier"], "T3");
    let memory_id = stored["memory_id"].as_str().expect("memory_id");
    assert!(!memory_id.is_empty());

    // Retrieve
    let resp = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("Authorization", format!("Bearer {key}"))
        .header("X-User-Id", &user_id)
        .json(&serde_json::json!({
            "query": "rg grep monorepo node_modules",
            "top_k": 3,
        }))
        .send()
        .await
        .expect("retrieve request");
    assert_eq!(resp.status(), 200);
    let results: serde_json::Value = resp.json().await.unwrap();

    // Response is a direct JSON array
    let arr = results.as_array().expect("retrieve must return array");
    assert!(!arr.is_empty(), "stored lesson must be retrievable");

    let first = &arr[0];
    assert!(
        first["content"].as_str().unwrap().contains("rg"),
        "retrieved content must contain 'rg'"
    );
    assert_eq!(first["memory_type"], "semantic");

    // Cleanup
    let _ = client
        .delete(format!("{base}/v1/memories/{memory_id}"))
        .header("Authorization", format!("Bearer {key}"))
        .header("X-User-Id", &user_id)
        .send()
        .await;
}

#[tokio::test]
#[ignore]
async fn batch_store_session_end_pattern() {
    let (base, key) = require_memoria_env();
    let user_id = unique_user_id();
    let client = client();

    let resp = client
        .post(format!("{base}/v1/memories/batch"))
        .header("Authorization", format!("Bearer {key}"))
        .header("X-User-Id", &user_id)
        .json(&serde_json::json!({
            "memories": [
                {
                    "content": "🔧 CORRECTION: Always use RS256 not HS256",
                    "memory_type": "semantic",
                    "trust_tier": "T2",
                },
                {
                    "content": "💡 LESSON: pnpm workspaces require --filter flag",
                    "memory_type": "semantic",
                    "trust_tier": "T3",
                },
                {
                    "content": "Session sess-e2e (15 turns): Implement OAuth",
                    "memory_type": "episodic",
                    "trust_tier": "T3",
                },
            ]
        }))
        .send()
        .await
        .expect("batch store");
    assert!(
        resp.status().is_success(),
        "batch store must succeed: {}",
        resp.status()
    );
    let stored: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(stored.len(), 3, "all 3 memories must be stored");

    // T2 correction should have higher confidence than T3 lesson
    let correction = stored.iter().find(|m| m["trust_tier"] == "T2").unwrap();
    let lesson = stored
        .iter()
        .find(|m| m["content"].as_str().unwrap().contains("pnpm"))
        .unwrap();
    let corr_conf: f64 = correction["initial_confidence"].as_f64().unwrap();
    let less_conf: f64 = lesson["initial_confidence"].as_f64().unwrap();
    assert!(
        corr_conf > less_conf,
        "T2 ({corr_conf}) must have higher confidence than T3 ({less_conf})"
    );

    // Retrieve all — semantic query should find corrections and lessons
    let resp = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("Authorization", format!("Bearer {key}"))
        .header("X-User-Id", &user_id)
        .json(&serde_json::json!({
            "query": "JWT RS256 pnpm OAuth lessons corrections",
            "top_k": 5,
        }))
        .send()
        .await
        .unwrap();
    let results: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(
        results.len() >= 2,
        "should retrieve at least 2 of 3 stored memories, got {}",
        results.len()
    );

    // Cleanup
    for m in &stored {
        let mid = m["memory_id"].as_str().unwrap();
        let _ = client
            .delete(format!("{base}/v1/memories/{mid}"))
            .header("Authorization", format!("Bearer {key}"))
            .header("X-User-Id", &user_id)
            .send()
            .await;
    }
}

#[tokio::test]
#[ignore]
async fn governance_and_consolidate_paths_reachable() {
    let (base, key) = require_memoria_env();
    let user_id = unique_user_id();
    let client = client();

    // These may return 500 for fresh users (no tables), but should NOT 404.
    let gov = client
        .post(format!("{base}/v1/governance"))
        .header("Authorization", format!("Bearer {key}"))
        .header("X-User-Id", &user_id)
        .json(&serde_json::json!({"force": false}))
        .send()
        .await
        .expect("governance request");
    assert_ne!(
        gov.status(),
        404,
        "governance must not 404 (was /v1/memories/governance before fix)"
    );

    let con = client
        .post(format!("{base}/v1/consolidate"))
        .header("Authorization", format!("Bearer {key}"))
        .header("X-User-Id", &user_id)
        .json(&serde_json::json!({"force": false}))
        .send()
        .await
        .expect("consolidate request");
    assert_ne!(
        con.status(),
        404,
        "consolidate must not 404 (was /v1/memories/consolidate before fix)"
    );
}

// ── Cross-session lesson flow: store → retrieve → SelfModel prompt ──────

/// The money test: proves that a lesson stored in Session A appears in
/// Session B's SelfModel prompt section. This is the complete data path
/// from Memoria storage through retrieval, LessonHint construction,
/// SelfModel.with_lessons(), and to_system_prompt_section().
#[tokio::test]
#[ignore]
async fn cross_session_lesson_appears_in_self_model_prompt() {
    let (base, key) = require_memoria_env();
    let user_id = unique_user_id();
    let client = client();

    // ─── Session A: store a specific lesson ─────────────────────────
    let resp = client
        .post(format!("{base}/v1/memories/batch"))
        .header("Authorization", format!("Bearer {key}"))
        .header("X-User-Id", &user_id)
        .json(&serde_json::json!({
            "memories": [
                {
                    "content": "🔧 CORRECTION: In this monorepo, always use `rg --glob '!node_modules'` instead of `grep -r`",
                    "memory_type": "semantic",
                    "trust_tier": "T2",
                },
                {
                    "content": "💡 LESSON: pnpm workspaces require --filter flag for cross-package commands",
                    "memory_type": "semantic",
                    "trust_tier": "T3",
                },
            ]
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let stored: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(stored.len(), 2);

    // ─── Session B: retrieve lessons (simulating bootstrap) ─────────
    let resp = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("Authorization", format!("Bearer {key}"))
        .header("X-User-Id", &user_id)
        .json(&serde_json::json!({
            "query": "grep rg pnpm monorepo tools",
            "top_k": 5,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let results: serde_json::Value = resp.json().await.unwrap();
    let memories = results.as_array().expect("direct array");
    assert!(
        memories.len() >= 2,
        "both lessons must be retrievable, got {}",
        memories.len()
    );

    // ─── Convert to LessonHints (same logic as memoria_retrieve_lessons) ──
    let hints: Vec<astra_services::LessonHint> = memories
        .iter()
        .filter_map(|m| {
            let content = m.get("content")?.as_str()?;
            let memory_type = m.get("memory_type")?.as_str()?;
            if !matches!(memory_type, "semantic" | "procedural") {
                return None;
            }
            let action = astra_services::sanitize_for_prompt(content);
            let compact = if action.len() > 80 {
                action
                    .split_once(['.', '—', ';'])
                    .map(|(s, _)| s.trim().to_string())
            } else {
                None
            };
            Some(astra_services::LessonHint {
                kind: astra_services::LessonKind::PromptShape,
                trigger_signal: "memoria".into(),
                action,
                compact,
                workload_tag: None,
            })
        })
        .collect();
    assert!(hints.len() >= 2, "both lessons parsed to hints");

    // ─── Attach to SelfModel and render prompt ──────────────────────
    let model_json = serde_json::json!({
        "capabilities": {
            "total_tools": 0, "tool_names": [], "tool_health": [],
            "deprioritized_tools": [], "pinned_tools": [], "skills": [],
            "boosted_tools": [], "widen_selection_pending": false,
            "outcome_memory": [],
        },
        "state": {
            "turn_number": 1, "token_budget": null, "scenario": null,
            "active_experiment": null, "session_elapsed_secs": 0,
            "correction_count": 0, "compression_count": 0,
        },
        "goals": {
            "goal": null, "session_goal": null, "plan_goal": null,
            "tracked_goal": null, "goal_source": "none",
            "tracking_status": "idle", "progress": null,
            "recent_milestones": [], "milestone_count": 0,
        },
        "recent_signals": [],
        "constraints": {
            "max_mutations_per_turn": 2, "config_drift_ceiling": 0.3,
            "min_tool_pool_size": 5, "token_reserve_fraction": 0.2,
        }
    });
    let self_model: astra_runtime::self_model::SelfModel =
        serde_json::from_value(model_json).unwrap();
    let self_model = self_model.with_lessons(hints);
    let prompt = self_model.to_system_prompt_section();

    // ─── The actual assertion: lessons visible in the prompt ─────────
    assert!(
        prompt.contains("📚 Lessons from prior sessions"),
        "prompt must have lessons header:\n{prompt}"
    );
    assert!(
        prompt.contains("rg") && prompt.contains("grep"),
        "rg/grep lesson must be visible in prompt:\n{prompt}"
    );
    assert!(
        prompt.contains("pnpm"),
        "pnpm lesson must be visible in prompt:\n{prompt}"
    );

    // ─── Cleanup ────────────────────────────────────────────────────
    for m in &stored {
        let mid = m["memory_id"].as_str().unwrap();
        let _ = client
            .delete(format!("{base}/v1/memories/{mid}"))
            .header("Authorization", format!("Bearer {key}"))
            .header("X-User-Id", &user_id)
            .send()
            .await;
    }
}
