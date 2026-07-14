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

// text_rejects_hallucinated_file_claim removed — merged into anti_hallucination_two_vectors

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
fn shipped_case_criteria_round_trip_through_real_serde() {
    // Criterion's Serde definition is the only variant vocabulary. A second
    // string whitelist inevitably rejects the next legitimate variant (as it
    // did for hard_judger) or accepts a removed one. Round-tripping every real
    // shipped instance also checks that the read and write contracts agree,
    // without treating YAML layout or unrelated `type:` fields as criteria.
    let cases = Case::load_dir(&shipped_cases_dir()).expect("load shipped cases");
    for case in cases {
        let criteria = case
            .criteria
            .iter()
            .chain(case.steps.iter().flat_map(|step| step.criteria.iter()));
        for criterion in criteria {
            let yaml = serde_yaml_ng::to_string(criterion)
                .unwrap_or_else(|error| panic!("{}: serialize criterion: {error}", case.name));
            serde_yaml_ng::from_str::<astra_test_harness::criteria::Criterion>(&yaml)
                .unwrap_or_else(|error| {
                    panic!(
                        "{}: deserialize serialized criterion: {error}\n{yaml}",
                        case.name
                    )
                });
        }
    }
}
