//! Shipped-case smoke tests.
//!
//! Preventive regression for the R2/R3/R4 pattern: rename a criterion
//! (or change a required field), and the shipped `cases/*.yaml` files
//! silently stop loading — but the crate still compiles and unit
//! tests pass, because unit tests use synthetic YAML. Only a
//! "load every shipped case" test surfaces the drift.
//!
//! These tests run at integration scope so they exercise the same
//! `Case::load_dir` path CI uses, against the actual YAML files the
//! repo ships.

use std::path::PathBuf;

use astra_test_harness::case::Case;

fn shipped_cases_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("cases")
}

#[test]
fn every_shipped_case_yaml_loads_cleanly() {
    // If a rename (criterion variant, reserved flag, serde tag)
    // lands without updating every YAML, `Case::load_dir` returns
    // an Err with the offending file and line. That's the signal
    // this test is designed to surface at PR time rather than at
    // "deploy + run" time.
    let dir = shipped_cases_dir();
    let cases = Case::load_dir(&dir).unwrap_or_else(|e| {
        panic!(
            "Case::load_dir({}) failed. If you renamed a Criterion variant, \
             a reserved-flag entry, or a required field, every shipped \
             cases/*.yaml must be updated in the same PR. Error: {e:#}",
            dir.display()
        )
    });

    // Sanity guard — we expect at least the fork_prefix + behavior +
    // selector + text cases. If this drops sharply, someone deleted
    // a pack without updating the guard.
    assert!(
        cases.len() >= 10,
        "shipped suite has {} cases; below the minimum sanity count of 10. \
         Did someone delete cases without updating this guard?",
        cases.len()
    );

    // Every case must have a prompt and a non-empty name. Loader
    // already requires them via serde, but this is defense-in-depth
    // for the "someone added a default" pattern.
    for c in &cases {
        assert!(!c.name.trim().is_empty(), "case with empty name loaded");
        assert!(
            !c.prompt.trim().is_empty(),
            "case {} has an empty prompt — models will see no instruction",
            c.name
        );
    }
}

#[test]
fn shipped_case_names_are_unique() {
    // Duplicate `name:` in two different YAML files would make the
    // report ambiguous and `astra-test` would happily run both.
    // Catch the duplicate at case-load time.
    let cases = Case::load_dir(&shipped_cases_dir()).expect("load_dir");
    let mut names: Vec<&str> = cases.iter().map(|c| c.name.as_str()).collect();
    names.sort();
    let before_dedup = names.len();
    names.dedup();
    let after_dedup = names.len();
    assert_eq!(
        before_dedup, after_dedup,
        "duplicate case names in shipped suite: {:?} reduced to {:?} after dedup",
        before_dedup, after_dedup
    );
}

// ── Class D regression: YAML criteria that don't match real CLI behavior ──

#[test]
fn text_rejects_hallucinated_file_claim_does_not_assert_exit_zero() {
    let case =
        Case::from_path(&shipped_cases_dir().join("text_rejects_hallucinated_file_claim.yaml"))
            .expect("load case");
    let has_exit_zero = case.criteria.iter().any(|c| {
        matches!(
            c,
            astra_test_harness::criteria::Criterion::ExitCode { code: 0 }
        )
    });
    assert!(
        !has_exit_zero,
        "text_rejects_hallucinated_file_claim must NOT assert exit_code=0. \
         A tool failure (read_file on nonexistent path) correctly exits 1. \
         The case should only check the judger criterion."
    );
}

#[test]
fn text_contains_simple_answer_has_adequate_timeout() {
    let case = Case::from_path(&shipped_cases_dir().join("text_contains_simple_answer.yaml"))
        .expect("load case");
    assert!(
        case.timeout_seconds >= 30,
        "text_contains_simple_answer timeout_seconds={} is too short; \
         a trivial factual question needs at least 30s.",
        case.timeout_seconds
    );
}

#[test]
fn fork_prefix_hit_e2e_tool_count_allows_model_retry() {
    let case = Case::from_path(&shipped_cases_dir().join("fork_prefix_hit_end_to_end.yaml"))
        .expect("load case");
    let max = case.criteria.iter().find_map(|c| match c {
        astra_test_harness::criteria::Criterion::ToolsCountBetween { max, .. } => Some(*max),
        _ => None,
    });
    let max = max
        .expect("fork_prefix_hit_end_to_end is missing a tools_count_between criterion entirely");
    assert!(
        max >= 6,
        "fork_prefix_hit_end_to_end tools_count max={max} is too strict; \
         models may retry spawn_agent. Needs >= 6.",
    );
}

#[test]
fn fork_prefix_spawn_inherits_uses_spawn_agent_not_delegate() {
    let case = Case::from_path(&shipped_cases_dir().join("fork_prefix_spawn_inherits.yaml"))
        .expect("load case");
    let requires_delegate = case.criteria.iter().any(|c| {
        matches!(c, astra_test_harness::criteria::Criterion::ToolCalled { name } if name == "delegate")
    });
    assert!(
        !requires_delegate,
        "fork_prefix_spawn_inherits must not require tool_called: delegate. \
         The delegate tool is not available in `astra chat` — it only exists \
         in the server-side DelegationEngine. Use spawn_agent instead."
    );
}

#[test]
fn shipped_cases_reference_only_known_criterion_variants() {
    // Positive sanity: when the serde tag on `Criterion` changes
    // (say, `fork_cache_outcome` → `fork_cache_class_v2`), every
    // case using the old tag fails to deserialize. `every_shipped_case_
    // yaml_loads_cleanly` above would already catch that. This test
    // adds an explicit-names check so a rename that kept the new
    // name in use is surfaced with a clearer error.
    //
    // Ground truth: the YAML `type:` values currently shipped. If
    // this list drifts the first test already fails; this test just
    // names the expected vocabulary for greppers.
    let known = [
        "exit_code",
        "tool_called",
        "tools_count_between",
        "stderr_matches",
        "text_contains",
        "session_event_count",
        "journal_tool_called",
        "fork_cache_outcome",
        "judger",
        "tokens_between",
        "duration_between",
        "tool_sequence",
        "turn_rounds_between",
        "cache_rate_above",
    ];
    // Concatenate every shipped YAML body and grep for `type:` tag
    // occurrences at column-1-after-indent. Cheap + doesn't require
    // reparsing.
    let dir = shipped_cases_dir();
    for entry in std::fs::read_dir(&dir).expect("read_dir") {
        let entry = entry.expect("entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml")
            && path.extension().and_then(|e| e.to_str()) != Some("yml")
        {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("read");
        for (lineno, line) in body.lines().enumerate() {
            // Match `- type: <name>` or `type: <name>` inside a list item.
            let trimmed = line
                .trim_start_matches(|c: char| c.is_whitespace() || c == '-')
                .trim();
            let Some(rest) = trimmed.strip_prefix("type:") else {
                continue;
            };
            let tag: String = rest
                .trim()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if tag.is_empty() {
                continue;
            }
            assert!(
                known.contains(&tag.as_str()),
                "{}:{} references unknown criterion tag {tag:?}. \
                 Known vocabulary: {known:?}. Update the test if the \
                 Criterion enum gained a legitimate new variant.",
                path.file_name().unwrap().to_string_lossy(),
                lineno + 1,
            );
        }
    }
}
