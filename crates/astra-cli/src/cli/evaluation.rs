//! Thin CLI orchestration for the canonical Evaluation control plane.
//!
//! This module owns no experiment state and never starts a completion itself.
//! It submits a frozen intent, follows the owner-scoped projection, and asks
//! the server to assess durable evidence through the same endpoints used by
//! the Web surface.

use super::cli_config::cli_args::{EvaluationCmd, EvaluationExperimentArgs, EvaluationRunArgs};
use super::cli_config::cli_utils::{map_thin_err, print_json_or_raw};
use astra_services::evaluation::{
    EvaluationExperimentPrepareRequest, EvaluationExperimentPrepareResponse,
    EvaluationExperimentProjection, EvaluationTrialLifecycle, EvaluationTrialStartResponse,
    TaskAssessmentResult,
};
use astra_thin_client::{ThinClient, paths};
use serde::de::DeserializeOwned;
use serde_json::json;
use std::time::{Duration, Instant};

pub(crate) async fn run(
    api: &ThinClient,
    token: &str,
    command: EvaluationCmd,
) -> Result<(), String> {
    match command {
        EvaluationCmd::Run(args) => run_experiment(api, token, &args).await,
        EvaluationCmd::Show(args) => {
            let path = experiment_path(&args)?;
            let body = api
                .get_bearer_path_query_text(token, &path, &[])
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        EvaluationCmd::Report(args) => {
            let path = paths::evaluation_report(&args.experiment_id)
                .ok_or_else(|| "experiment_id must be a nonempty safe path segment".to_string())?;
            let body = api
                .get_bearer_path_query_text(token, &path, &[])
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
    }
}

async fn run_experiment(
    api: &ThinClient,
    token: &str,
    args: &EvaluationRunArgs,
) -> Result<(), String> {
    let request = read_intent(&args.intent)?;
    let request_bytes = serde_json::to_value(&request).map_err(|error| error.to_string())?;
    let prepared_body = api
        .post_bearer_path_json_text(token, paths::EVALUATION_PREPARE, &request_bytes)
        .await
        .map_err(map_thin_err)?;
    let prepared: EvaluationExperimentPrepareResponse = decode_json(&prepared_body, "prepare")?;
    let default_wait_secs = request
        .max_wall_time_secs
        .checked_mul(prepared.trials.len() as u64)
        .and_then(|seconds| seconds.checked_add(60))
        .ok_or("evaluation wait budget overflows")?;
    let wait_secs = args.wait_secs.unwrap_or(default_wait_secs);
    let report = run_prepared_evaluation(api, token, prepared, wait_secs, args.poll_ms)
        .await
        .map_err(|error| {
            format!(
                "{error}; resume with `astra evaluation run {}` using the same intent file",
                args.intent.display()
            )
        })?;
    print_json_or_raw(&report);
    Ok(())
}

pub(crate) async fn run_prepared_evaluation(
    api: &ThinClient,
    token: &str,
    prepared: EvaluationExperimentPrepareResponse,
    wait_secs: u64,
    poll_ms: u64,
) -> Result<String, String> {
    let experiment_id = prepared.experiment.experiment_id.clone();
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(wait_secs))
        .ok_or("evaluation wait budget is too large")?;

    eprintln!(
        "Evaluation {} · {} trials · waiting up to {}s",
        experiment_id,
        prepared.trials.len(),
        wait_secs
    );

    let mut trials = prepared.trials;
    trials.sort_by_key(|binding| binding.trial.sequence);
    let trial_ids = trials
        .iter()
        .map(|binding| binding.trial_id.clone())
        .collect::<Vec<_>>();
    for binding in trials {
        let trial_id = binding.trial_id.clone();
        if binding.binding_status == "planned" {
            let path = paths::evaluation_trial_start(&experiment_id, &trial_id)
                .ok_or_else(|| format!("invalid evaluation trial identity: {trial_id}"))?;
            let body = api
                .post_bearer_path_json_text(token, &path, &json!({}))
                .await
                .map_err(map_thin_err)?;
            let started: EvaluationTrialStartResponse = decode_json(&body, "trial start")?;
            eprintln!(
                "  started {} ({:?}) → run {}",
                trial_id, binding.trial.arm, started.run_id
            );
        } else if binding.binding_status == "bound" {
            eprintln!("  resuming {}", trial_id);
        } else {
            return Err(format!(
                "experiment {experiment_id} has unsupported trial binding status `{}` for {trial_id}",
                binding.binding_status
            ));
        }

        wait_for_terminal_trial(api, token, &experiment_id, &trial_id, deadline, poll_ms)
            .await
            .map_err(|error| {
                format!("evaluation {experiment_id} stopped at trial {trial_id}: {error}")
            })?;
        try_assess_trial(api, token, &experiment_id, &trial_id).await?;
    }

    // Observation is the arm-order barrier. Assessments are a read/verify
    // phase and may still be pending while the next arm runs; waiting here
    // preserves the frozen baseline-first execution order without serializing
    // unrelated verifier work.
    for trial_id in trial_ids {
        assess_trial(api, token, &experiment_id, &trial_id, deadline, poll_ms).await?;
    }

    let report_path = paths::evaluation_report(&experiment_id)
        .ok_or_else(|| format!("invalid evaluation experiment identity: {experiment_id}"))?;
    let report = api
        .get_bearer_path_query_text(token, &report_path, &[])
        .await
        .map_err(map_thin_err)?;
    Ok(report)
}

async fn try_assess_trial(
    api: &ThinClient,
    token: &str,
    experiment_id: &str,
    trial_id: &str,
) -> Result<(), String> {
    let path = paths::evaluation_trial_assess(experiment_id, trial_id)
        .ok_or_else(|| format!("invalid evaluation trial identity: {trial_id}"))?;
    let body = api
        .post_bearer_path_empty_text(token, &path)
        .await
        .map_err(map_thin_err)?;
    let result: TaskAssessmentResult = decode_json(&body, "assessment")?;
    if let TaskAssessmentResult::Recorded(record) = result {
        eprintln!("  assessed {} → {:?}", trial_id, record.outcome);
    }
    Ok(())
}

fn read_intent(path: &std::path::Path) -> Result<EvaluationExperimentPrepareRequest, String> {
    let bytes = std::fs::read(path).map_err(|error| {
        format!(
            "could not read Evaluation intent {}: {error}",
            path.display()
        )
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid Evaluation intent {}: {error}", path.display()))
}

fn decode_json<T: DeserializeOwned>(body: &str, operation: &str) -> Result<T, String> {
    serde_json::from_str(body).map_err(|error| {
        format!("server returned invalid JSON for Evaluation {operation}: {error}")
    })
}

fn experiment_path(args: &EvaluationExperimentArgs) -> Result<String, String> {
    paths::evaluation_experiment(&args.experiment_id)
        .ok_or_else(|| "experiment_id must be a nonempty safe path segment".to_string())
}

async fn wait_for_terminal_trial(
    api: &ThinClient,
    token: &str,
    experiment_id: &str,
    trial_id: &str,
    deadline: Instant,
    poll_ms: u64,
) -> Result<(), String> {
    let path = paths::evaluation_experiment(experiment_id)
        .ok_or_else(|| format!("invalid evaluation experiment identity: {experiment_id}"))?;
    loop {
        let projection: EvaluationExperimentProjection = api
            .get_bearer_path_query_json(token, &path, &[])
            .await
            .map_err(map_thin_err)?;
        let trial = projection
            .trials
            .iter()
            .find(|trial| trial.binding.trial_id == trial_id)
            .ok_or_else(|| format!("server projection omitted trial {trial_id}"))?;
        match trial.lifecycle {
            EvaluationTrialLifecycle::Observed => return Ok(()),
            EvaluationTrialLifecycle::TerminalAwaitingObservation => {
                // A terminal Run may still be waiting for its durable
                // observation. Repair that boundary before allowing the next
                // arm to start.
                try_assess_trial(api, token, experiment_id, trial_id).await?;
            }
            EvaluationTrialLifecycle::Unavailable => {
                return Err(format!(
                    "trial became unavailable (run status: {:?})",
                    trial.run_status
                ));
            }
            EvaluationTrialLifecycle::Planned => {
                return Err("trial is still planned after start".to_string());
            }
            EvaluationTrialLifecycle::Running
            | EvaluationTrialLifecycle::Waiting
            | EvaluationTrialLifecycle::Paused => {}
        }
        sleep_until_next_poll(deadline, poll_ms).await?;
    }
}

async fn assess_trial(
    api: &ThinClient,
    token: &str,
    experiment_id: &str,
    trial_id: &str,
    deadline: Instant,
    poll_ms: u64,
) -> Result<(), String> {
    let path = paths::evaluation_trial_assess(experiment_id, trial_id)
        .ok_or_else(|| format!("invalid evaluation trial identity: {trial_id}"))?;
    loop {
        let body = api
            .post_bearer_path_empty_text(token, &path)
            .await
            .map_err(map_thin_err)?;
        let result: TaskAssessmentResult = decode_json(&body, "assessment")?;
        match result {
            TaskAssessmentResult::Recorded(record) => {
                eprintln!("  assessed {} → {:?}", trial_id, record.outcome);
                return Ok(());
            }
            TaskAssessmentResult::Pending => {
                sleep_until_next_poll(deadline, poll_ms).await?;
            }
        }
    }
}

async fn sleep_until_next_poll(deadline: Instant, poll_ms: u64) -> Result<(), String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("wait deadline exceeded".to_string());
    }
    tokio::time::sleep(Duration::from_millis(poll_ms).min(remaining)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::evaluation::EvaluationTargetKind;
    use std::io::Write;

    #[test]
    fn intent_file_is_decoded_as_control_plane_request() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(
            file,
            r#"{{
                "submission_idempotency_key":"cli-test",
                "target":{{"kind":"skill_routing_judgment","skill_name":"review","baseline":{{"revision_id":"v1"}},"candidate":{{"revision_id":"v1"}}}},
                "case":{{"case_id":"case-1","message":"return json","verifier_config":{{"kind":"json_value_equals","expected":{{"ok":true}}}}}},
                "model_offering_id":"primary",
                "judgment_model_offering_id":"jev",
                "max_concurrency":1,
                "max_wall_time_secs":30
            }}"#
        )
        .unwrap();
        let request = read_intent(file.path()).unwrap();
        assert_eq!(
            request.target.kind,
            EvaluationTargetKind::SkillRoutingJudgment
        );
        assert_eq!(request.judgment_model_offering_id.as_deref(), Some("jev"));
    }

    #[test]
    fn unsafe_experiment_ids_never_become_request_paths() {
        let args = EvaluationExperimentArgs {
            experiment_id: "../other-owner".into(),
        };
        assert!(experiment_path(&args).is_err());
        assert!(paths::evaluation_trial_start("exp", "trial/other").is_none());
    }
}
