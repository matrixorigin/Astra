//! Canonical completion control shared by execution and durable checkpoints.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSuccessfulToolCompletion {
    pub tool_name: String,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub final_text: Option<String>,
}

/// Recovery state for a textless provider response.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionSettlementState {
    /// Host-internal canonical Work establishment repairs already attempted in
    /// this user turn. Keeping the counter in typed loop state preserves the
    /// bounded-once contract across transport failure and resume.
    pub canonical_work_establishment_retries: u32,
    /// Host-internal output-cap continuations already attempted in this user
    /// turn. This is independent of provider prose and survives retry/resume.
    pub output_cap_continuations: u8,
    /// Number of same-turn recovery calls made after the provider returned a
    /// successful response with neither tool calls nor user-visible text.
    pub textless_response_retries: u32,
    /// Number of bounded terminal rewrites after a tool failure remained
    /// unresolved across multiple policy observations.  The retry is
    /// synthesis-only: it calibrates claims against retained evidence rather
    /// than reopening exploration or hiding the failed outcome.
    pub outcome_reconciliation_retries: u32,
    /// Evidence-linked model interpretation accepted for the active boundary.
    /// This does not replace execution facts or deterministic verifier receipts.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub outcome_reconciliation_assessment: Option<crate::task_resolution::TaskResolutionAssessment>,
    /// Number of bounded same-turn retries after a task whose typed profile
    /// requires a workspace change attempted to finish without recording one.
    /// This is deliberately separate from the read-only escalation advisory:
    /// the latter guides exploration while this field protects the terminal
    /// completion boundary.
    pub workspace_mutation_retries: u32,
    /// Number of bounded retries after an external or mixed mutation contract
    /// attempted to finish without an executor-owned external delta receipt.
    pub external_effect_retries: u32,
    /// Structured external observation scope carried across the bounded
    /// recovery boundary.  A model may split observation and mutation across
    /// two calls; the executor-owned scope is the only authority allowed to
    /// bridge those calls, never command text or assistant prose.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub external_effect_recovery_paths: Option<Vec<String>>,
    /// Number of bounded same-turn retries after the final successful
    /// workspace mutation had no later successful observation.  A mutation is
    /// progress, but it is not evidence that the resulting workspace is
    /// coherent; the retry gives the agent one chance to inspect or validate
    /// the state it actually created.
    pub post_mutation_observation_retries: u32,
    /// Number of bounded same-action retries after an admitted post-mutation
    /// observation could not produce observation evidence because the
    /// selected capability was unavailable. This is not repair authority:
    /// the workspace is unchanged and the retry remains restricted to the
    /// original observation obligation.
    pub post_mutation_observation_failed_action_retries: u32,
    /// A failed, executor-attested post-mutation observation may authorize
    /// exactly one repair followed by exactly one final observation. This is
    /// chain state, not an ordinary turn-budget renewal.
    pub post_mutation_repair_retries: u32,
    /// Exact normalized validator that proved the post-mutation result wrong.
    /// A repair may be settled only by rerunning this same validator; a
    /// generic workspace read cannot substitute for its failed assertion.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub post_mutation_repair_validation_operation: Option<String>,
    /// Number of bounded retries after an explicit verification contract was
    /// not satisfied at the terminal boundary.  This is intentionally
    /// separate from observation: reading the changed workspace is not a
    /// passing verification receipt.
    pub verification_retries: u32,
    /// A failed, runtime-recognized canonical Work validation earns one
    /// narrowly-scoped repair-and-revalidation cycle.  This is intentionally
    /// separate from ordinary budget renewal: the repair must be followed by
    /// the same canonical validation and then a truthful Work settlement.
    pub canonical_validation_recovery_retries: u32,
    /// A matching repair tool can fail before it establishes a successful
    /// correction. Permit one outcome-aware retry of that repair while
    /// preserving the independent request-shape correction budget on the
    /// completion-action window. This never grants another repair cycle.
    pub canonical_validation_recovery_failed_action_retries: u32,
    /// Normalized identity of the failed validation that authorized the
    /// bounded repair. The following revalidation must match this operation;
    /// an unrelated build/test cannot erase the original failure.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub canonical_validation_recovery_operation: Option<String>,
    /// A single typed completion action that was already justified by the
    /// user's structured intent and the executed-tool ledger.  This is not a
    /// general budget extension: it is consumed once and is followed by a
    /// text-only boundary.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub completion_action_window: Option<CompletionActionWindow>,
    /// Provider-declared success observed on a tool round whose typed
    /// completion obligation still required a dependent action.  The host
    /// only reports stop-after-success for the current round, so retain this
    /// terminal template until that bounded action actually settles it.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub deferred_success_completion: Option<RuntimeSuccessfulToolCompletion>,
    /// The next LLM boundary is a bounded final-answer recovery call. Hosts
    /// must advertise no tools and reject tool execution while this is set.
    pub text_only: bool,
    /// A foreground fanout reached a terminal group boundary but one or more
    /// slot results are paginated. The carrier records every exact next byte
    /// offset; finishing one short slot cannot silently discard another
    /// slot's unread evidence.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub foreground_fanout_pagination: Option<ForegroundFanoutPagination>,
    /// A stalled/explicitly bounded run still owns a canonical Work attempt.
    /// The next boundary may report that attempt's typed outcome, but may not
    /// resume open-ended exploration. Server hosts project only the exact
    /// settlement capability while this is set.
    pub work_settlement_only: bool,
    /// The next provider boundary reviews a just-completed Work graph. Strict-
    /// history providers must reuse the preceding wire declaration for that
    /// one request, while runtime admission remains narrowed to the current
    /// lifecycle surface. This is presentation/cache state only and must never
    /// authorize completion. Cleared when synthesis is accepted or a later
    /// semantic user turn resets the review surface; durable Work state alone
    /// decides whether a corrective tool reopened execution.
    pub preserve_final_synthesis_wire_surface: bool,
    /// Latest non-empty provider text observed in this user turn. This is
    /// independent from the deferred mixed-response candidate so an
    /// interruption can hand off the most recent model state instead of
    /// repeating an older candidate after a later boundary response.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub latest_provider_text: Option<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub deferred_candidate_text: Option<String>,
    /// Source of the active wrap-up boundary, if any.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub wrapup_origin: Option<BudgetWrapupOrigin>,
}

/// Exact bounded continuations still required before a terminal foreground
/// fanout may enter synthesis. This is execution authority, not display state:
/// admission must match both group and `(slot, offset)`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForegroundFanoutPagination {
    pub group_id: String,
    pub target_count: u64,
    pub pending_slots: BTreeMap<u64, u64>,
}

/// A narrow action that can finish an already-established obligation at the
/// end of an agentic slice.  Ordinary exploration never creates this window.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub enum CompletionAction {
    /// Submit an evidence-linked model assessment at this exact boundary.
    /// Matching admission does not establish acceptance or verification success.
    #[serde(rename = "outcome_reconciliation")]
    OutcomeReconciliation { boundary_id: String },
    #[serde(rename = "required_workspace_mutation")]
    RequiredWorkspaceMutation,
    #[serde(rename = "required_external_effect")]
    RequiredExternalEffect,
    /// Spend one terminal boundary on a task-facing tool action when the
    /// structured workspace intent is unknown or merely permits mutation.
    /// Ordinary admission and safety policy still own executable authority;
    /// this variant only prevents the terminal window from guessing that the
    /// remaining action must be either a write or a read.
    #[serde(rename = "completion_task_action")]
    CompletionTaskAction,
    #[serde(rename = "post_mutation_observation")]
    PostMutationObservation,
    #[serde(rename = "post_mutation_repair")]
    PostMutationRepair,
    #[serde(rename = "explicit_verification")]
    ExplicitVerification { missing_labels: Vec<String> },
    /// Re-run one canonical validator when the current durable Work attempt's
    /// latest validation failed or was invalidated by a later mutation. This
    /// is settlement authority, not a general execution-budget extension.
    #[serde(rename = "canonical_work_validation")]
    CanonicalWorkValidation,
    /// Make one focused workspace change after a failed canonical Work
    /// validation.  The next action is always canonical revalidation; this
    /// never opens a general exploratory slice.
    #[serde(rename = "canonical_work_repair")]
    CanonicalWorkRepair,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionActionWindow {
    pub action: CompletionAction,
    /// The first matching provider action consumes the only attempt.  The
    /// following provider boundary is text-only, regardless of success.
    pub attempts_remaining: u8,
    /// A provider may make one non-executed, semantically unrelated request
    /// and then correct it. This is separate from the single executable
    /// action attempt: a rejection is not evidence that the action ran.
    pub mismatch_corrections_remaining: u8,
    pub consumed: bool,
    /// Whether the consumed attempt matched the typed action.  A rejected or
    /// unrelated call must never be treated as completion evidence.
    pub matched: bool,
}

/// Why the runtime asked the provider to wrap up.  This is kept separate from
/// the boolean capability gate so a later ignored tool request cannot be
/// misreported as a token-rail overflow when the actual boundary was simply a
/// bounded agentic slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub enum BudgetWrapupOrigin {
    RoundSlice,
    TokenRail,
}

/// Decode a nullable field without treating a missing field as `None`.
#[doc(hidden)]
pub fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    <Option<T> as serde::Deserialize>::deserialize(deserializer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_control_never_defaults_missing_authority() {
        let state = CompletionSettlementState {
            canonical_work_establishment_retries: 1,
            output_cap_continuations: 1,
            textless_response_retries: 1,
            outcome_reconciliation_retries: 2,
            outcome_reconciliation_assessment: None,
            workspace_mutation_retries: 3,
            external_effect_retries: 4,
            external_effect_recovery_paths: Some(vec!["artifact".into()]),
            post_mutation_observation_retries: 5,
            post_mutation_observation_failed_action_retries: 6,
            post_mutation_repair_retries: 7,
            post_mutation_repair_validation_operation: Some("validator-a".into()),
            verification_retries: 8,
            canonical_validation_recovery_retries: 9,
            canonical_validation_recovery_failed_action_retries: 10,
            canonical_validation_recovery_operation: Some("validator-b".into()),
            deferred_success_completion: Some(RuntimeSuccessfulToolCompletion {
                tool_name: "validator".into(),
                final_text: Some("observed completion".into()),
            }),
            preserve_final_synthesis_wire_surface: true,
            latest_provider_text: Some("latest".into()),
            deferred_candidate_text: Some("candidate".into()),
            completion_action_window: Some(CompletionActionWindow {
                action: CompletionAction::ExplicitVerification {
                    missing_labels: vec!["validator".into()],
                },
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: false,
            }),
            foreground_fanout_pagination: Some(ForegroundFanoutPagination {
                group_id: "group".into(),
                target_count: 2,
                pending_slots: BTreeMap::from([(0, 40), (1, 80)]),
            }),
            text_only: true,
            work_settlement_only: true,
            wrapup_origin: Some(BudgetWrapupOrigin::TokenRail),
        };
        let wire = serde_json::to_value(&state).unwrap();
        assert_eq!(
            serde_json::from_value::<CompletionSettlementState>(wire.clone()).unwrap(),
            state
        );
        for field in wire.as_object().unwrap().keys() {
            let mut incomplete = wire.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<CompletionSettlementState>(incomplete).is_err(),
                "missing {field}"
            );
        }
        let mut unknown = wire.clone();
        unknown["completion_action_window"]["action"] = serde_json::json!("future_action");
        assert!(serde_json::from_value::<CompletionSettlementState>(unknown).is_err());
        for field in wire["completion_action_window"].as_object().unwrap().keys() {
            let mut incomplete = wire.clone();
            incomplete["completion_action_window"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(
                serde_json::from_value::<CompletionSettlementState>(incomplete).is_err(),
                "missing window {field}"
            );
        }
        let empty = CompletionSettlementState::default();
        assert_eq!(
            serde_json::from_value::<CompletionSettlementState>(
                serde_json::to_value(&empty).unwrap()
            )
            .unwrap(),
            empty
        );
    }
}
