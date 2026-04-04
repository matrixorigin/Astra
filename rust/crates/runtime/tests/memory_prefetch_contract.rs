use astra_runtime::turn::bridge_inprocess::prefetch_memories;
/// End-to-end contract tests for memory prefetch in InProcessBridge.
///
/// Uses a real HTTP mock server to simulate Memoria API responses,
/// then verifies the full prefetch pipeline produces correct output.
use axum::{Json, Router, routing::post};
use serde_json::json;
use tokio::net::TcpListener;

/// Start a mock Memoria server that returns canned memories for any query.
async fn start_mock_memoria(memories: Vec<serde_json::Value>) -> String {
    let app = Router::new().route(
        "/v1/memories/retrieve",
        post(move |Json(_body): Json<serde_json::Value>| {
            let memories = memories.clone();
            async move { Json(memories) }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Yield to let the server task start accepting connections
    tokio::task::yield_now().await;
    format!("http://{addr}")
}

/// Start a mock that returns different results based on query content.
async fn start_mock_memoria_selective() -> String {
    let app = Router::new().route(
        "/v1/memories/retrieve",
        post(|Json(body): Json<serde_json::Value>| async move {
            let query = body["query"].as_str().unwrap_or("");
            // "memoria" keyword query should match the repo mapping
            if query == "memoria" || query.contains("matrixorigin") {
                Json(vec![json!({
                    "content": "[@fact/semantic] memoria repository is matrixorigin/Memoria on GitHub",
                    "score": 0.95
                })])
            } else if query.contains("最新") || query.contains("ci") {
                // Full Chinese message — embedding miss (simulates real behavior)
                Json(vec![])
            } else {
                Json(vec![])
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::task::yield_now().await;
    format!("http://{addr}")
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Core scenario: "memoria 最新的ci?" should find matrixorigin/Memoria via
/// hybrid retrieval even when the full message embedding misses.
#[tokio::test]
async fn prefetch_finds_repo_via_entity_query_when_full_query_misses() {
    let url = start_mock_memoria_selective().await;

    let result = prefetch_memories(&url, "test-key", "memoria 最新的ci?", "user-1").await;

    assert!(
        result.items > 0,
        "should find memories via entity query 'memoria'"
    );
    let section = result.section.expect("should produce a memory section");
    assert!(
        section.contains("matrixorigin/Memoria"),
        "section should contain the repo mapping, got: {section}"
    );
}

/// When Memoria returns results for the full query, they are included.
#[tokio::test]
async fn prefetch_includes_full_query_results() {
    let url = start_mock_memoria(vec![json!({
        "content": "[@pref/active] user prefers dark mode",
        "score": 0.9
    })])
    .await;

    let result = prefetch_memories(&url, "test-key", "what theme do I use?", "user-1").await;

    assert_eq!(result.items, 1);
    let section = result.section.unwrap();
    assert!(section.contains("dark mode"), "got: {section}");
}

/// Duplicate memories from full + entity queries are deduplicated.
#[tokio::test]
async fn prefetch_deduplicates_across_queries() {
    // Both queries return the same memory
    let url = start_mock_memoria(vec![json!({
        "content": "[@fact/semantic] memoria is matrixorigin/Memoria",
        "score": 0.95
    })])
    .await;

    let result = prefetch_memories(&url, "test-key", "memoria latest ci", "user-1").await;

    // Should be 1, not 2 (deduped)
    assert_eq!(result.items, 1, "duplicates should be merged");
}

/// Empty key skips prefetch entirely.
#[tokio::test]
async fn prefetch_skips_on_empty_key() {
    let result = prefetch_memories("http://unused", "", "memoria ci", "user-1").await;
    assert_eq!(result.items, 0);
    assert!(result.section.is_none());
}

/// Empty message skips prefetch.
#[tokio::test]
async fn prefetch_skips_on_empty_message() {
    let result = prefetch_memories("http://unused", "key", "   ", "user-1").await;
    assert_eq!(result.items, 0);
    assert!(result.section.is_none());
}

/// Memoria API returning empty results produces no section.
#[tokio::test]
async fn prefetch_no_section_when_no_memories() {
    let url = start_mock_memoria(vec![]).await;

    let result = prefetch_memories(&url, "test-key", "random question", "user-1").await;

    assert_eq!(result.items, 0);
    assert!(result.section.is_none());
}

/// Pure ASCII message: entity query equals full query, only one fetch happens.
/// (We can't directly assert fetch count, but we verify correctness.)
#[tokio::test]
async fn prefetch_pure_ascii_still_works() {
    let url = start_mock_memoria(vec![json!({
        "content": "[@pref/active] matrixone = matrixorigin/matrixone",
        "score": 0.9
    })])
    .await;

    let result = prefetch_memories(&url, "test-key", "matrixone latest pr", "user-1").await;

    assert_eq!(result.items, 1);
    let section = result.section.unwrap();
    assert!(section.contains("matrixorigin/matrixone"), "got: {section}");
}

/// Unreachable Memoria server doesn't crash — returns empty gracefully.
#[tokio::test]
async fn prefetch_handles_unreachable_server() {
    let result = prefetch_memories("http://127.0.0.1:1", "test-key", "memoria ci", "user-1").await;

    assert_eq!(result.items, 0);
    assert!(result.section.is_none());
}
