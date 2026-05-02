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
