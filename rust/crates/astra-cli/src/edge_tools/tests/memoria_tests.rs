use super::{memoria, parse_memory_search_contents, test_executor};

// ── parse_memory_search_contents: all JSON variants ──────────────────────

#[test]
fn parse_memory_search_contents_all_formats() {
    // memories array
    let raw = r#"{"memories":[{"content":"matrixorigin is a GitHub org","score":0.9},{"content":"user prefers Rust","score":0.7}]}"#;
    assert_eq!(
        parse_memory_search_contents(raw),
        vec!["matrixorigin is a GitHub org", "user prefers Rust"]
    );

    // results array
    let raw = r#"{"results":[{"content":"mo is a database company"},{"content":"user likes dark mode"}]}"#;
    assert_eq!(
        parse_memory_search_contents(raw),
        vec!["mo is a database company", "user likes dark mode"]
    );

    // top-level array
    let raw = r#"[{"content":"matrixorigin = GitHub org"},{"text":"user follows MO"}]"#;
    assert_eq!(
        parse_memory_search_contents(raw),
        vec!["matrixorigin = GitHub org", "user follows MO"]
    );

    // error response
    assert!(parse_memory_search_contents(r#"{"error":"Memory unavailable"}"#).is_empty());

    // invalid JSON
    assert!(parse_memory_search_contents("not json").is_empty());
    assert!(parse_memory_search_contents("").is_empty());

    // empty content filtered
    let raw = r#"{"memories":[{"content":""},{"content":"valid memory"}]}"#;
    assert_eq!(parse_memory_search_contents(raw), vec!["valid memory"]);

    // single object (not wrapped in array)
    let raw = r#"{"content":"single memory result"}"#;
    assert_eq!(
        parse_memory_search_contents(raw),
        vec!["single memory result"]
    );

    // no content field
    let raw = r#"{"memories":[{"summary":"no content field"}]}"#;
    assert!(parse_memory_search_contents(raw).is_empty());
}

// ── memory_boost_search: edge cases ──────────────────────────────────────

#[tokio::test]
async fn memory_boost_search_edge_cases() {
    let executor = test_executor();
    assert!(executor.memory_boost_search("", 5).await.is_empty());
    assert!(executor.memory_boost_search("   ", 5).await.is_empty());
    // feedback on empty list should not panic
    executor.memory_feedback_useful(vec![]);
}

// ── parse_memory_search_hits: all JSON variants ──────────────────────────

#[test]
fn parse_memory_search_hits_all_formats() {
    // extracts memory IDs
    let raw = r#"{"memories":[
        {"memory_id":"m-001","content":"rust is great","score":0.9},
        {"memory_id":"m-002","content":"user prefers dark mode","score":0.7}
    ]}"#;
    let hits = memoria::parse_memory_search_hits(raw);
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].memory_id.as_deref(), Some("m-001"));
    assert_eq!(hits[0].content, "rust is great");
    assert_eq!(hits[1].memory_id.as_deref(), Some("m-002"));

    // id field alias
    let hits =
        memoria::parse_memory_search_hits(r#"{"memories":[{"id":"abc","content":"test memory"}]}"#);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].memory_id.as_deref(), Some("abc"));

    // missing IDs are None
    let hits = memoria::parse_memory_search_hits(r#"{"memories":[{"content":"no id here"}]}"#);
    assert_eq!(hits.len(), 1);
    assert!(hits[0].memory_id.is_none());
    assert_eq!(hits[0].content, "no id here");

    // single object with id
    let hits =
        memoria::parse_memory_search_hits(r#"{"memory_id":"single-1","content":"single hit"}"#);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].memory_id.as_deref(), Some("single-1"));

    // empty content filtered
    let raw =
        r#"{"memories":[{"memory_id":"m1","content":""},{"memory_id":"m2","content":"valid"}]}"#;
    let hits = memoria::parse_memory_search_hits(raw);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].memory_id.as_deref(), Some("m2"));

    // error response
    assert!(memoria::parse_memory_search_hits(r#"{"error":"not available"}"#).is_empty());
}
