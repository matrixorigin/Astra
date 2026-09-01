use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

const BASELINE: &str = include_str!("../../../docs/product/session-product-m0/baseline.json");
const QUALITY_CORPUS: &str =
    include_str!("../../../docs/product/session-product-m0/quality-corpus.json");
const ONLINE_VALIDATION: &str =
    include_str!("../../../docs/product/session-product-m0/online-validation.json");

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn baseline() -> Value {
    serde_json::from_str(BASELINE).expect("M0 baseline must be valid JSON")
}

fn required_str<'a>(value: &'a Value, pointer: &str) -> &'a str {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("M0 baseline requires string at {pointer}"))
}

fn required_array<'a>(value: &'a Value, pointer: &str) -> &'a Vec<Value> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("M0 baseline requires array at {pointer}"))
}

fn collect_source_paths(value: &Value, output: &mut BTreeSet<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_source_paths(value, output);
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                match key.as_str() {
                    "path" => {
                        if let Some(path) = value.as_str() {
                            output.insert(path.to_string());
                        }
                    }
                    "sources" | "existing_lanes" => {
                        for path in value.as_array().into_iter().flatten() {
                            if let Some(path) = path.as_str() {
                                output.insert(path.to_string());
                            }
                        }
                    }
                    _ => collect_source_paths(value, output),
                }
            }
        }
        _ => {}
    }
}

#[test]
fn m0_baseline_has_independent_complete_ledgers_and_fixed_decisions() {
    let baseline = baseline();
    assert_eq!(
        required_str(&baseline, "/schema_version"),
        "astra.session-product.m0.v1"
    );
    assert_eq!(required_str(&baseline, "/as_of"), "2026-08-03");

    let allowed_statuses = required_array(&baseline, "/status_vocabulary")
        .iter()
        .map(|value| value.as_str().expect("status must be a string"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        allowed_statuses,
        BTreeSet::from(["missing", "obsolete", "partial", "proven"])
    );

    let tracks = required_array(&baseline, "/tracks");
    let track_ids = tracks
        .iter()
        .map(|track| required_str(track, "/id"))
        .collect::<BTreeSet<_>>();
    assert_eq!(track_ids, BTreeSet::from(["A", "B", "C"]));

    for track in tracks {
        let track_id = required_str(track, "/id");
        let gates = required_array(track, "/gates");
        assert!(!gates.is_empty(), "Track {track_id} must have gates");
        let mut gate_ids = BTreeSet::new();
        for gate in gates {
            let gate_id = required_str(gate, "/id");
            assert!(
                gate_ids.insert(gate_id),
                "duplicate Track {track_id} gate {gate_id}"
            );
            let status = required_str(gate, "/status");
            assert!(allowed_statuses.contains(status), "unknown status {status}");
            let levels = gate
                .get("levels")
                .and_then(Value::as_object)
                .expect("gate levels");
            let implemented = levels["implemented"].as_bool().expect("implemented bool");
            let integrated = levels["integrated"].as_bool().expect("integrated bool");
            let user_usable = levels["user_usable"].as_bool().expect("user_usable bool");
            let proven = levels["proven"].as_bool().expect("proven bool");
            assert!(
                !integrated || implemented,
                "integration requires implementation"
            );
            assert!(
                !user_usable || integrated,
                "user usability requires integration"
            );
            assert!(!proven || user_usable, "proof requires a user-usable path");

            let evidence = required_array(gate, "/evidence");
            let gaps = required_array(gate, "/gaps");
            if status == "proven" {
                assert!(proven, "proven gate must set levels.proven");
                assert!(gaps.is_empty(), "proven gate cannot retain gaps");
                assert!(!evidence.is_empty(), "proven gate requires evidence");
            } else {
                assert!(!proven, "non-proven gate cannot set levels.proven");
                assert!(!gaps.is_empty(), "non-proven gate must name its gaps");
            }

            for item in evidence {
                let kind = required_str(item, "/kind");
                assert!(
                    matches!(kind, "source" | "test" | "benchmark" | "artifact"),
                    "unsupported evidence kind {kind}"
                );
                assert!(!required_str(item, "/claim").trim().is_empty());
                if matches!(kind, "test" | "benchmark") {
                    assert!(
                        !required_str(item, "/command").trim().is_empty(),
                        "repeatable evidence requires a command"
                    );
                }
            }
        }
    }

    let phase_ids = tracks
        .iter()
        .find(|track| required_str(track, "/id") == "A")
        .and_then(|track| track.get("gates"))
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|gate| required_str(gate, "/id"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        phase_ids,
        BTreeSet::from([
            "phase-1-live-continuity",
            "phase-2-causal-resume",
            "phase-3-shared-coordinator-topology-parity",
            "phase-4-cross-device-handoff",
            "phase-5-durable-cow-fork",
            "phase-6-context-and-cache-efficiency",
            "phase-7-product-observability",
        ])
    );

    let authorities = required_array(&baseline, "/identity_and_authority");
    let authority_domains = authorities
        .iter()
        .map(|entry| required_str(entry, "/domain"))
        .collect::<BTreeSet<_>>();
    assert_eq!(authority_domains.len(), authorities.len());
    let work = authorities
        .iter()
        .find(|entry| required_str(entry, "/domain") == "Work")
        .expect("Work authority decision");
    assert_eq!(required_str(work, "/identity"), "work_id");
    assert_eq!(
        required_str(work, "/branch_identity"),
        "work_id + branch_id"
    );
    assert_eq!(
        required_str(work, "/canonical_owner"),
        "DatabaseWorkRepository"
    );

    let work_resources = required_array(&baseline, "/work_repository_manifest/resources")
        .iter()
        .map(|resource| required_str(resource, "/name"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        work_resources,
        BTreeSet::from([
            "AcceptanceDecision",
            "CheckRun",
            "CriterionSetRevision",
            "Edge",
            "GoalRevision",
            "GraphRevision",
            "ItemRevision",
            "ReadReceipt",
            "Work",
            "WorkBranch",
            "WorkEvent",
        ])
    );
    assert_eq!(
        required_str(
            &baseline,
            "/work_repository_manifest/active_state_migration"
        ),
        "forbidden"
    );

    let legacy_stores = required_array(&baseline, "/legacy_authority_matrix")
        .iter()
        .map(|entry| required_str(entry, "/store"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        legacy_stores,
        BTreeSet::from([
            "astra-plan execution authority",
            "checks",
            "child run",
            "task contract",
        ])
    );
    for entry in required_array(&baseline, "/legacy_authority_matrix") {
        for field in [
            "current_identity",
            "current_revision",
            "current_mutation_owner",
            "offline_archive",
            "drop",
            "replacement",
        ] {
            assert!(
                !required_str(entry, &format!("/{field}")).trim().is_empty(),
                "legacy authority row requires {field}"
            );
        }
        assert!(!required_array(entry, "/reusable_invariants").is_empty());
    }

    let topology_ids = required_array(&baseline, "/topologies")
        .iter()
        .map(|entry| required_str(entry, "/id"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        topology_ids,
        BTreeSet::from([
            "cli-plus-server-current",
            "edge-plus-server-current",
            "offline-cli-current",
            "server-only-current",
        ])
    );

    let control_capabilities = required_array(&baseline, "/control_semantics_audit")
        .iter()
        .map(|entry| required_str(entry, "/capability"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        control_capabilities,
        BTreeSet::from(["introspect", "policy", "reflection", "trace"])
    );
    let reflection = required_array(&baseline, "/control_semantics_audit")
        .iter()
        .find(|entry| required_str(entry, "/capability") == "reflection")
        .unwrap();
    assert_eq!(
        reflection["text_or_like_controls_behavior"].as_bool(),
        Some(true),
        "M0 must keep the legacy reflection text/LIKE defect visible until M7 removes it"
    );

    let supported_tasks = required_array(&baseline, "/support_envelope/v1_tasks")
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        supported_tasks,
        BTreeSet::from(["cross-file feature", "failing-test or CI repair"])
    );
    assert_eq!(required_array(&baseline, "/quality_corpus").len(), 2);
    assert_eq!(
        required_array(&baseline, "/product_ia/duplicate_or_competing_surfaces").len(),
        2,
        "the baseline must be updated when a competing product authority is removed"
    );
    assert!(required_array(&baseline, "/product_ia/broken_journeys").len() >= 3);
    assert!(required_array(&baseline, "/product_ia/implementation_terms_to_demote").len() >= 5);

    assert_eq!(
        required_array(&baseline, "/benchmark_manifest/matrix/context_windows"),
        &[
            Value::from(131_072),
            Value::from(204_800),
            Value::from(1_000_000)
        ]
    );
    assert_eq!(
        required_array(&baseline, "/benchmark_manifest/matrix/turn_counts"),
        &[Value::from(10), Value::from(100)]
    );
    assert_eq!(
        required_array(&baseline, "/benchmark_manifest/matrix/tenants"),
        &[Value::from(1), Value::from(100)]
    );
    assert_eq!(
        baseline.pointer("/benchmark_manifest/targets/load_head/p95_ms"),
        Some(&Value::from(50))
    );
    assert_eq!(
        baseline.pointer("/benchmark_manifest/targets/correctness_failures"),
        Some(&Value::from(0))
    );
    assert!(
        required_array(&baseline, "/benchmark_manifest/scale_invariants").len() >= 5,
        "M0 must fix the long-session and multi-tenant scale invariants"
    );
    for forbidden in [
        "active_state_migrator",
        "compatibility_adapter",
        "dual_reader_or_writer",
        "legacy_data_fallback",
    ] {
        assert_eq!(
            required_str(&baseline, &format!("/cutover_policy/{forbidden}")),
            "forbidden"
        );
    }
}

#[test]
fn m0_quality_corpus_is_versioned_real_repo_input_with_a_fixed_control_arm() {
    let corpus: Value =
        serde_json::from_str(QUALITY_CORPUS).expect("M0 quality corpus must be valid JSON");
    assert_eq!(
        required_str(&corpus, "/schema_version"),
        "astra.session-product.quality-corpus.v1"
    );
    assert_eq!(required_str(&corpus, "/repository/kind"), "git");
    let base_revision = required_str(&corpus, "/repository/base_revision");
    assert_eq!(
        base_revision.len(),
        40,
        "quality corpus must pin a full Git revision"
    );
    assert!(
        base_revision
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    );
    assert_eq!(required_str(&corpus, "/baseline_arm/id"), "no-reflection");
    assert_eq!(
        required_str(&corpus, "/baseline_arm/reflection"),
        "disabled"
    );
    assert_eq!(
        required_str(&corpus, "/baseline_arm/automatic_correction"),
        "disabled"
    );
    assert_eq!(
        required_str(&corpus, "/baseline_arm/provider_correctness"),
        "deterministic fixture"
    );

    let root = repository_root();
    let cases = required_array(&corpus, "/cases");
    assert_eq!(cases.len(), 2);
    let case_classes = cases
        .iter()
        .map(|case| required_str(case, "/task_class"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        case_classes,
        BTreeSet::from(["cross-file feature", "failing-test or CI repair"])
    );
    for case in cases {
        assert!(!required_str(case, "/task").trim().is_empty());
        assert!(!required_array(case, "/required_outcomes").is_empty());
        assert!(!required_array(case, "/unhappy_worlds").is_empty());
        for entry_point in required_array(case, "/entry_points") {
            let relative = entry_point.as_str().expect("entry point path string");
            assert!(
                root.join(relative).exists(),
                "quality corpus entry point is stale: {relative}"
            );
        }
    }
}

#[test]
fn m0_online_validation_keeps_model_evidence_advisory() {
    let validation: Value = serde_json::from_str(ONLINE_VALIDATION)
        .expect("M0 online validation receipt must be valid JSON");
    assert_eq!(required_str(&validation, "/result"), "passed");
    assert_eq!(required_str(&validation, "/model"), "deepseek-v4-flash");
    assert_eq!(
        validation
            .pointer("/model_judgement/same_family_judger")
            .and_then(Value::as_bool),
        Some(true),
        "the same-family judging limitation must remain explicit"
    );
    let assertions = required_array(&validation, "/structural_assertions");
    assert_eq!(assertions.len(), 6);
    assert!(
        assertions
            .iter()
            .all(|assertion| { assertion.get("passed").and_then(Value::as_bool) == Some(true) })
    );
    assert_eq!(
        required_str(&validation, "/evidence_class"),
        "online-integration"
    );
    assert_eq!(
        validation
            .pointer("/correctness_authority")
            .and_then(Value::as_bool),
        Some(false),
        "online provider evidence cannot become correctness authority"
    );
}

#[test]
fn m0_evidence_cleanup_and_ia_references_cannot_silently_rot() {
    let baseline = baseline();
    let root = repository_root();
    let mut paths = BTreeSet::new();
    collect_source_paths(&baseline, &mut paths);
    assert!(
        paths.len() >= 40,
        "M0 baseline must reference concrete evidence"
    );
    for relative in paths {
        let path = Path::new(&relative);
        assert!(
            path.is_relative(),
            "M0 source path must be relative: {relative}"
        );
        assert!(
            !relative.split('/').any(|part| part == ".."),
            "M0 source path cannot escape the repository: {relative}"
        );
        assert!(
            root.join(path).exists(),
            "M0 source path is stale: {relative}"
        );
    }

    for track in required_array(&baseline, "/tracks") {
        for gate in required_array(track, "/gates") {
            for evidence in required_array(gate, "/evidence") {
                let Some(symbol) = evidence.get("symbol").and_then(Value::as_str) else {
                    continue;
                };
                let relative = required_str(evidence, "/path");
                let source = std::fs::read_to_string(root.join(relative))
                    .unwrap_or_else(|error| panic!("read M0 evidence {relative}: {error}"));
                assert!(
                    source.contains(symbol),
                    "M0 evidence symbol {symbol} disappeared from {relative}"
                );
            }
        }
    }

    let cleanup = required_array(&baseline, "/cleanup_manifest");
    let mut cleanup_ids = BTreeSet::new();
    for entry in cleanup {
        let id = required_str(entry, "/id");
        assert!(cleanup_ids.insert(id), "duplicate cleanup entry {id}");
        let status = required_str(entry, "/status");
        assert!(matches!(status, "present" | "removed"));
        assert!(matches!(
            required_str(entry, "/disposition"),
            "delete" | "archive-and-drop"
        ));
        assert!(matches!(
            required_str(entry, "/owner_milestone"),
            "M2" | "M5" | "M7" | "M9"
        ));
        let sources = required_array(entry, "/sources");
        assert!(!sources.is_empty(), "cleanup entry {id} needs source proof");
        if status == "removed" {
            assert!(
                entry
                    .get("absence_assertion")
                    .and_then(Value::as_str)
                    .is_some(),
                "removed cleanup entry {id} requires an absence assertion"
            );
        } else {
            for source in sources {
                let source = source.as_str().expect("cleanup source string");
                assert!(
                    root.join(source).exists(),
                    "present cleanup target {id} no longer exists; update it atomically with an absence assertion"
                );
            }
        }
    }
}

#[test]
fn m2_removed_conversation_authorities_are_physically_absent() {
    let root = repository_root();
    for relative in [
        "crates/runtime/src/turn/bridge/mod.rs",
        "crates/runtime/src/turn/bridge/inprocess.rs",
        "crates/services/src/session_fork.rs",
        "crates/astra-cli/src/cli/chat_stream/sse_loop/cli_loop_host.rs",
        "crates/runtime/src/server/plan_handlers.rs",
        "crates/runtime/src/server/task_handlers.rs",
        "crates/astra-cli/src/cli/task/task_queue_command.rs",
        "crates/astra-cli/src/cli/task/task_worker_support.rs",
        "crates/runtime/src/server/session/session_todo_handlers.rs",
        "crates/runtime/src/server/session/session_todo_sweeper.rs",
        "crates/astra-tools/src/task_mgmt.rs",
        "crates/astra-tools/src/task_mgmt_matrixone.rs",
        "crates/runtime/src/server/tool_task_runtime.rs",
        "crates/astra-cli/src/edge_tools/tests/task_tests.rs",
        "crates/astra-test-harness/cases/session_todos_create_and_list.yaml",
    ] {
        assert!(
            !root.join(relative).exists(),
            "removed authority path must not return: {relative}"
        );
    }

    for (relative, forbidden) in [
        (
            "crates/services/src/lib.rs",
            "FileSessionContextCoordinator",
        ),
        (
            "crates/services/src/session_context_coordinator.rs",
            "struct FileSessionContextCoordinator",
        ),
        (
            "crates/astra-cli/src/cli/slash/slash_session.rs",
            "fork_session_into_state",
        ),
        ("crates/astra-cli/src/tui/slash_dispatch.rs", "ForkPicker"),
        (
            "crates/astra-cli/src/cli/chat_stream/sse_loop/server_admission_host.rs",
            "CliAgenticLoopHost",
        ),
        (
            "crates/runtime/src/turn/agentic_loop/host.rs",
            "struct TaskBoardSnapshot",
        ),
        (
            "crates/runtime/src/turn/agentic_loop/host.rs",
            "TaskBoardAdvisory",
        ),
        (
            "crates/runtime/src/turn/agentic_loop/host.rs",
            "task_board_monitor",
        ),
        (
            "crates/core/src/observation_journal.rs",
            "struct TaskSnapshot",
        ),
        (
            "crates/core/src/observation_journal.rs",
            "task_completion_ratio",
        ),
        (
            "crates/runtime/src/turn/runtime_policy.rs",
            "PhaseTransitionSuggested",
        ),
        (
            "crates/astra-turn-core/src/loop_circuit_breaker.rs",
            "CompletionObserved",
        ),
        (
            "crates/astra-turn-core/src/loop_circuit_breaker.rs",
            "task_completed",
        ),
        (
            "crates/runtime/src/turn/agentic_loop/execution_phase.rs",
            "completion_observation_payload",
        ),
        ("crates/astra-cli/src/edge_tools.rs", "\"task_board\" =>"),
        (
            "crates/astra-cli/src/edge_tools.rs",
            "execute_task_tool_args",
        ),
        (
            "crates/astra-cli/src/cli/chat_stream/sse_loop/agentic_loop_turn.rs",
            "plan_resume_hint",
        ),
        (
            "crates/astra-tools/src/executor.rs",
            "task_manager: Arc<TaskManager>",
        ),
        (
            "crates/services/src/storage.rs",
            "CREATE TABLE IF NOT EXISTS session_todos",
        ),
        (
            "crates/services/src/storage.rs",
            "CREATE TABLE IF NOT EXISTS session_todo_counters",
        ),
        (
            "crates/services/src/storage.rs",
            "CREATE TABLE IF NOT EXISTS session_todo_idempotency",
        ),
    ] {
        let source = std::fs::read_to_string(root.join(relative))
            .unwrap_or_else(|error| panic!("read source absence target {relative}: {error}"));
        assert!(
            !source.contains(forbidden),
            "removed authority symbol {forbidden:?} must not return in {relative}"
        );
    }
}
