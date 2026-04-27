//! Guards against the "connectivity probe" contract silently going undocumented.
//!
//! The non-stream mock in the session-artifacts HTTP journey
//! (`system_matrix_http_e2e/journey_session_artifacts_matrix.rs`) optionally
//! answers the first non-stream fallback request with an HTTP 200 "probe ok"
//! body before serving any real fallback. Five assertions use
//! `assert_nonstream_hits_in_range` with a +1 tolerance to accept this probe.
//!
//! Without a doc comment, readers cannot tell why the ranges are widened.
//! This test is a drift guard: it reads the journey source and asserts the
//! helper carries a doc comment explaining the probe contract.

const JOURNEY_SOURCE: &str =
    include_str!("system_matrix_http_e2e/journey_session_artifacts_matrix.rs");

#[test]
fn probe_contract_is_documented_above_range_helper() {
    let helper = "fn assert_nonstream_hits_in_range";
    let helper_pos = JOURNEY_SOURCE
        .find(helper)
        .expect("assert_nonstream_hits_in_range helper should exist in journey source");

    // Walk backwards through the lines immediately preceding the helper,
    // collecting every `///` doc line until the contiguous block ends.
    let prefix = &JOURNEY_SOURCE[..helper_pos];
    let doc_lines: Vec<&str> = prefix
        .lines()
        .rev()
        .take_while(|line| line.trim_start().starts_with("///"))
        .collect();
    assert!(
        !doc_lines.is_empty(),
        "doc comment should precede assert_nonstream_hits_in_range"
    );
    let doc_block: String = doc_lines.into_iter().rev().collect::<Vec<_>>().join("\n");
    let doc_block = doc_block.as_str();

    assert!(
        doc_block.contains("probe"),
        "probe contract doc should mention 'probe': got {doc_block:?}"
    );
    assert!(
        doc_block.contains("HTTP 200"),
        "probe contract doc should mention 'HTTP 200': got {doc_block:?}"
    );
    assert!(
        doc_block.contains("probe ok"),
        "probe contract doc should reference the 'probe ok' body: got {doc_block:?}"
    );
    assert!(
        doc_block.contains("N..=N+1") || doc_block.contains("N..=N + 1"),
        "probe contract doc should explain N..=N+1 tolerance: got {doc_block:?}"
    );
}
