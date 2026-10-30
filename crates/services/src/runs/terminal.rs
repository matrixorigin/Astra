//! One bounded terminal fact on the Run. Answers remain in raw history; real
//! control/effect facts remain with their existing owners. This receipt is not
//! an execution grant or evidence that an external effect has settled.

use super::*;
use astra_turn_types::interruption::{InterruptionKind, InterruptionRecord};
use astra_turn_types::{
    CanonicalTokenUsage, SessionCursorV1, tool_ledger_receipt::ToolLedgerReceipt,
};

/// Typed input to the canonical terminal owner. Generic presentation events
/// neither carry lifecycle authority nor determine execution outcomes.
#[derive(Clone, Copy, Debug)]
pub struct CanonicalRunTerminalRequest<'a> {
    pub user_id: &'a str,
    pub run_id: &'a str,
    pub expected_session_id: &'a str,
    pub expected_statuses: &'a [&'a str],
    pub expected_owner_generation: u64,
    pub status: &'a str,
    pub waiting_for: Option<&'a str>,
    pub error_message: Option<&'a str>,
    pub error_code: Option<&'a str>,
    pub error_kind: Option<astra_core::ErrorKind>,
    pub error_metadata: Option<&'a crate::models::ModelGatewayErrorMetadata>,
    pub interruption: Option<&'a InterruptionRecord>,
    pub cancellation_origin: Option<DurableCancellationOrigin>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub tool_calls: u32,
}

const MAX_TERMINAL_RECEIPT_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum CanonicalTerminalOutcome {
    Completed,
    Delegated,
    Failed,
    Cancelled { origin: DurableCancellationOrigin },
}

impl CanonicalTerminalOutcome {
    pub fn status(&self) -> &'static str {
        match self {
            Self::Completed => STATUS_COMPLETED,
            Self::Delegated => STATUS_DELEGATED,
            Self::Failed => STATUS_FAILED,
            Self::Cancelled { .. } => STATUS_CANCELLED,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "disposition", rename_all = "snake_case", deny_unknown_fields)]
pub enum CanonicalTerminalContext {
    Advanced {
        cursor: SessionCursorV1,
        commit_hash: String,
    },
    /// Legitimately empty outcome: validate the admitted base without
    /// fabricating an empty conversation turn.
    Unchanged,
    /// A real control revocation forbids a Context write, even when the Run's
    /// cancellation and original raw history can still be committed.
    Revoked,
    Unowned,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalTerminalTranscript {
    pub item_count: u32,
    /// Digest of the ordered immutable raw identities, not answer text copied
    /// into C0. Unknown-ACK resolution still validates the actual C1 rows.
    pub digest: String,
    pub assistant_source_event_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalRunTerminalReceipt {
    pub schema_version: u32,
    pub user_id: String,
    pub session_id: String,
    pub run_id: String,
    pub generation: u64,
    pub opening_hash: String,
    pub context_admission_hash: Option<String>,
    pub outcome: CanonicalTerminalOutcome,
    /// Exact identity of the optional C0 error. The message itself has one
    /// owner, agent_runs.error_message.
    pub error_hash: Option<String>,
    pub error_code: Option<String>,
    /// Source-authored classification, independent of a domain error code.
    pub error_kind: Option<astra_core::ErrorKind>,
    pub error_metadata: Option<crate::models::ModelGatewayErrorMetadata>,
    pub interruption: Option<InterruptionRecord>,
    pub context: CanonicalTerminalContext,
    pub transcript: CanonicalTerminalTranscript,
    /// None means unavailable evidence. Never manufacture measured zeros.
    pub usage: Option<CanonicalTokenUsage>,
    pub tool_ledger: Option<ToolLedgerReceipt>,
}

impl CanonicalRunTerminalReceipt {
    pub fn outcome_for_request(
        request: CanonicalRunTerminalRequest<'_>,
    ) -> Result<CanonicalTerminalOutcome, String> {
        if request.status != STATUS_CANCELLED && request.cancellation_origin.is_some() {
            return Err("only cancellation may carry a cancellation origin".into());
        }
        Ok(match request.status {
            STATUS_COMPLETED => CanonicalTerminalOutcome::Completed,
            STATUS_DELEGATED => CanonicalTerminalOutcome::Delegated,
            STATUS_FAILED => CanonicalTerminalOutcome::Failed,
            STATUS_CANCELLED => CanonicalTerminalOutcome::Cancelled {
                origin: request
                    .cancellation_origin
                    .ok_or("missing cancellation origin")?,
            },
            _ => return Err("only terminal outcomes have canonical completion receipts".into()),
        })
    }

    /// Project the canonical logical completion events for both live delivery
    /// and replay. Answer bytes are supplied by C1 only when its exact window
    /// is requested. Wire expansion remains with the existing client formatter.
    #[allow(clippy::too_many_arguments)]
    pub fn completion_events(
        &self,
        answer: Option<&str>,
        model_item_id: Option<&str>,
        error_message: Option<&str>,
        first: Option<i64>,
        after: i64,
        through: i64,
    ) -> Result<Vec<serde_json::Value>, String> {
        self.validate()?;
        if self.error_hash != Self::error_hash(error_message) {
            return Err("terminal error projection does not match its C0 identity".into());
        }
        let wanted = |offset: i64| {
            first.is_none_or(|first| {
                first
                    .checked_add(offset)
                    .is_some_and(|index| index > after && index <= through)
            })
        };
        let mut events = Vec::with_capacity(usize::from(self.completion_count()));
        let mut offset = 0;
        let mut push = |offset: i64, event_type: &str, data: serde_json::Value| {
            let mut event =
                serde_json::json!({"event_type":event_type, "run_id":self.run_id, "data":data});
            if let Some(first) = first {
                event["index"] = (first + offset).into();
            }
            events.push(event);
        };
        if self.transcript.assistant_source_event_id.is_some() {
            if wanted(offset) {
                let answer =
                    answer.ok_or("terminal answer is missing from canonical raw history")?;
                let mut data = serde_json::json!({"full_text":answer});
                if matches!(self.outcome, CanonicalTerminalOutcome::Failed) {
                    data["partial"] = true.into();
                }
                if let Some(id) = model_item_id {
                    data["model_item_id"] = id.into();
                }
                push(offset, "text_done", data);
            }
            offset += 1;
        }
        if matches!(self.outcome, CanonicalTerminalOutcome::Failed) {
            if wanted(offset) {
                let mut data = serde_json::Map::from_iter([
                    ("error".into(), error_message.unwrap_or("Run failed").into()),
                    ("error_code".into(), serde_json::json!(self.error_code)),
                ]);
                if let Some(kind) = self.error_kind {
                    data.insert("error_kind".into(), serde_json::json!(kind));
                }
                if let Some(metadata) = &self.error_metadata {
                    data.extend(metadata.wire_fields());
                }
                push(offset, "run_error", data.into());
            }
            offset += 1;
        }
        if wanted(offset) {
            let mut data = serde_json::json!({"run_id":self.run_id, "owner_generation":self.generation,
                "status":self.outcome.status(), "outcome":self.outcome.status(),
                "qualified_usage":self.usage, "error":error_message, "error_code":self.error_code});
            if let Some(kind) = self.error_kind {
                data["error_kind"] = serde_json::json!(kind);
            }
            if let CanonicalTerminalOutcome::Cancelled { origin } = self.outcome {
                data["cancelled"] = true.into();
                data["cancellation_origin"] = origin.as_str().into();
            }
            if let Some(interruption) = &self.interruption {
                data["interruption"] = interruption.to_json();
            }
            if let Some(usage) = self.usage {
                for (key, value) in [
                    ("prompt_tokens", usage.input_tokens()),
                    ("cache_read_tokens", usage.cached_input_tokens()),
                    ("cache_creation_tokens", usage.cache_creation_tokens()),
                    ("completion_tokens", usage.output_tokens()),
                ] {
                    data[key] = serde_json::to_value(value).map_err(|error| error.to_string())?;
                }
            }
            if let Some(ledger) = &self.tool_ledger {
                data["tool_call_count"] = ledger.attempted.into();
                data["tool_ledger_receipt"] =
                    serde_json::to_value(ledger).map_err(|error| error.to_string())?;
            }
            push(offset, "run_finished", data);
        }
        Ok(events)
    }

    pub fn error_hash(message: Option<&str>) -> Option<String> {
        message.map(|message| {
            let mut hash = Sha256::new();
            hash.update(b"astra.run-terminal-error.v1\0");
            hash.update(message.as_bytes());
            format!("{:x}", hash.finalize())
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err("unsupported canonical terminal receipt schema".into());
        }
        for identity in [&self.user_id, &self.session_id, &self.run_id] {
            if identity.is_empty() || identity.len() > 128 || identity.trim() != identity {
                return Err("invalid canonical terminal identity".into());
            }
        }
        i64::try_from(self.generation).map_err(|_| "terminal generation exceeds BIGINT")?;
        for hash in [
            Some(self.opening_hash.as_str()),
            self.context_admission_hash.as_deref(),
            self.error_hash.as_deref(),
            Some(self.transcript.digest.as_str()),
        ]
        .into_iter()
        .flatten()
        {
            validate_terminal_hash(hash)?;
        }
        if self
            .error_code
            .as_ref()
            .is_some_and(|code| code.is_empty() || code.len() > 128)
        {
            return Err("invalid canonical terminal error code".into());
        }
        if self.error_kind.is_some() && !matches!(self.outcome, CanonicalTerminalOutcome::Failed) {
            return Err("only failed outcomes carry an error classification".into());
        }
        if let Some(metadata) = &self.error_metadata {
            metadata.validate()?;
            if !matches!(self.outcome, CanonicalTerminalOutcome::Failed) {
                return Err("only failed outcomes carry model gateway recovery facts".into());
            }
        }
        if let Some(interruption) = &self.interruption
            && (!matches!(self.outcome, CanonicalTerminalOutcome::Failed)
                || interruption.kind != InterruptionKind::ExecutionIncomplete
                || self.error_kind.is_some()
                || self.error_metadata.is_some()
                || self.error_code.as_deref() != Some(interruption.kind.label())
                || interruption.user_message.trim().is_empty()
                || interruption.error_detail.is_some()
                || interruption.stall_signal.is_some()
                || self.error_hash != Self::error_hash(Some(&interruption.user_message)))
        {
            return Err("invalid canonical terminal interruption".into());
        }
        if let Some(source) = &self.transcript.assistant_source_event_id
            && (self.transcript.item_count == 0 || source.is_empty() || source.len() > 255)
        {
            return Err("invalid terminal assistant source identity".into());
        }
        if matches!(self.context, CanonicalTerminalContext::Unowned)
            != self.context_admission_hash.is_none()
        {
            return Err("terminal Context disposition disagrees with admission".into());
        }
        if let CanonicalTerminalContext::Advanced {
            cursor,
            commit_hash,
        } = &self.context
        {
            validate_terminal_hash(commit_hash)?;
            validate_terminal_hash(&cursor.canonical_root_hash)?;
            if cursor.schema_version != astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION
                || cursor.owner_id != self.user_id
                || cursor.session_id != self.session_id
                || cursor.branch_id != astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID
                || cursor.completed_turn == 0
                || cursor.journal_event_seq == 0
                || cursor.conversation_seq == 0
                || cursor.projection_schema == 0
            {
                return Err("terminal Context cursor does not identify a committed turn".into());
            }
            i64::try_from(cursor.conversation_seq)
                .map_err(|_| "Context sequence exceeds BIGINT")?;
        }
        if let Some(ledger) = &self.tool_ledger {
            ledger.validate()?;
            if ledger.run_id != self.run_id || ledger.owner_generation != self.generation {
                return Err("terminal tool receipt belongs to another execution".into());
            }
        }
        Ok(())
    }

    /// The existing formatter emits at most one answer, one error and one
    /// completion frame. All indices are reserved in the same terminal write.
    pub fn completion_count(&self) -> u8 {
        1 + u8::from(self.transcript.assistant_source_event_id.is_some())
            + u8::from(matches!(self.outcome, CanonicalTerminalOutcome::Failed))
    }
}

fn validate_terminal_hash(hash: &str) -> Result<(), String> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("canonical terminal identity requires lowercase SHA-256".into());
    }
    Ok(())
}

/// Stored allocation is immutable. Later genuine events may advance the Run's
/// tail; replay must never derive completion indices from that later tail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommittedRunTerminalReceipt {
    pub receipt: CanonicalRunTerminalReceipt,
    completion_first: i64,
    completion_count: u8,
    control_count: u32,
    control_digest: Option<String>,
}

impl CommittedRunTerminalReceipt {
    pub fn decode(encoded: &str, last_event_idx: i64) -> Result<Self, String> {
        if encoded.len() > MAX_TERMINAL_RECEIPT_BYTES {
            return Err("canonical terminal receipt exceeds its bounded size".into());
        }
        let stored: Self = serde_json::from_str(encoded).map_err(|error| error.to_string())?;
        stored.receipt.validate()?;
        if stored.control_count > 1024
            || (stored.control_count == 0) != stored.control_digest.is_none()
        {
            return Err("invalid terminal control allocation".into());
        }
        if let Some(digest) = &stored.control_digest {
            validate_terminal_hash(digest)?;
        }
        if stored
            .completion_first
            .checked_sub(i64::from(stored.control_count))
            .is_none_or(|first| first <= 0)
        {
            return Err("terminal control range precedes Run opening".into());
        }
        if stored.completion_first <= 0
            || stored.completion_count != stored.receipt.completion_count()
            || stored
                .completion_first
                .checked_add(i64::from(stored.completion_count) - 1)
                .is_none_or(|end| end > last_event_idx)
        {
            return Err("invalid canonical terminal completion allocation".into());
        }
        Ok(stored)
    }

    pub fn completion_first(&self) -> i64 {
        self.completion_first
    }
    pub fn completion_count(&self) -> u8 {
        self.completion_count
    }
}

/// A control race is an outcome to converge under the same transaction,
/// never permission to fall back to independent transcript/status writes.
pub enum CanonicalTerminalClaim {
    Claimed {
        admission: RunMutationAdmission,
        control_events: Vec<serde_json::Value>,
    },
    CancellationRequested,
    Paused,
    Conflict,
}

impl DatabaseRunStateStore {
    /// Quiet terminal claim under the caller's canonical Session fence. Real
    /// control history requires the existing locked control owner; unrelated
    /// trace/action events do not invalidate this conditional write.
    pub async fn claim_canonical_terminal_in_transaction(
        &self,
        tx: &mut Transaction<'_, MySql>,
        request: CanonicalRunTerminalRequest<'_>,
        receipt: &CanonicalRunTerminalReceipt,
    ) -> Result<Option<RunMutationAdmission>, String> {
        self.claim_canonical_terminal_tx(tx, request, receipt, None, &[])
            .await
    }

    /// Genuine controls use the existing locked disposition planners. Quiet
    /// Runs never pay for this history read, and no caller can supply a bypass
    /// flag in place of the store's locked facts.
    pub async fn claim_canonical_terminal_with_controls_in_transaction(
        &self,
        tx: &mut Transaction<'_, MySql>,
        request: CanonicalRunTerminalRequest<'_>,
        receipt: &CanonicalRunTerminalReceipt,
    ) -> Result<CanonicalTerminalClaim, String> {
        if let Some(admission) = self
            .claim_canonical_terminal_in_transaction(tx, request, receipt)
            .await?
        {
            return Ok(CanonicalTerminalClaim::Claimed {
                admission,
                control_events: Vec::new(),
            });
        }
        let Some(locked) = self
            .admit_mutation_in_transaction(
                tx,
                request.user_id,
                request.expected_session_id,
                request.run_id,
            )
            .await
            .map_err(|error| error.to_string())?
        else {
            return Ok(CanonicalTerminalClaim::Conflict);
        };
        let run = &locked.run;
        if run.run_generation != request.expected_owner_generation {
            return Ok(CanonicalTerminalClaim::Conflict);
        }
        if run.status == STATUS_CANCELLED
            && request.status != STATUS_CANCELLED
            && locked.cancellation_requested
        {
            return Ok(CanonicalTerminalClaim::CancellationRequested);
        }
        if run.status == STATUS_PAUSED && request.status != STATUS_CANCELLED {
            return Ok(CanonicalTerminalClaim::Paused);
        }
        if !request.expected_statuses.contains(&run.status.as_str()) {
            return Ok(CanonicalTerminalClaim::Conflict);
        }
        let origin = match receipt.outcome {
            CanonicalTerminalOutcome::Cancelled { origin } => Some(origin),
            _ => None,
        };
        let segments = validated_durable_lineage_segments(run)?;
        let markers = if segments.len() == 1 {
            DurableLineageCancellationMarkers {
                direct: locked.cancellation_requested,
                ancestor: false,
            }
        } else {
            lock_durable_lineage_cancellation_markers_for_segments_tx(
                tx,
                &run.user_id,
                &run.session_id,
                &run.run_id,
                &segments,
            )
            .await?
        };
        if !cancellation_markers_admit_transition(origin, markers) {
            return Ok(if markers.direct || markers.ancestor {
                CanonicalTerminalClaim::CancellationRequested
            } else {
                CanonicalTerminalClaim::Conflict
            });
        }
        let mut controls = self
            .terminal_interaction_closure_events_tx(tx, run, request.status)
            .await
            .map_err(|error| error.to_string())?;
        controls.extend(
            self.terminal_user_intent_return_events_tx(
                tx,
                request.user_id,
                request.run_id,
                true,
                "canonical_terminal_control_dispositions",
            )
            .await
            .map_err(|error| error.to_string())?,
        );
        if controls.len() > 1024 {
            return Err("canonical terminal control work exceeds its bound".into());
        }
        let rows = controls
            .iter()
            .enumerate()
            .map(|(offset, event)| {
                let index = run
                    .last_event_idx
                    .checked_add(1 + offset as i64)
                    .ok_or_else(|| "terminal event index exhausted".to_string())?;
                build_run_event_insert_row(
                    request.user_id,
                    request.run_id,
                    request.expected_session_id,
                    run.agent_id.as_deref(),
                    index,
                    &self.owner_pod_id,
                    event,
                )
                .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, String>>()?;
        let Some(admission) = self
            .claim_canonical_terminal_tx(tx, request, receipt, Some(run), &rows)
            .await?
        else {
            return Ok(CanonicalTerminalClaim::Conflict);
        };
        Self::insert_run_event_rows_tx(
            tx,
            request.run_id,
            &rows,
            "canonical_terminal_insert_controls",
        )
        .await
        .map_err(|error| error.to_string())?;
        enqueue_work_terminal_event_for_run(tx, run, request.status).await?;
        Ok(CanonicalTerminalClaim::Claimed {
            admission,
            control_events: controls,
        })
    }

    async fn claim_canonical_terminal_tx(
        &self,
        tx: &mut Transaction<'_, MySql>,
        request: CanonicalRunTerminalRequest<'_>,
        receipt: &CanonicalRunTerminalReceipt,
        locked: Option<&DurableRunRecord>,
        controls: &[RunEventInsertRow],
    ) -> Result<Option<RunMutationAdmission>, String> {
        receipt.validate()?;
        validate_expected_run_session_id(request.expected_session_id)?;
        if request.expected_statuses.is_empty()
            || (request.status != STATUS_CANCELLED
                && request.expected_statuses.contains(&STATUS_PAUSED))
            || request
                .expected_statuses
                .iter()
                .any(|status| !matches!(*status, STATUS_RUNNING | STATUS_WAITING | STATUS_PAUSED))
            || request.waiting_for.is_some()
            || receipt.user_id != request.user_id
            || receipt.session_id != request.expected_session_id
            || receipt.run_id != request.run_id
            || receipt.generation != request.expected_owner_generation
            || receipt.outcome.status() != request.status
            || receipt.error_hash != CanonicalRunTerminalReceipt::error_hash(request.error_message)
            || receipt.error_kind != request.error_kind
            || receipt.error_code.as_deref() != request.error_code
            || receipt.error_metadata.as_ref() != request.error_metadata
            || receipt.interruption.as_ref() != request.interruption
            || receipt.outcome != CanonicalRunTerminalReceipt::outcome_for_request(request)?
        {
            return Err("canonical terminal request disagrees with its exact receipt".into());
        }
        if receipt
            .tool_ledger
            .as_ref()
            .is_some_and(|ledger| ledger.attempted != request.tool_calls)
            || receipt.usage.is_some_and(|usage| {
                usage
                    .input_column()
                    .is_some_and(|total| u64::try_from(total).ok() != Some(request.prompt_tokens))
                    || usage
                        .output_tokens()
                        .is_some_and(|total| total != request.completion_tokens)
            })
        {
            return Err("terminal accounting disagrees with its qualified evidence".into());
        }
        let count = receipt.completion_count();
        let control_count =
            u32::try_from(controls.len()).map_err(|_| "terminal control count overflow")?;
        let allocation_count = i64::from(count) + i64::from(control_count);
        let control_digest = (!controls.is_empty()).then(|| {
            terminal_control_digest(
                controls
                    .iter()
                    .map(|row| (row.event_idx, row.event_hash.as_str())),
            )
        });
        // Zero is an internal placeholder, never a valid decoded allocation.
        // MatrixOne evaluates JSON_SET and the tail increment from the old row.
        let encoded = serde_json::to_string(&CommittedRunTerminalReceipt {
            receipt: receipt.clone(),
            completion_first: 0,
            completion_count: count,
            control_count,
            control_digest,
        })
        .map_err(|error| error.to_string())?;
        if encoded.len() > MAX_TERMINAL_RECEIPT_BYTES {
            return Err("canonical terminal receipt exceeds its bounded size".into());
        }
        let generation = i64::try_from(request.expected_owner_generation)
            .map_err(|_| "Run generation exceeds BIGINT")?;
        let prompt =
            i64::try_from(request.prompt_tokens).map_err(|_| "prompt usage exceeds BIGINT")?;
        let completion = i64::try_from(request.completion_tokens)
            .map_err(|_| "completion usage exceeds BIGINT")?;
        let mut update = QueryBuilder::<MySql>::new("UPDATE agent_runs SET status = ");
        update.push_bind(request.status)
            .push(", waiting_for = NULL, error_message = ").push_bind(request.error_message)
            .push(", error_code = ").push_bind(receipt.error_code.as_deref())
            .push(", total_prompt_tokens = ").push_bind(prompt)
            .push(", total_completion_tokens = ").push_bind(completion)
            .push(", total_tool_calls = ").push_bind(i64::from(request.tool_calls))
            .push(", terminal_receipt_json = JSON_SET(").push_bind(encoded)
            .push(", '$.completion_first', last_event_idx + ").push_bind(i64::from(control_count) + 1)
            .push("), last_event_idx = last_event_idx + ").push_bind(allocation_count)
            .push(", owner_pod_id = NULL, owner_lease_expires_at = NULL, updated_at = NOW(6) WHERE user_id = ")
            .push_bind(request.user_id).push(" AND session_id = ").push_bind(request.expected_session_id)
            .push(" AND run_id = ").push_bind(request.run_id)
            .push(" AND run_generation = ").push_bind(generation)
            .push(" AND owner_pod_id = ").push_bind(&self.owner_pod_id)
            .push(" AND owner_lease_expires_at >= NOW(6) AND terminal_receipt_json IS NULL AND last_event_idx <= ")
            .push_bind(i64::MAX - allocation_count).push(" AND status IN (");
        let mut states = update.separated(", ");
        for status in request.expected_statuses {
            states.push_bind(*status);
        }
        states.push_unseparated(")");
        if locked.is_none() {
            if matches!(
                receipt.outcome,
                CanonicalTerminalOutcome::Cancelled {
                    origin: DurableCancellationOrigin::User
                }
            ) {
                update.push(" AND cancellation_requested_at IS NOT NULL");
            } else {
                update.push(" AND cancellation_requested_at IS NULL");
            }
        }
        update
            .push(" AND opening_hash = ")
            .push_bind(&receipt.opening_hash);
        match receipt.context_admission_hash.as_deref() {
            Some(hash) => {
                update
                    .push(" AND context_admission_hash = ")
                    .push_bind(hash);
            }
            None => {
                update.push(" AND context_admission_hash IS NULL");
            }
        }
        if let Some(run) = locked {
            update
                .push(" AND last_event_idx = ")
                .push_bind(run.last_event_idx);
        } else {
            update.push(" AND parent_run_id IS NULL AND work_id IS NULL
                AND NOT EXISTS (SELECT 1 FROM agent_run_events control
                    FORCE INDEX (idx_agent_run_events_control_type_idx)
                    WHERE control.user_id = agent_runs.user_id AND control.run_id = agent_runs.run_id
                      AND control.event_type IN ('user_intent', 'user_intent_applied', 'user_intent_returned',
                        'approval_required', 'approval_resolved', 'ask_user_prompted', 'ask_user_resolved',
                        'provider_interaction_required', 'provider_interaction_resolved'))");
        }
        let affected = update
            .build()
            .execute(&mut **tx)
            .await
            .map_err(|error| error.to_string())?
            .rows_affected();
        Ok((affected == 1).then(|| RunMutationAdmission {
            user_id: request.user_id.to_owned(),
            session_id: request.expected_session_id.to_owned(),
            run_id: request.run_id.to_owned(),
            generation: request.expected_owner_generation,
            opening_hash: Some(receipt.opening_hash.clone()),
            context_admission_hash: receipt.context_admission_hash.clone(),
            operation: RunMutationOperation::Terminal,
        }))
    }

    /// C0 owns outcome and allocation; C1 owns answer bytes. Status summaries
    /// never fetch the answer, and replay fetches it only for a requested frame.
    pub(super) async fn canonical_terminal_projection_events(
        &self,
        run: &DurableRunRecord,
        windows: &[(i64, i64)],
        include_answer: bool,
    ) -> Result<Vec<serde_json::Value>, String> {
        let Some(stored) = committed_terminal_for_run(run)? else {
            return Ok(Vec::new());
        };
        let first = stored.completion_first();
        let has_answer = stored
            .receipt
            .transcript
            .assistant_source_event_id
            .is_some();
        let needs_answer = include_answer
            && has_answer
            && windows
                .iter()
                .any(|(after, through)| first > *after && first <= *through);
        let answer = if needs_answer {
            let row = sqlx::query(
                "SELECT role, content, payload_json, content_hash FROM session_transcript_items
                WHERE user_id = ? AND session_id = ? AND run_id = ? AND source_event_id = ?",
            )
            .bind(&run.user_id)
            .bind(&run.session_id)
            .bind(&run.run_id)
            .bind(
                stored
                    .receipt
                    .transcript
                    .assistant_source_event_id
                    .as_deref(),
            )
            .fetch_optional(self.pool.get())
            .await
            .map_err(|error| error.to_string())?
            .ok_or("canonical terminal answer is missing")?;
            let role: String = row.try_get("role").map_err(|error| error.to_string())?;
            let content: String = row.try_get("content").map_err(|error| error.to_string())?;
            let payload: Option<String> = row
                .try_get("payload_json")
                .map_err(|error| error.to_string())?;
            let hash: String = row
                .try_get("content_hash")
                .map_err(|error| error.to_string())?;
            if role != "assistant"
                || astra_turn_types::transcript_content_hash(&role, &content, payload.as_deref())
                    != hash
            {
                return Err("canonical terminal answer has invalid raw identity".into());
            }
            let payload: Option<serde_json::Value> = payload
                .map(|payload| serde_json::from_str(&payload))
                .transpose()
                .map_err(|error| error.to_string())?;
            if payload.as_ref().is_some_and(|payload| !payload.is_object()) {
                return Err("invalid terminal answer payload".into());
            }
            let model_item_id = payload
                .as_ref()
                .and_then(|payload| payload.get("model_item_id"));
            if model_item_id.is_some_and(|id| !id.is_null() && !id.is_string()) {
                return Err("invalid terminal model item identity".into());
            }
            Some((
                content,
                model_item_id
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            ))
        } else {
            None
        };
        let mut events = Vec::new();
        for (after, through) in windows {
            let after = if !include_answer && has_answer {
                (*after).max(first)
            } else {
                *after
            };
            events.extend(stored.receipt.completion_events(
                answer.as_ref().map(|(text, _)| text.as_str()),
                answer.as_ref().and_then(|(_, model)| model.as_deref()),
                run.error_message.as_deref(),
                Some(first),
                after,
                *through,
            )?);
        }
        events.sort_by_key(|event| event["index"].as_i64());
        events.dedup_by_key(|event| event["index"].as_i64());
        Ok(events)
    }

    /// Return the exact committed outcome, including its server-allocated
    /// range. A terminal status or matching caller JSON alone is insufficient.
    pub async fn resolve_canonical_terminal_receipt(
        &self,
        request: CanonicalRunTerminalRequest<'_>,
        expected: &CanonicalRunTerminalReceipt,
    ) -> Result<Option<CommittedRunTerminalReceipt>, String> {
        expected.validate()?;
        let row = sqlx::query(
            "SELECT terminal_receipt_json, last_event_idx, status,
                error_message, error_code, total_prompt_tokens, total_completion_tokens,
                total_tool_calls, run_generation, opening_hash, context_admission_hash,
                owner_pod_id, owner_lease_expires_at
            FROM agent_runs WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(request.user_id)
        .bind(request.expected_session_id)
        .bind(request.run_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|error| error.to_string())?;
        let Some(row) = row else {
            return Ok(None);
        };
        let Some(encoded) = row
            .try_get::<Option<String>, _>("terminal_receipt_json")
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        let decoded = CommittedRunTerminalReceipt::decode(
            &encoded,
            row.try_get("last_event_idx")
                .map_err(|error| error.to_string())?,
        )?;
        let stored_status: String = row.try_get("status").map_err(|error| error.to_string())?;
        let error_message: Option<String> = row
            .try_get("error_message")
            .map_err(|error| error.to_string())?;
        let error_code: Option<String> = row
            .try_get("error_code")
            .map_err(|error| error.to_string())?;
        let opening_hash: Option<String> = row
            .try_get("opening_hash")
            .map_err(|error| error.to_string())?;
        let context_hash: Option<String> = row
            .try_get("context_admission_hash")
            .map_err(|error| error.to_string())?;
        let owner: Option<String> = row
            .try_get("owner_pod_id")
            .map_err(|error| error.to_string())?;
        let lease: Option<chrono::NaiveDateTime> = row
            .try_get("owner_lease_expires_at")
            .map_err(|error| error.to_string())?;
        let number = |name| {
            row.try_get::<i64, _>(name)
                .map_err(|error| error.to_string())
        };
        if decoded.receipt != *expected
            || stored_status != request.status
            || decoded.receipt.user_id != request.user_id
            || decoded.receipt.session_id != request.expected_session_id
            || decoded.receipt.run_id != request.run_id
            || decoded.receipt.generation != request.expected_owner_generation
            || decoded.receipt.error_kind != request.error_kind
            || decoded.receipt.error_code.as_deref() != request.error_code
            || decoded.receipt.error_metadata.as_ref() != request.error_metadata
            || decoded.receipt.interruption.as_ref() != request.interruption
            || error_message.as_deref() != request.error_message
            || error_code != expected.error_code
            || opening_hash.as_deref() != Some(expected.opening_hash.as_str())
            || context_hash != expected.context_admission_hash
            || number("run_generation")?
                != i64::try_from(request.expected_owner_generation)
                    .map_err(|_| "generation exceeds BIGINT")?
            || number("total_prompt_tokens")?
                != i64::try_from(request.prompt_tokens).map_err(|_| "prompt exceeds BIGINT")?
            || number("total_completion_tokens")?
                != i64::try_from(request.completion_tokens)
                    .map_err(|_| "completion exceeds BIGINT")?
            || number("total_tool_calls")? != i64::from(request.tool_calls)
            || owner.is_some()
            || lease.is_some()
        {
            return Err(
                "committed canonical terminal receipt conflicts with requested outcome".into(),
            );
        }
        if decoded.control_count != 0 {
            let first = decoded.completion_first - i64::from(decoded.control_count);
            let rows = self
                .load_atomic_terminal_event_rows(
                    request.user_id,
                    request.run_id,
                    Some(first),
                    decoded.control_count as usize,
                )
                .await?;
            if rows.len() != decoded.control_count as usize
                || rows.iter().enumerate().any(|(offset, (receipt, payload))| {
                    receipt.event_idx != first + offset as i64
                        || !atomic_terminal_event_hash_is_valid(receipt, payload)
                })
                || Some(terminal_control_digest(rows.iter().map(|(receipt, _)| {
                    (receipt.event_idx, receipt.event_hash.as_str())
                }))) != decoded.control_digest
            {
                return Err("committed terminal control facts are missing or changed".into());
            }
        }
        Ok(Some(decoded))
    }
}

fn terminal_control_digest<'a>(rows: impl Iterator<Item = (i64, &'a str)>) -> String {
    let mut hash = Sha256::new();
    hash.update(b"astra.run-terminal-controls.v1\0");
    for (index, payload_hash) in rows {
        hash.update(index.to_be_bytes());
        hash.update(payload_hash.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

fn committed_terminal_for_run(
    run: &DurableRunRecord,
) -> Result<Option<CommittedRunTerminalReceipt>, String> {
    let Some(encoded) = &run.terminal_receipt_json else {
        return Ok(None);
    };
    let stored = CommittedRunTerminalReceipt::decode(encoded, run.last_event_idx)?;
    let receipt = &stored.receipt;
    if receipt.user_id != run.user_id
        || receipt.session_id != run.session_id
        || receipt.run_id != run.run_id
        || receipt.generation != run.run_generation
        || receipt.outcome.status() != run.status
        || Some(receipt.opening_hash.as_str()) != run.opening_hash.as_deref()
        || receipt.context_admission_hash != run.context_admission_hash
        || receipt.error_hash
            != CanonicalRunTerminalReceipt::error_hash(run.error_message.as_deref())
        || receipt.error_code != run.error_code
    {
        return Err("canonical terminal receipt disagrees with its Run".into());
    }
    Ok(Some(stored))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ModelGatewayErrorMetadata;

    fn failed_receipt() -> CommittedRunTerminalReceipt {
        let details = serde_json::json!({
            "source":"moi_model_gateway", "retryable":false,
            "http_status":402, "action":"open_billing_overview",
            "private_upstream_detail":"must never enter the receipt",
        });
        CommittedRunTerminalReceipt {
            receipt: CanonicalRunTerminalReceipt {
                schema_version: 1,
                user_id: "owner".into(),
                session_id: "session".into(),
                run_id: "run".into(),
                generation: 1,
                opening_hash: "a".repeat(64),
                context_admission_hash: None,
                outcome: CanonicalTerminalOutcome::Failed,
                error_hash: CanonicalRunTerminalReceipt::error_hash(Some("Inference rejected")),
                error_code: Some("insufficient_credit".into()),
                error_kind: Some(astra_core::ErrorKind::Unknown),
                error_metadata: ModelGatewayErrorMetadata::from_details(
                    details.as_object().unwrap(),
                ),
                interruption: None,
                context: CanonicalTerminalContext::Unowned,
                transcript: CanonicalTerminalTranscript {
                    item_count: 0,
                    digest: "b".repeat(64),
                    assistant_source_event_id: None,
                },
                usage: None,
                tool_ledger: None,
            },
            completion_first: 1,
            completion_count: 2,
            control_count: 0,
            control_digest: None,
        }
    }

    #[test]
    fn cold_terminal_interruption_requires_coherent_sanitized_outcome() {
        use astra_turn_types::interruption::{InterruptionStateSummary, ResumeAction};
        let mut stored = failed_receipt();
        let record = InterruptionRecord::new(
            InterruptionKind::ExecutionIncomplete,
            ResumeAction::ContinueImmediately,
            InterruptionStateSummary::default(),
        );
        stored.receipt.error_kind = None;
        stored.receipt.error_metadata = None;
        stored.receipt.error_code = Some(record.kind.label().into());
        stored.receipt.error_hash =
            CanonicalRunTerminalReceipt::error_hash(Some(&record.user_message));
        stored.receipt.interruption = Some(record);
        let encoded = serde_json::to_string(&stored).unwrap();
        assert_eq!(
            CommittedRunTerminalReceipt::decode(&encoded, 2).unwrap(),
            stored
        );
        for (path, value) in [
            (
                "/receipt/interruption/error_detail",
                serde_json::json!("private"),
            ),
            (
                "/receipt/interruption/user_message",
                serde_json::json!("changed error"),
            ),
            (
                "/receipt/interruption/kind",
                serde_json::json!("budget_exhausted"),
            ),
            ("/receipt/error_kind", serde_json::json!("network")),
            ("/receipt/error_code", serde_json::json!("other_error")),
            ("/receipt/outcome/status", serde_json::json!("completed")),
        ] {
            let mut corrupt: serde_json::Value = serde_json::from_str(&encoded).unwrap();
            *corrupt.pointer_mut(path).unwrap() = value;
            assert!(
                CommittedRunTerminalReceipt::decode(&corrupt.to_string(), 2).is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn cold_terminal_error_keeps_exact_safe_recovery_facts() {
        let encoded = serde_json::to_string(&failed_receipt()).unwrap();
        assert!(!encoded.contains("private_upstream_detail"));
        assert!(!encoded.contains("must never enter"));
        let restored = CommittedRunTerminalReceipt::decode(&encoded, 2).unwrap();
        let events = restored
            .receipt
            .completion_events(None, None, Some("Inference rejected"), Some(1), -1, 2)
            .unwrap();
        let public = events
            .into_iter()
            .map(transform_run_event_for_client)
            .collect::<Vec<_>>();
        assert_eq!(public.len(), 2);
        assert_eq!(public[0]["type"], "run_error");
        assert_eq!(public[0]["error_code"], "insufficient_credit");
        assert_eq!(public[0]["error_kind"], "unknown");
        assert_eq!(public[0]["http_status"], 402);
        assert_eq!(public[0]["retryable"], false);
        assert_eq!(public[0]["action"], "open_billing_overview");
        assert_eq!(public[1]["type"], "run_finished");
        assert_eq!(public[1]["error_kind"], "unknown");
    }

    #[test]
    fn cold_terminal_classification_is_typed_and_failure_only() {
        let encoded = serde_json::to_value(failed_receipt()).unwrap();
        for invalid in [
            serde_json::json!("insufficient_credit"),
            serde_json::json!(7),
        ] {
            let mut corrupt = encoded.clone();
            corrupt["receipt"]["error_kind"] = invalid;
            assert!(CommittedRunTerminalReceipt::decode(&corrupt.to_string(), 2).is_err());
        }
        let mut receipt = failed_receipt().receipt;
        receipt.error_metadata = None;
        receipt.error_code = None;
        receipt.validate().unwrap();
        // Missing evidence stays missing; Unknown is an explicit source classification.
        receipt.error_kind = None;
        receipt.validate().unwrap();
        receipt.outcome = CanonicalTerminalOutcome::Completed;
        receipt.validate().unwrap();
        receipt.error_kind = Some(astra_core::ErrorKind::Unknown);
        assert!(receipt.validate().is_err());
        receipt.outcome = CanonicalTerminalOutcome::Cancelled {
            origin: DurableCancellationOrigin::User,
        };
        assert!(receipt.validate().is_err());
    }

    #[test]
    fn corrupt_or_nonfailure_recovery_facts_fail_cold_decode() {
        let encoded = serde_json::to_value(failed_receipt()).unwrap();
        for (field, value) in [
            ("source", serde_json::json!("untrusted_provider")),
            ("retryable", serde_json::json!("false")),
            ("http_status", serde_json::json!(200)),
            ("http_status", serde_json::json!(600)),
            ("action", serde_json::json!("open https://private.invalid")),
            ("provider_body", serde_json::json!("private")),
        ] {
            let mut corrupt = encoded.clone();
            corrupt["receipt"]["error_metadata"][field] = value;
            assert!(
                CommittedRunTerminalReceipt::decode(&corrupt.to_string(), 2).is_err(),
                "{field}"
            );
        }
        let mut cancelled = failed_receipt();
        cancelled.receipt.error_kind = None;
        cancelled.receipt.outcome = CanonicalTerminalOutcome::Cancelled {
            origin: DurableCancellationOrigin::User,
        };
        assert!(
            CommittedRunTerminalReceipt::decode(&serde_json::to_string(&cancelled).unwrap(), 2,)
                .is_err()
        );
    }
}
