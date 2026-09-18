use astra_services::evaluation::router::*;
use std::{fs, process::Command};

fn prepare(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let input: RouterDatasetInput = serde_json::from_str(include_str!(
        "../../../fixtures/contracts/model_router_offline.json"
    ))
    .unwrap();
    prepare_input(dir, &input)
}
fn prepare_input(
    dir: &std::path::Path,
    input: &RouterDatasetInput,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let input_path = dir.join("input.json");
    fs::write(&input_path, serde_json::to_vec(&input).unwrap()).unwrap();
    // Exercise the same digest command an operator uses before approving data.
    let hashes = Command::new(env!("CARGO_BIN_EXE_astra-test"))
        .args(["router-source-hashes", "--input"])
        .arg(&input_path)
        .output()
        .unwrap();
    assert!(
        hashes.status.success(),
        "{}",
        String::from_utf8_lossy(&hashes.stderr)
    );
    let authorization = RouterDataAuthorization {
        dataset_id: input.manifest.dataset_id.clone(),
        owner_id: input.manifest.owner_id.clone(),
        target_use: TARGET_USE.into(),
        redaction_version: input.manifest.redaction_version.clone(),
        expires_at: input.manifest.expires_at,
        approved_sources: serde_json::from_slice(&hashes.stdout).unwrap(),
        revoked_source_ids: vec![],
    };
    let auth_path = dir.join("authorization.json");
    fs::write(&auth_path, serde_json::to_vec(&authorization).unwrap()).unwrap();
    (input_path, auth_path)
}
fn run(
    input: &std::path::Path,
    auth: &std::path::Path,
    output: &std::path::Path,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_astra-test"))
        .args([
            "--astra-bin",
            "/no-provider-or-runtime-needed",
            "router-offline",
            "--input",
        ])
        .arg(input)
        .arg("--authorization")
        .arg(auth)
        .arg("--output")
        .arg(output)
        .output()
        .unwrap()
}
#[test]
fn offline_cli_builds_reviewable_artifacts_without_live_services() {
    let temp = tempfile::tempdir().unwrap();
    let (input, auth) = prepare(temp.path());
    let output = temp.path().join("result");
    let result = run(&input, &auth, &output);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["production_qualified"], false);
    assert_eq!(report["coverage"]["train"]["complete_paired_groups"], 1);
    let complete: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("complete.json")).unwrap()).unwrap();
    assert_eq!(complete["dataset_sha256"], report["dataset_sha256"]);
    let examples = fs::read_to_string(output.join("examples.jsonl")).unwrap();
    assert!(!examples.contains("work_admission"));
    assert!(output.join("candidate.json").exists());
    assert!(!run(&input, &auth, &output).status.success());
    assert_eq!(
        fs::read_to_string(output.join("examples.jsonl")).unwrap(),
        examples
    );
}
#[test]
fn offline_cli_denies_revoked_and_mismatched_data_before_writing() {
    let temp = tempfile::tempdir().unwrap();
    let (input, auth) = prepare(temp.path());
    let mut authorization: RouterDataAuthorization =
        serde_json::from_slice(&fs::read(&auth).unwrap()).unwrap();
    authorization.revoked_source_ids.push("source-1".into());
    fs::write(&auth, serde_json::to_vec(&authorization).unwrap()).unwrap();
    let output = temp.path().join("revoked-result");
    assert!(!run(&input, &auth, &output).status.success());
    assert!(!output.exists());
    authorization.revoked_source_ids.clear();
    fs::write(&auth, serde_json::to_vec(&authorization).unwrap()).unwrap();
    let mut changed: RouterDatasetInput =
        serde_json::from_slice(&fs::read(&input).unwrap()).unwrap();
    changed.sources[0]
        .paired
        .as_mut()
        .unwrap()
        .strong
        .snapshot_root = "wrong-snapshot".into();
    fs::write(&input, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(!run(&input, &auth, &output).status.success());
    assert!(!output.exists());
}

#[test]
fn offline_cli_keeps_full_observed_timing_and_denies_revoked_nested_evidence() {
    let mut input: RouterDatasetInput = serde_json::from_str(include_str!(
        "../../../fixtures/contracts/model_router_offline.json"
    ))
    .unwrap();
    let source = &mut input.sources[0];
    let mut observed = source.paired.as_ref().unwrap().economy.episode.clone();
    observed.execution_id = "original-execution".into();
    observed.started_at = source.decision_at - chrono::Duration::seconds(5);
    observed.quality.as_mut().unwrap().target_execution_id = observed.execution_id.clone();
    observed.quality.as_mut().unwrap().evidence_ids = vec!["observed-verification".into()];
    source.followup = Some(RouterFollowup {
        source_id: "withdrawn-followup".into(),
        observed_at: observed.completed_at + chrono::Duration::seconds(5),
        response_reference: observed.response_reference.clone().unwrap(),
        assessment: astra_turn_types::TurnAssessment {
            feedback_relation: astra_turn_types::FeedbackResponseRelation::PreviousResponse,
            satisfaction: astra_turn_types::ResponseSatisfaction::Dissatisfied,
            ..Default::default()
        },
    });
    source.observed = Some(observed.clone());
    let temp = tempfile::tempdir().unwrap();
    let (input_path, auth_path) = prepare_input(temp.path(), &input);
    let output = temp.path().join("result");
    let result = run(&input_path, &auth_path, &output);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let examples = fs::read_to_string(output.join("examples.jsonl")).unwrap();
    let example: RouterExample = serde_json::from_str(examples.trim()).unwrap();
    assert_eq!(example.observed.as_ref(), Some(&observed));
    let complete: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("complete.json")).unwrap()).unwrap();
    let lineage: std::collections::BTreeSet<String> =
        serde_json::from_value(complete["source_ids"].clone()).unwrap();
    assert_eq!(
        lineage,
        [
            "source-1",
            "withdrawn-followup",
            "original-execution",
            "observed-verification",
            "economy-execution",
            "strong-execution",
            "economy-verification",
            "strong-verification",
            "synthetic-environment-root"
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    );
    let approved: RouterDataAuthorization =
        serde_json::from_slice(&fs::read(&auth_path).unwrap()).unwrap();
    for id in [
        "withdrawn-followup",
        "observed-verification",
        "economy-verification",
        "strong-verification",
        "original-execution",
        "synthetic-environment-root",
    ] {
        let mut revoked = approved.clone();
        revoked.revoked_source_ids.push(id.into());
        fs::write(&auth_path, serde_json::to_vec(&revoked).unwrap()).unwrap();
        let denied = temp.path().join(format!("denied-{id}"));
        let result = run(&input_path, &auth_path, &denied);
        assert!(!result.status.success(), "{id}");
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("Referenced routing evidence was deleted or revoked"),
            "{id}"
        );
        assert!(!denied.exists());
    }
}
