use super::*;

// ── parse_memory_search_contents ──────────────────────────────────────────

#[test]
fn parse_memory_memories_array() {
    let raw = r#"{"memories":[{"content":"matrixorigin is a GitHub org","score":0.9},{"content":"user prefers Rust","score":0.7}]}"#;
    let result = parse_memory_search_contents(raw);
    assert_eq!(
        result,
        vec!["matrixorigin is a GitHub org", "user prefers Rust"]
    );
}

#[test]
fn parse_memory_results_array() {
    let raw = r#"{"results":[{"content":"mo is a database company"},{"content":"user likes dark mode"}]}"#;
    let result = parse_memory_search_contents(raw);
    assert_eq!(
        result,
        vec!["mo is a database company", "user likes dark mode"]
    );
}

#[test]
fn parse_memory_top_level_array() {
    let raw = r#"[{"content":"matrixorigin = GitHub org"},{"text":"user follows MO"}]"#;
    let result = parse_memory_search_contents(raw);
    assert_eq!(result, vec!["matrixorigin = GitHub org", "user follows MO"]);
}

#[test]
fn parse_memory_error_response() {
    let raw = r#"{"error":"Memory unavailable: not connected"}"#;
    let result = parse_memory_search_contents(raw);
    assert!(result.is_empty(), "error response should return empty");
}

#[test]
fn parse_memory_invalid_json() {
    assert!(parse_memory_search_contents("not json").is_empty());
    assert!(parse_memory_search_contents("").is_empty());
}

#[test]
fn parse_memory_empty_content_filtered() {
    let raw = r#"{"memories":[{"content":""},{"content":"valid memory"}]}"#;
    let result = parse_memory_search_contents(raw);
    assert_eq!(result, vec!["valid memory"]);
}

#[test]
fn parse_memory_single_object() {
    let raw = r#"{"content":"single memory result"}"#;
    let result = parse_memory_search_contents(raw);
    assert_eq!(result, vec!["single memory result"]);
}

#[test]
fn parse_memory_no_content_field() {
    let raw = r#"{"memories":[{"summary":"no content field"}]}"#;
    let result = parse_memory_search_contents(raw);
    assert!(result.is_empty());
}

#[tokio::test]
async fn memory_boost_search_empty_query() {
    let executor = test_executor();
    let result = executor.memory_boost_search("", 5).await;
    assert!(result.is_empty(), "empty query should return empty");
}

#[tokio::test]
async fn memory_boost_search_whitespace_query() {
    let executor = test_executor();
    let result = executor.memory_boost_search("   ", 5).await;
    assert!(result.is_empty(), "whitespace query should return empty");
}

// ── parse_memory_search_hits (with IDs) ───────────────────────────────────

#[test]
fn parse_hits_extracts_memory_ids() {
    let raw = r#"{"memories":[
        {"memory_id":"m-001","content":"rust is great","score":0.9},
        {"memory_id":"m-002","content":"user prefers dark mode","score":0.7}
    ]}"#;
    let hits = memoria::parse_memory_search_hits(raw);
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].memory_id.as_deref(), Some("m-001"));
    assert_eq!(hits[0].content, "rust is great");
    assert_eq!(hits[1].memory_id.as_deref(), Some("m-002"));
}

#[test]
fn parse_hits_handles_id_field_alias() {
    let raw = r#"{"memories":[{"id":"abc","content":"test memory"}]}"#;
    let hits = memoria::parse_memory_search_hits(raw);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].memory_id.as_deref(), Some("abc"));
}

#[test]
fn parse_hits_missing_ids_are_none() {
    let raw = r#"{"memories":[{"content":"no id here"}]}"#;
    let hits = memoria::parse_memory_search_hits(raw);
    assert_eq!(hits.len(), 1);
    assert!(hits[0].memory_id.is_none());
    assert_eq!(hits[0].content, "no id here");
}

#[test]
fn parse_hits_single_object_with_id() {
    let raw = r#"{"memory_id":"single-1","content":"single hit"}"#;
    let hits = memoria::parse_memory_search_hits(raw);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].memory_id.as_deref(), Some("single-1"));
}

#[test]
fn parse_hits_empty_content_filtered() {
    let raw =
        r#"{"memories":[{"memory_id":"m1","content":""},{"memory_id":"m2","content":"valid"}]}"#;
    let hits = memoria::parse_memory_search_hits(raw);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].memory_id.as_deref(), Some("m2"));
}

#[test]
fn parse_hits_error_response_empty() {
    let raw = r#"{"error":"not available"}"#;
    assert!(memoria::parse_memory_search_hits(raw).is_empty());
}

#[test]
fn memory_feedback_useful_noop_on_empty() {
    let executor = test_executor();
    // Should not panic
    executor.memory_feedback_useful(vec![]);
}
