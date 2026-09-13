//! Evidence integrity for Agent assessments of recovery by another approach.
//!
//! These assessments never rewrite execution facts or satisfy a deterministic
//! verifier. The caller supplies the current intent scope and its authoritative,
//! chronological invocation records; model-supplied scope is only checked against
//! that authority, never used to select another user's or turn's ledger.

use super::{
    effective_tool_result_class, record_is_non_failure_outcome, result_class_is_outcome_failure,
};
use astra_services::session_journal::{ToolCallDisposition, ToolCallRecord};
pub use astra_turn_types::task_resolution::{TaskResolutionAssessment, TaskResolutionConclusion};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AssessmentEvidenceError {
    #[error("assessment does not belong to the current intent scope")]
    WrongScope,
    #[error("assessment belongs to a different reconciliation boundary")]
    WrongBoundary,
    #[error("assessment exceeds the bounded reconciliation payload")]
    TooLarge,
    #[error("assessment evidence does not match the owner-scoped invocation ledger")]
    LedgerMismatch,
    #[error("authoritative invocation evidence is currently unavailable")]
    LedgerUnavailable,
    #[error("assessment must identify a target, explanation, and failed invocation")]
    MissingClaim,
    #[error("invocation reference is missing or ambiguous in current evidence")]
    UnavailableReference,
    #[error("assessment repeats an invocation reference")]
    DuplicateReference,
    #[error("failed invocation reference does not identify an executed failure")]
    NotExecutedFailure,
    #[error("supporting evidence must have executed after the referenced failures")]
    NotLaterExecution,
    #[error("supported assessment has no affirmative execution observation")]
    MissingPositiveObservation,
    #[error("supported assessment still declares unresolved gaps")]
    UnresolvedGaps,
}

/// Runtime authority for a reconciliation boundary, never model-authored.
pub struct AssessmentInvocationScope<'a> {
    pub user_id: &'a str,
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub turn_chain_id: &'a str,
}

/// Resolve referenced execution facts against ledger rows fetched by the host.
/// The host retains ownership of retrieval and subject freshness; this function
/// verifies exact completion digests and rebuilds classification inputs from the
/// durable outcomes rather than trusting journal display fields.
pub fn validate_bound_assessment_evidence(
    assessment: &TaskResolutionAssessment,
    current_scope: &str,
    current_boundary: &str,
    owner: AssessmentInvocationScope<'_>,
    records: &[ToolCallRecord],
    ledger: &[astra_turn_types::ToolInvocationRecord],
) -> Result<(), AssessmentEvidenceError> {
    let resolved = resolve_bound_assessment_invocations(
        assessment,
        current_scope,
        current_boundary,
        owner,
        records,
        ledger,
    )?;
    validate_assessment_evidence(assessment, current_scope, current_boundary, &resolved)
}

/// Resolve invocation-backed members before combining them with observations
/// resolved from other canonical authorities. This does not accept a claim.
pub fn resolve_bound_assessment_invocations(
    assessment: &TaskResolutionAssessment,
    current_scope: &str,
    current_boundary: &str,
    owner: AssessmentInvocationScope<'_>,
    records: &[ToolCallRecord],
    ledger: &[astra_turn_types::ToolInvocationRecord],
) -> Result<Vec<ToolCallRecord>, AssessmentEvidenceError> {
    use astra_turn_types::{
        DurableToolReference, ToolInvocationCompletionRef, ToolInvocationState,
    };
    let mismatch = AssessmentEvidenceError::LedgerMismatch;
    // Validate bounded wire shape/references before traversing durable outcomes.
    // Do not use the display-derived classification as the final decision.
    validate_assessment_header(assessment, current_scope, current_boundary)?;
    let requested: BTreeSet<_> = assessment
        .failed_call_ids
        .iter()
        .chain(&assessment.evidence_call_ids)
        .map(String::as_str)
        .collect();
    let mut resolved = Vec::new();
    for record in records.iter().filter(|record| {
        record
            .tool_call_id
            .as_deref()
            .is_some_and(|id| requested.contains(id))
    }) {
        let expected = record
            .execution_completion
            .as_ref()
            .and_then(|reference| reference.as_invocation())
            .ok_or(mismatch)?;
        let identity = &expected.identity;
        if record.tool_call_id.as_deref() != Some(identity.invocation_id.as_str())
            || identity.user_id != owner.user_id
            || identity.session_id != owner.session_id
            || identity.run_id != owner.run_id
            || identity.turn_chain_id != owner.turn_chain_id
        {
            return Err(mismatch);
        }
        let mut matches = ledger.iter().filter(|row| row.identity == *identity);
        let row = matches.next().ok_or(mismatch)?;
        if matches.next().is_some()
            || ToolInvocationCompletionRef::from_record(row).map_err(|_| mismatch)? != *expected
            || row.completion_source.is_some()
            || !matches!(
                row.state,
                ToolInvocationState::Succeeded | ToolInvocationState::Failed
            )
        {
            return Err(mismatch);
        }
        let result = row.outcome.as_ref().ok_or(mismatch)?.result();
        resolved.push(ToolCallRecord {
            tool_call_id: record.tool_call_id.clone(),
            execution_completion: Some(expected.clone().into()),
            round: record.round,
            name: match &row.fingerprint.tool {
                DurableToolReference::BuiltIn { tool_name, .. } => tool_name.clone(),
                DurableToolReference::Provider { .. } => String::new(),
            },
            disposition: Some(ToolCallDisposition::from_execution_metadata(
                result.metadata.get("disposition"),
                result
                    .metadata
                    .get("execution_started")
                    .and_then(serde_json::Value::as_bool),
                ToolCallDisposition::Executed,
            )),
            ok: row.state == ToolInvocationState::Succeeded,
            result_full: Some(result.output.clone()),
            exit_semantics: result.exit_semantics.clone(),
            result_class: result
                .metadata
                .get("result_class")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            ..Default::default()
        });
    }
    Ok(resolved)
}

/// Check references and ordering within caller-supplied evidence, NOT invocation
/// authority, subject freshness, or semantic relevance. The runtime must resolve
/// these records against the invocation ledger and current reconciliation
/// boundary before calling this function. Vector order alone is not proof of
/// freshness after a mutation or of ordering between concurrent executions.
/// Success permits retention as a model interpretation, never removal of a
/// required check. A populated completion reference alone is not ledger proof.
pub fn validate_assessment_evidence(
    assessment: &TaskResolutionAssessment,
    current_scope: &str,
    current_boundary: &str,
    records: &[ToolCallRecord],
) -> Result<(), AssessmentEvidenceError> {
    validate_assessment_header(assessment, current_scope, current_boundary)?;
    validate_assessment_records(assessment, records)
}

pub fn validate_assessment_header(
    assessment: &TaskResolutionAssessment,
    current_scope: &str,
    current_boundary: &str,
) -> Result<(), AssessmentEvidenceError> {
    use AssessmentEvidenceError as Error;
    if current_scope.trim().is_empty() || assessment.scope != current_scope {
        return Err(Error::WrongScope);
    }
    if current_boundary.trim().is_empty() || assessment.boundary_id != current_boundary {
        return Err(Error::WrongBoundary);
    }
    // This is a compact explanation of a bounded evidence set, not another
    // transcript channel. Reject oversized submissions; never truncate IDs or
    // structured evidence into a different claim.
    if assessment.verification_target.len() > 1024
        || assessment.rationale.len() > 4096
        || assessment.failed_call_ids.len() > 32
        || assessment.evidence_call_ids.len() > 32
        || assessment.remaining_gaps.len() > 32
        || assessment
            .failed_call_ids
            .iter()
            .chain(&assessment.evidence_call_ids)
            .any(|id| id.len() > 256)
        || assessment.remaining_gaps.iter().any(|gap| gap.len() > 1024)
    {
        return Err(Error::TooLarge);
    }
    if assessment.verification_target.trim().is_empty()
        || assessment.rationale.trim().is_empty()
        || assessment.failed_call_ids.is_empty()
    {
        return Err(Error::MissingClaim);
    }
    Ok(())
}

fn validate_assessment_records(
    assessment: &TaskResolutionAssessment,
    records: &[ToolCallRecord],
) -> Result<(), AssessmentEvidenceError> {
    use AssessmentEvidenceError as Error;
    let mut by_id = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        if let Some(id) = record.tool_call_id.as_deref().filter(|id| !id.is_empty()) {
            // Duplicate provider IDs cannot silently choose a convenient result.
            by_id
                .entry(id)
                .and_modify(|entry| *entry = None)
                .or_insert(Some((index, record)));
        }
    }
    let mut seen = BTreeSet::new();
    let mut lookup = |id: &str| {
        if !seen.insert(id.to_owned()) {
            return Err(Error::DuplicateReference);
        }
        by_id
            .get(id)
            .copied()
            .flatten()
            .ok_or(Error::UnavailableReference)
    };
    let mut last_failure = 0;
    let mut last_failure_round = None;
    for id in &assessment.failed_call_ids {
        let (index, record) = lookup(id)?;
        let classified_failure = effective_tool_result_class(record)
            .is_some_and(|class| result_class_is_outcome_failure(&class));
        if record.disposition != Some(ToolCallDisposition::Executed)
            || !(classified_failure || (!record.ok && !record_is_non_failure_outcome(record)))
        {
            return Err(Error::NotExecutedFailure);
        }
        last_failure = last_failure.max(index);
        last_failure_round = last_failure_round.max(record.round);
    }
    let mut positive_observation = false;
    for id in &assessment.evidence_call_ids {
        let (index, record) = lookup(id)?;
        if record.disposition != Some(ToolCallDisposition::Executed)
            || index <= last_failure
            || record
                .round
                .zip(last_failure_round)
                .is_some_and(|(evidence, failure)| evidence <= failure)
        {
            return Err(Error::NotLaterExecution);
        }
        positive_observation |= is_assessment_observation_candidate(record);
    }
    if assessment.conclusion == TaskResolutionConclusion::Supported && !positive_observation {
        return Err(Error::MissingPositiveObservation);
    }
    if assessment.conclusion == TaskResolutionConclusion::Supported
        && !assessment.remaining_gaps.is_empty()
    {
        return Err(Error::UnresolvedGaps);
    }
    Ok(())
}

/// A completed observation can invite an Agent assessment; it does not prove
/// relevance to a failed verification target or authorize task completion.
pub fn is_assessment_observation_candidate(record: &ToolCallRecord) -> bool {
    let class = effective_tool_result_class(record);
    let completed_process = record.exit_semantics.as_deref().is_none_or(|tag| {
        matches!(
            serde_json::from_value::<astra_tools::exit_semantics::ExitSemantics>(
                serde_json::Value::String(tag.into())
            ),
            Ok(astra_tools::exit_semantics::ExitSemantics::Success
                | astra_tools::exit_semantics::ExitSemantics::EmptyResult
                | astra_tools::exit_semantics::ExitSemantics::DomainNegative)
        )
    });
    record.disposition == Some(ToolCallDisposition::Executed)
        && (record.ok || record_is_non_failure_outcome(record))
        && completed_process
        && !class
            .as_deref()
            .is_some_and(|class| class == "inconclusive" || result_class_is_outcome_failure(class))
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::ToolCallDisposition;

    fn validate_assessment_evidence(
        assessment: &TaskResolutionAssessment,
        current_scope: &str,
        records: &[ToolCallRecord],
    ) -> Result<(), AssessmentEvidenceError> {
        super::validate_assessment_evidence(assessment, current_scope, "boundary-1", records)
    }

    fn record(id: &str, command: &str, ok: bool) -> ToolCallRecord {
        ToolCallRecord {
            tool_call_id: Some(id.into()),
            name: "bash".into(),
            ok,
            disposition: Some(ToolCallDisposition::Executed),
            args_full: Some(serde_json::json!({"command": command}).to_string()),
            result_class: Some(if ok { "success" } else { "test_failure" }.into()),
            ..Default::default()
        }
    }

    fn bound_record(
        id: &str,
        ok: bool,
    ) -> (ToolCallRecord, astra_turn_types::ToolInvocationRecord) {
        bound_record_with_metadata(id, ok, BTreeMap::new())
    }

    fn bound_record_with_metadata(
        id: &str,
        ok: bool,
        metadata: BTreeMap<String, serde_json::Value>,
    ) -> (ToolCallRecord, astra_turn_types::ToolInvocationRecord) {
        use astra_turn_types::*;
        let mut ledger = crate::invocation_ledger::InMemoryInvocationLedger::default();
        let identity = ToolInvocationIdentity::new("user", "session", "run", "chain", id).unwrap();
        let decision = ToolInvocationDecision::new(&serde_json::json!({"allowed": true})).unwrap();
        let fingerprint = ToolInvocationFingerprint::new(
            DurableToolReference::built_in("bash", "v1").unwrap(),
            &serde_json::json!({"command": id}),
            &decision.decision_id,
        )
        .unwrap();
        ledger
            .prepare(identity.clone(), fingerprint, decision)
            .unwrap();
        ledger
            .claim_dispatch(
                &identity,
                ToolInvocationDispatchLease::new("owner", u64::MAX).unwrap(),
            )
            .unwrap();
        let result = ToolInvocationResultPayload {
            output: "direct result".into(),
            metadata,
            exit_semantics: None,
        };
        let outcome = if ok {
            ToolInvocationTerminalOutcome::Succeeded { result }
        } else {
            ToolInvocationTerminalOutcome::Failed {
                result,
                error_kind: None,
                retryable: false,
            }
        };
        let row = ledger
            .compare_and_complete(
                &identity,
                ToolInvocationState::Dispatched,
                Some("owner"),
                outcome,
            )
            .unwrap();
        let mut projected = record(id, id, ok);
        projected.execution_completion = Some(
            ToolInvocationCompletionRef::from_record(&row)
                .unwrap()
                .into(),
        );
        (projected, row)
    }

    fn owner() -> AssessmentInvocationScope<'static> {
        AssessmentInvocationScope {
            user_id: "user",
            session_id: "session",
            run_id: "run",
            turn_chain_id: "chain",
        }
    }

    #[test]
    fn authoritative_non_execution_and_relabelled_calls_cannot_support_recovery() {
        for metadata in [
            serde_json::json!({"disposition": "reused"}),
            serde_json::json!({"disposition": "deferred"}),
            serde_json::json!({"execution_started": false}),
            serde_json::json!({"disposition": "executed", "execution_started": false}),
        ] {
            let (failed, failed_row) = bound_record("failed", false);
            let (later, later_row) = bound_record_with_metadata(
                "later",
                true,
                serde_json::from_value(metadata).unwrap(),
            );
            assert_eq!(
                validate_bound_assessment_evidence(
                    &claim(),
                    "current-intent",
                    "boundary-1",
                    owner(),
                    &[failed, later],
                    &[failed_row, later_row]
                ),
                Err(AssessmentEvidenceError::NotLaterExecution)
            );
        }
        let (failed, failed_row) = bound_record("failed", false);
        let (mut later, later_row) = bound_record("different-invocation", true);
        later.tool_call_id = Some("later".into());
        assert_eq!(
            validate_bound_assessment_evidence(
                &claim(),
                "current-intent",
                "boundary-1",
                owner(),
                &[failed, later],
                &[failed_row, later_row]
            ),
            Err(AssessmentEvidenceError::LedgerMismatch)
        );
    }

    #[test]
    fn same_round_sibling_is_not_later_verification() {
        let mut failed = record("failed", "old command", false);
        let mut later = record("later", "new command", true);
        failed.round = Some(4);
        later.round = Some(4);
        assert_eq!(
            validate_assessment_evidence(
                &claim(),
                "current-intent",
                &[failed.clone(), later.clone()]
            ),
            Err(AssessmentEvidenceError::NotLaterExecution)
        );
        later.round = Some(5);
        assert!(validate_assessment_evidence(&claim(), "current-intent", &[failed, later]).is_ok());
    }

    #[test]
    fn ledger_facts_override_display_status_and_reject_digest_or_owner_mismatch() {
        let (mut failed, failed_row) = bound_record("failed", false);
        let (mut later, later_row) = bound_record("later", true);
        failed.ok = true;
        failed.result_class = Some("success".into());
        later.ok = false;
        later.result_class = Some("test_failure".into());
        let records = [failed, later];
        let rows = [failed_row, later_row];
        assert_eq!(
            validate_bound_assessment_evidence(
                &claim(),
                "current-intent",
                "boundary-1",
                owner(),
                &records,
                &rows
            ),
            Ok(())
        );
        let mut wrong_owner = owner();
        wrong_owner.user_id = "another-user";
        assert_eq!(
            validate_bound_assessment_evidence(
                &claim(),
                "current-intent",
                "boundary-1",
                wrong_owner,
                &records,
                &rows
            ),
            Err(AssessmentEvidenceError::LedgerMismatch)
        );
        let mut forged = records.to_vec();
        let astra_turn_types::task_resolution::ToolExecutionEvidenceRef::Invocation(reference) =
            forged[1].execution_completion.as_mut().unwrap()
        else {
            unreachable!()
        };
        reference.outcome_digest = Some(format!("sha256:{}", "0".repeat(64)));
        assert_eq!(
            validate_bound_assessment_evidence(
                &claim(),
                "current-intent",
                "boundary-1",
                owner(),
                &forged,
                &rows
            ),
            Err(AssessmentEvidenceError::LedgerMismatch)
        );
        assert_eq!(
            validate_bound_assessment_evidence(
                &claim(),
                "current-intent",
                "boundary-1",
                owner(),
                &records,
                &rows[..1]
            ),
            Err(AssessmentEvidenceError::LedgerMismatch)
        );
    }

    fn claim() -> TaskResolutionAssessment {
        TaskResolutionAssessment {
            scope: "current-intent".into(),
            boundary_id: "boundary-1".into(),
            verification_target: "runtime unit tests".into(),
            failed_call_ids: vec!["failed".into()],
            evidence_call_ids: vec!["later".into()],
            conclusion: TaskResolutionConclusion::Supported,
            rationale: "The later direct invocation reports the requested test results.".into(),
            remaining_gaps: vec![],
        }
    }

    #[test]
    fn reused_boundary_and_oversized_claims_are_rejected_without_truncation() {
        let records = [
            record("failed", "check", false),
            record("later", "check-again", true),
        ];
        assert_eq!(
            super::validate_assessment_evidence(&claim(), "current-intent", "boundary-2", &records),
            Err(AssessmentEvidenceError::WrongBoundary)
        );
        let mut assessment = claim();
        assessment.rationale = "x".repeat(4097);
        assert_eq!(
            validate_assessment_evidence(&assessment, "current-intent", &records),
            Err(AssessmentEvidenceError::TooLarge)
        );
        assert_eq!(assessment.rationale.len(), 4097);
    }

    #[test]
    fn truncated_and_missing_disposition_are_not_completed_support() {
        let failed = record("failed", "check", false);
        let mut later = record("later", "check | head", false);
        later.result_class = None;
        later.exit_semantics = Some("pipeline_truncated".into());
        assert_eq!(
            validate_assessment_evidence(&claim(), "current-intent", &[failed.clone(), later]),
            Err(AssessmentEvidenceError::MissingPositiveObservation)
        );
        let mut legacy = record("later", "check", true);
        legacy.disposition = None;
        assert_eq!(
            validate_assessment_evidence(&claim(), "current-intent", &[failed, legacy]),
            Err(AssessmentEvidenceError::NotLaterExecution)
        );
    }

    #[test]
    fn changed_command_assessment_does_not_rewrite_execution_failures() {
        let records = [
            record("failed", "cargo test -p astra-runtime | tail -20", false),
            record("later", "cargo test -p astra-runtime", true),
        ];
        assert_eq!(
            validate_assessment_evidence(&claim(), "current-intent", &records),
            Ok(())
        );
        assert_eq!(
            super::super::count_unresolved_tool_outcome_failures(&records),
            1
        );
        assert!(!records[0].ok);
    }

    #[test]
    fn unknown_preserves_missing_support_and_gaps() {
        let mut assessment = claim();
        assessment.conclusion = TaskResolutionConclusion::Unknown;
        assessment.evidence_call_ids.clear();
        assessment
            .remaining_gaps
            .push("No later verification available".into());
        let records = [record("failed", "check", false)];
        assert_eq!(
            validate_assessment_evidence(&assessment, "current-intent", &records),
            Ok(())
        );
        assessment.conclusion = TaskResolutionConclusion::Supported;
        assert_eq!(
            validate_assessment_evidence(&assessment, "current-intent", &records),
            Err(AssessmentEvidenceError::MissingPositiveObservation)
        );
    }

    #[test]
    fn rejects_wrong_scope_missing_duplicate_and_old_references() {
        let records = [
            record("failed", "check", false),
            record("later", "check-again", true),
        ];
        assert_eq!(
            validate_assessment_evidence(&claim(), "other-intent", &records),
            Err(AssessmentEvidenceError::WrongScope)
        );
        assert_eq!(
            validate_assessment_evidence(&claim(), "current-intent", &records[..1]),
            Err(AssessmentEvidenceError::UnavailableReference)
        );
        assert_eq!(
            validate_assessment_evidence(
                &claim(),
                "current-intent",
                &[records[1].clone(), records[0].clone()]
            ),
            Err(AssessmentEvidenceError::NotLaterExecution)
        );
        let mut duplicated = records.to_vec();
        duplicated.push(records[1].clone());
        assert_eq!(
            validate_assessment_evidence(&claim(), "current-intent", &duplicated),
            Err(AssessmentEvidenceError::UnavailableReference)
        );
    }

    #[test]
    fn reuse_and_inconclusive_results_are_not_positive_execution_proof() {
        for disposition in [
            ToolCallDisposition::Rejected,
            ToolCallDisposition::Suppressed,
        ] {
            let mut later = record("later", "check-again", true);
            later.disposition = Some(disposition);
            assert_eq!(
                validate_assessment_evidence(
                    &claim(),
                    "current-intent",
                    &[record("failed", "check", false), later]
                ),
                Err(AssessmentEvidenceError::NotLaterExecution)
            );
        }
        let mut later = record("later", "check-again", true);
        later.result_class = Some("inconclusive".into());
        assert_eq!(
            validate_assessment_evidence(
                &claim(),
                "current-intent",
                &[record("failed", "check", false), later]
            ),
            Err(AssessmentEvidenceError::MissingPositiveObservation)
        );
    }

    #[test]
    fn wire_requires_uncertainty_fields_and_rejects_unrecognized_conclusions() {
        let wire = serde_json::to_value(claim()).unwrap();
        for field in [
            "conclusion",
            "remaining_gaps",
            "failed_call_ids",
            "evidence_call_ids",
        ] {
            let mut missing = wire.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<TaskResolutionAssessment>(missing).is_err());
        }
        let mut unsupported = wire.clone();
        unsupported["conclusion"] = serde_json::json!("probably_fixed");
        assert!(serde_json::from_value::<TaskResolutionAssessment>(unsupported).is_err());
        let mut extra = wire;
        extra["clear_all_failures"] = serde_json::json!(true);
        assert!(serde_json::from_value::<TaskResolutionAssessment>(extra).is_err());

        let mut assessment = claim();
        assessment
            .remaining_gaps
            .push("Required integration check has not run".into());
        assert_eq!(
            validate_assessment_evidence(
                &assessment,
                "current-intent",
                &[
                    record("failed", "check", false),
                    record("later", "check-again", true)
                ]
            ),
            Err(AssessmentEvidenceError::UnresolvedGaps)
        );
    }
}
