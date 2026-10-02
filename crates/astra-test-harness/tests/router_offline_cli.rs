use astra_services::model_routing::offline::*;
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

#[test]
fn qualification_cli_publishes_rejection_and_refuses_shadow_activation() {
    use astra_services::tuning::{RouterQualificationProtocol, router_evaluation_plan_sha256};
    use astra_turn_core::model_routing::offline::RouterTrainingConfig;
    let temp = tempfile::tempdir().unwrap();
    let (input_path, auth_path) = prepare(temp.path());
    let input: RouterDatasetInput =
        serde_json::from_slice(&fs::read(&input_path).unwrap()).unwrap();
    let hash = Command::new(env!("CARGO_BIN_EXE_astra-test"))
        .arg("router-config-hash")
        .output()
        .unwrap();
    assert!(hash.status.success());
    assert_eq!(
        String::from_utf8_lossy(&hash.stdout).trim(),
        content_sha256(&RouterTrainingConfig::default()).unwrap()
    );
    let protocol = RouterQualificationProtocol {
        schema_version: 1,
        job_id: "cli-qualification".into(),
        owner_id: input.manifest.owner_id.clone(),
        dataset_id: input.manifest.dataset_id.clone(),
        registered_at: "2024-01-01T00:00:00Z".parse().unwrap(),
        training_config_sha256: String::from_utf8(hash.stdout).unwrap().trim().into(),
        evaluation_plan_sha256: router_evaluation_plan_sha256(&input).unwrap(),
        minimum_test_groups: 1000,
        minimum_stratum_groups: 1000,
        minimum_pair_coverage: 0.95,
        maximum_quality_regression: 0.01,
        minimum_cost_saving_fraction: 0.2,
        maximum_episode_cost_usd: 1.0,
        maximum_p95_latency_ratio: 1.1,
        confidence: 0.95,
        required_strata: vec![input.sources[0].decision.features.unwrap()],
    };
    let protocol_path = temp.path().join("protocol.json");
    fs::write(&protocol_path, serde_json::to_vec(&protocol).unwrap()).unwrap();
    let output = temp.path().join("qualified");
    let command = |name: &str, output: &std::path::Path| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_astra-test"));
        c.args(["--astra-bin", "/no-live-service", name, "--input"])
            .arg(&input_path)
            .arg("--authorization")
            .arg(&auth_path)
            .arg("--protocol")
            .arg(&protocol_path)
            .arg("--output")
            .arg(output);
        c
    };
    let result = command("router-qualify", &output).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("qualification.json")).unwrap()).unwrap();
    assert_eq!(report["tuning"]["status"], "rejected");
    assert_eq!(report["tuning"]["production_qualified"], false);
    let complete: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("complete.json")).unwrap()).unwrap();
    assert_eq!(complete["source_ids"], report["tuning"]["source_ids"]);
    assert!(
        !command("router-qualify", &output)
            .output()
            .unwrap()
            .status
            .success()
    );
    let shadow_output = temp.path().join("shadow");
    let result = command("router-shadow", &shadow_output)
        .arg("--shadow-input")
        .arg(&input_path)
        .arg("--shadow-authorization")
        .arg(&auth_path)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("did not pass qualification"));
    assert!(!shadow_output.exists());
    let mut revoked: RouterDataAuthorization =
        serde_json::from_slice(&fs::read(&auth_path).unwrap()).unwrap();
    revoked
        .revoked_source_ids
        .push("strong-verification".into());
    fs::write(&auth_path, serde_json::to_vec(&revoked).unwrap()).unwrap();
    let denied = temp.path().join("revoked");
    assert!(
        !command("router-qualify", &denied)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(!denied.exists());
}

#[test]
fn public_qualification_and_shadow_workflow_scores_without_provider_io() {
    use astra_services::tuning::{RouterQualificationProtocol, router_evaluation_plan_sha256};
    use astra_turn_core::model_routing::offline::RouterTrainingConfig;
    use astra_turn_types::{TaskDifficulty, model_routing::ModelRoutingReason};
    let temp = tempfile::tempdir().unwrap();
    let mut input: RouterDatasetInput = serde_json::from_str(include_str!(
        "../../../fixtures/contracts/model_router_offline.json"
    ))
    .unwrap();
    let template = input.sources.remove(0);
    for (split, day, count) in [
        ("train", "2024-01-01", 8),
        ("validation", "2024-02-01", 8),
        ("test", "2024-03-01", 1200),
    ] {
        for i in 0..count {
            let mut source = template.clone();
            let id = format!("{split}-{i}");
            source.source_id = id.clone();
            source.decision.run_id = id.clone();
            source.decision.session_id = id.clone();
            source.group_keys = vec![id.clone()];
            source
                .decision
                .input_reference
                .as_mut()
                .unwrap()
                .prefix_root = id;
            source.decision.assessment.as_mut().unwrap().difficulty = TaskDifficulty::Moderate;
            source.decision.features.as_mut().unwrap().difficulty = TaskDifficulty::Moderate;
            source.decision.selected_offering_id = input.manifest.strong.offering_id.clone();
            source.decision.selected_contract_root = input.manifest.strong.contract_root.clone();
            source.decision.reason = ModelRoutingReason::StrongRequired;
            source.decision_at = format!("{day}T00:00:00Z").parse().unwrap();
            let pair = source.paired.as_mut().unwrap();
            for (name, arm) in [("economy", &mut pair.economy), ("strong", &mut pair.strong)] {
                arm.input_reference = source.decision.input_reference.clone().unwrap();
                arm.episode.execution_id = format!("{}-{name}", source.source_id);
                let replay_at =
                    source.decision_at + chrono::Duration::days(i64::from(split == "test") * 2);
                arm.episode.started_at = replay_at + chrono::Duration::seconds(1);
                arm.episode.completed_at = replay_at + chrono::Duration::seconds(5);
                let quality = arm.episode.quality.as_mut().unwrap();
                quality.target_execution_id = arm.episode.execution_id.clone();
                quality.assessed_at = replay_at + chrono::Duration::seconds(6);
            }
            input.sources.push(source);
        }
    }
    let (input_path, auth_path) = prepare_input(temp.path(), &input);
    let config = RouterTrainingConfig {
        minimum_training_groups: 4,
        minimum_validation_groups: 4,
        maximum_quality_regression: 0.01,
        quality_thresholds: vec![0.5],
    };
    let config_path = temp.path().join("config.json");
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let protocol = RouterQualificationProtocol {
        schema_version: 1,
        job_id: "public-qualification".into(),
        owner_id: input.manifest.owner_id.clone(),
        dataset_id: input.manifest.dataset_id.clone(),
        registered_at: "2024-03-02T00:00:00Z".parse().unwrap(),
        training_config_sha256: content_sha256(&config).unwrap(),
        evaluation_plan_sha256: router_evaluation_plan_sha256(&input).unwrap(),
        minimum_test_groups: 1000,
        minimum_stratum_groups: 1000,
        minimum_pair_coverage: 0.95,
        maximum_quality_regression: 0.1,
        minimum_cost_saving_fraction: 0.2,
        maximum_episode_cost_usd: 0.1,
        maximum_p95_latency_ratio: 1.1,
        confidence: 0.9,
        required_strata: vec![input.sources[0].decision.features.unwrap()],
    };
    let protocol_path = temp.path().join("protocol.json");
    fs::write(&protocol_path, serde_json::to_vec(&protocol).unwrap()).unwrap();
    let command = |name: &str, output: &std::path::Path| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_astra-test"));
        c.args(["--astra-bin", "/no-provider-or-runtime", name, "--input"])
            .arg(&input_path)
            .arg("--authorization")
            .arg(&auth_path)
            .arg("--protocol")
            .arg(&protocol_path)
            .arg("--config")
            .arg(&config_path)
            .arg("--output")
            .arg(output);
        c
    };
    let qualified = temp.path().join("qualified");
    let result = command("router-qualify", &qualified).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(qualified.join("qualification.json")).unwrap()).unwrap();
    assert_eq!(report["tuning"]["status"], "ready_for_shadow");
    let mut shadow = input.clone();
    shadow.manifest.dataset_id = "shadow-dataset".into();
    shadow.manifest.created_at += chrono::Duration::days(3);
    shadow.sources.truncate(1);
    let s = &mut shadow.sources[0];
    s.source_id = "new-source".into();
    s.decision.run_id = "new-run".into();
    s.decision.session_id = "new-session".into();
    s.group_keys = vec!["new-group".into()];
    s.decision.input_reference.as_mut().unwrap().prefix_root = "new-prefix".into();
    s.decision_at = input.manifest.created_at + chrono::Duration::days(1);
    s.paired = None;
    let shadow_dir = temp.path().join("shadow-input");
    fs::create_dir(&shadow_dir).unwrap();
    let (shadow_input, shadow_auth) = prepare_input(&shadow_dir, &shadow);
    let output = temp.path().join("shadow-output");
    let result = command("router-shadow", &output)
        .arg("--shadow-input")
        .arg(&shadow_input)
        .arg("--shadow-authorization")
        .arg(&shadow_auth)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("shadow.json")).unwrap()).unwrap();
    assert_eq!(result["mode"], "offline_shadow");
    assert_eq!(result["decisions"][0]["choice"], "economy");
    assert_eq!(result["disagreements"], 1);
    assert_eq!(
        result["tuning"]["candidate_sha256"],
        report["tuning"]["candidate_sha256"]
    );
    assert_eq!(result["tuning"]["production_qualified"], false);
    let complete: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("complete.json")).unwrap()).unwrap();
    assert!(
        complete["source_ids"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("new-source"))
    );
    assert!(
        complete["source_ids"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("strong-verification"))
    );
    // Known over-ceiling spending must reject even when that pair lacks a label.
    let mut costly = input.clone();
    let episode = &mut costly
        .sources
        .last_mut()
        .unwrap()
        .paired
        .as_mut()
        .unwrap()
        .economy
        .episode;
    episode.cost.as_mut().unwrap().total_usd = 100.0;
    episode.quality = None;
    prepare_input(temp.path(), &costly);
    let rejected = temp.path().join("over-ceiling");
    let result = command("router-qualify", &rejected).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(rejected.join("qualification.json")).unwrap()).unwrap();
    assert_eq!(report["tuning"]["status"], "rejected");
    assert!(
        report["cohorts"]["overall"]["pair_coverage"]
            .as_f64()
            .unwrap()
            > 0.95
    );
    assert!(
        report["cohorts"]["overall"]["failures"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(
                "episode_cost_exceeds_prespecified_bound"
            ))
    );

    // Register 600 incomplete March turns and 1,200 complete April turns. Moving
    // the test boundary past March must not turn this rejected cohort into a pass.
    let mut window = input;
    let incomplete_template = window.sources.last().unwrap().clone();
    for source in window
        .sources
        .iter_mut()
        .filter(|s| s.source_id.starts_with("test"))
    {
        source.decision_at += chrono::Duration::days(31);
        let pair = source.paired.as_mut().unwrap();
        for arm in [&mut pair.economy, &mut pair.strong] {
            arm.episode.started_at += chrono::Duration::days(31);
            arm.episode.completed_at += chrono::Duration::days(31);
            arm.episode.quality.as_mut().unwrap().assessed_at += chrono::Duration::days(31);
        }
    }
    for i in 0..600 {
        let mut early = incomplete_template.clone();
        let id = format!("incomplete-{i}");
        early.source_id = id.clone();
        early.decision.run_id = id.clone();
        early.decision.session_id = id.clone();
        early.decision.input_reference.as_mut().unwrap().prefix_root = id.clone();
        early.group_keys = vec![id];
        early.paired = None;
        window.sources.push(early);
    }
    prepare_input(temp.path(), &window);
    let plan = Command::new(env!("CARGO_BIN_EXE_astra-test"))
        .args(["router-plan-hash", "--input"])
        .arg(&input_path)
        .output()
        .unwrap();
    assert!(plan.status.success());
    let mut protocol = protocol;
    protocol.registered_at = "2024-04-02T00:00:00Z".parse().unwrap();
    protocol.evaluation_plan_sha256 = String::from_utf8(plan.stdout).unwrap().trim().into();
    assert_eq!(
        protocol.evaluation_plan_sha256,
        router_evaluation_plan_sha256(&window).unwrap()
    );
    fs::write(&protocol_path, serde_json::to_vec(&protocol).unwrap()).unwrap();
    let original = temp.path().join("original-window");
    assert!(
        command("router-qualify", &original)
            .output()
            .unwrap()
            .status
            .success()
    );
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(original.join("qualification.json")).unwrap()).unwrap();
    assert_eq!(report["tuning"]["status"], "rejected");
    assert!(
        report["cohorts"]["overall"]["pair_coverage"]
            .as_f64()
            .unwrap()
            < 0.95
    );
    window.manifest.validation_before += chrono::Duration::days(31);
    fs::write(&input_path, serde_json::to_vec(&window).unwrap()).unwrap();
    let denied = temp.path().join("changed-window");
    let result = command("router-qualify", &denied).output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("evaluation plan differs"));
    assert!(!denied.exists());
}
