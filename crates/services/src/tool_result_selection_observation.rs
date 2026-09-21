//! Best-effort tool-result selection observations in the existing trace lane.
//!
//! The trace explains evaluation. Provider-wire application remains owned by
//! the durable projection decision/receipt stored with model-request facts.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use astra_core::SharedPool;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;

use crate::cancellation_safe_db::CancellationSafePoolConnection;
use crate::session_journal::TraceSpanBuilder;
use crate::{ServiceError, ServiceResult};

pub const TOOL_RESULT_SELECTION_TRACE_NAME: &str = "tool_result_selection";
const MAX_TRACE_BYTES: usize = 32_768;
const MAX_CANDIDATES: usize = 512;

pub fn tool_result_selection_trace(
    observation_span_id: &str,
    observation: &astra_turn_types::ToolResultSelectionObservationV1,
    observed_at_us: u64,
) -> Result<TraceSpanBuilder, &'static str> {
    observation.validate()?;
    if observation_span_id.trim().is_empty()
        || observation_span_id.len() > 512
        || !observation_span_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:".contains(&byte))
    {
        return Err("invalid tool-result selection trace identity");
    }
    let encoded = serde_json::to_string(observation)
        .map_err(|_| "serialize tool-result selection observation")?;
    let attrs = HashMap::from([(
        astra_turn_types::TOOL_RESULT_SELECTION_TRACE_ATTR.to_owned(),
        encoded,
    )]);
    Ok(TraceSpanBuilder::default()
        .span_id(observation_span_id.to_owned())
        .name(TOOL_RESULT_SELECTION_TRACE_NAME.to_owned())
        .trace_id(Some(observation.correlation.run_id.clone()))
        .turn(Some(observation.correlation.turn))
        .start_us(observed_at_us)
        .end_us(observed_at_us)
        .attrs(Some(&attrs)))
}

/// A bounded, content-free read projection over existing evaluation traces and
/// provider-wire receipts. It is diagnostic only: neither recommendations nor
/// this projection are execution authority.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultJudgmentView {
    pub evaluation_coverage: ToolResultJudgmentCoverage,
    pub application_coverage: ToolResultJudgmentCoverage,
    pub evaluations: usize,
    pub selected: usize,
    pub baseline: usize,
    pub not_dispatched: usize,
    pub unavailable: usize,
    pub terminal_missing: usize,
    pub conflicting: usize,
    pub invalid_or_oversized: usize,
    pub applications: ToolResultApplicationCounts,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ToolResultJudgmentModel>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultJudgmentCoverage {
    Available,
    CaptureIncomplete,
    CaptureTruncated,
    #[default]
    NotObserved,
    SourceUnavailable,
    SourceExcluded,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultApplicationCounts {
    pub included: usize,
    pub partially_included: usize,
    pub omitted: usize,
    pub unknown: usize,
    pub invalid_or_conflicting: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ToolResultJudgmentModel {
    pub provider: String,
    pub model: String,
    pub observed_invocations: usize,
}

impl ToolResultJudgmentView {
    pub fn unavailable(coverage: ToolResultJudgmentCoverage) -> Self {
        Self {
            evaluation_coverage: coverage,
            application_coverage: coverage,
            ..Self::default()
        }
    }

    /// Concise user-facing facts. Raw trace identities and digests remain in
    /// their authoritative stores and are intentionally not dumped here.
    pub fn render(&self) -> String {
        if self.evaluation_coverage == ToolResultJudgmentCoverage::NotObserved
            && self.application_coverage == ToolResultJudgmentCoverage::NotObserved
        {
            return "Tool-result judgment: none observed.".into();
        }
        if self.evaluation_coverage == ToolResultJudgmentCoverage::SourceExcluded
            && self.application_coverage == ToolResultJudgmentCoverage::SourceExcluded
        {
            return "Tool-result judgment: excluded by source policy.".into();
        }
        if self.evaluation_coverage == ToolResultJudgmentCoverage::SourceUnavailable
            && self.application_coverage == ToolResultJudgmentCoverage::SourceUnavailable
        {
            return "Tool-result judgment: evidence unavailable.".into();
        }
        let mut line = format!(
            "Tool-result judgment: {} evaluation(s); {} selected, {} kept the baseline, {} skipped, {} unavailable",
            self.evaluations, self.selected, self.baseline, self.not_dispatched, self.unavailable
        );
        if self.terminal_missing > 0 || self.conflicting > 0 || self.invalid_or_oversized > 0 {
            line.push_str(&format!(
                "; {} missing terminal evidence, {} conflicting, {} invalid/oversized",
                self.terminal_missing, self.conflicting, self.invalid_or_oversized
            ));
        }
        line.push_str(&format!(
            ". Provider-wire application: {} included, {} partial, {} omitted, {} unknown, {} invalid/conflicting",
            self.applications.included,
            self.applications.partially_included,
            self.applications.omitted,
            self.applications.unknown,
            self.applications.invalid_or_conflicting,
        ));
        if !self.models.is_empty() {
            let models = self
                .models
                .iter()
                .map(|m| {
                    format!(
                        "{} via {} ({} observed invocation(s))",
                        m.model,
                        crate::judgment_presentation::provider_label(&m.provider),
                        m.observed_invocations
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            line.push_str(&format!(". Models: {models}"));
        }
        if self.evaluation_coverage == ToolResultJudgmentCoverage::SourceUnavailable {
            line.push_str(". Evaluation evidence unavailable");
        }
        if self.application_coverage == ToolResultJudgmentCoverage::SourceUnavailable {
            line.push_str(". Application evidence unavailable");
        }
        if let Some(note) = coverage_note(CoverageSubject::Evaluation, self.evaluation_coverage) {
            line.push_str(&format!(". {note}"));
        }
        if let Some(note) = coverage_note(CoverageSubject::Application, self.application_coverage) {
            line.push_str(&format!(". {note}"));
        }
        line.push('.');
        line
    }

    /// Short, actionable projection for the default introspect/reflect view.
    /// Detailed counts and identities remain available from [`Self::render`]
    /// and the structured report.
    pub fn render_compact(&self) -> String {
        let terminal_results = self
            .evaluations
            .saturating_sub(self.terminal_missing)
            .saturating_sub(self.conflicting);
        let evaluation = match self.evaluation_coverage {
            ToolResultJudgmentCoverage::NotObserved => {
                "evaluation not observed in this view".to_string()
            }
            ToolResultJudgmentCoverage::SourceExcluded => "evaluation excluded".to_string(),
            ToolResultJudgmentCoverage::SourceUnavailable => "evaluation unavailable".to_string(),
            _ => {
                let mut parts = Vec::new();
                if terminal_results > 0 {
                    parts.push(format!("{terminal_results} terminal results"));
                }
                if self.selected > 0 {
                    parts.push(format!("{} selected", self.selected));
                }
                if self.baseline > 0 {
                    parts.push(format!("{} kept baseline", self.baseline));
                }
                if self.not_dispatched > 0 {
                    parts.push(format!("{} skipped", self.not_dispatched));
                }
                if self.unavailable > 0 {
                    parts.push(format!("{} unavailable", self.unavailable));
                }
                if self.terminal_missing > 0 {
                    parts.push(format!(
                        "{} started; terminal result unknown",
                        self.terminal_missing
                    ));
                }
                if self.conflicting > 0 {
                    parts.push(format!("{} conflicting", self.conflicting));
                }
                if parts.is_empty() {
                    parts.push("no terminal result captured".into());
                }
                parts.join(" · ")
            }
        };
        let application = match self.application_coverage {
            ToolResultJudgmentCoverage::NotObserved => "application unknown".to_string(),
            ToolResultJudgmentCoverage::SourceExcluded => "application excluded".to_string(),
            ToolResultJudgmentCoverage::SourceUnavailable => "application unavailable".to_string(),
            _ => {
                let a = &self.applications;
                let mut parts = Vec::new();
                if a.included > 0 {
                    parts.push(format!("{} included", a.included));
                }
                if a.partially_included > 0 {
                    parts.push(format!("{} partial", a.partially_included));
                }
                if a.omitted > 0 {
                    parts.push(format!("{} omitted", a.omitted));
                }
                if a.unknown > 0 {
                    parts.push(format!("{} unknown", a.unknown));
                }
                if a.invalid_or_conflicting > 0 {
                    parts.push(format!("{} invalid/conflicting", a.invalid_or_conflicting));
                }
                if parts.is_empty() {
                    "application not observed".to_string()
                } else {
                    format!("application {}", parts.join(", "))
                }
            }
        };
        let mut line = format!("Tool-result selection · {evaluation} · {application}");
        if let Some(model) = self.models.first() {
            let identity = format!(
                "{} ({})",
                crate::judgment_presentation::provider_label(&model.provider),
                model.model
            );
            line.push_str(&format!(" · model {identity}"));
            if self.models.len() > 1 {
                line.push_str(&format!(" +{} other model(s)", self.models.len() - 1));
            }
        }
        if self.evaluation_coverage == ToolResultJudgmentCoverage::NotObserved
            && self.application_coverage == ToolResultJudgmentCoverage::NotObserved
        {
            line.push_str(" · not evidence of zero calls");
        }
        if let Some(note) = coverage_note(CoverageSubject::Evaluation, self.evaluation_coverage) {
            line.push_str(&format!(" · {note}"));
        }
        if let Some(note) = coverage_note(CoverageSubject::Application, self.application_coverage) {
            line.push_str(&format!(" · {note}"));
        }
        line
    }

    pub fn render_for_depth(&self, depth: astra_core::ObservationDepth) -> String {
        match depth {
            astra_core::ObservationDepth::Hint | astra_core::ObservationDepth::Summary => {
                self.render_compact()
            }
            astra_core::ObservationDepth::Diagnostic | astra_core::ObservationDepth::Forensic => {
                self.render()
            }
        }
    }
}

#[derive(Clone, Copy)]
enum CoverageSubject {
    Evaluation,
    Application,
}

fn coverage_note(
    subject: CoverageSubject,
    coverage: ToolResultJudgmentCoverage,
) -> Option<&'static str> {
    match coverage {
        ToolResultJudgmentCoverage::CaptureIncomplete => Some(match subject {
            CoverageSubject::Evaluation => "evaluation evidence incomplete; some facts are missing",
            CoverageSubject::Application => {
                "application evidence incomplete; some receipts are missing"
            }
        }),
        ToolResultJudgmentCoverage::CaptureTruncated => Some(match subject {
            CoverageSubject::Evaluation => "evaluation capture truncated; counts are lower bounds",
            CoverageSubject::Application => {
                "application capture truncated; counts are lower bounds"
            }
        }),
        _ => None,
    }
}

pub struct ToolResultSelectionTraceRow {
    pub user_id: String,
    pub session_id: String,
    pub metadata_json: Option<String>,
    pub metadata_oversized: bool,
}

pub struct ToolResultJudgmentProjectionInput<'a> {
    pub applications: &'a [crate::inference_execution::ToolResultProjectionApplicationFact],
    pub applications_truncated: bool,
    pub invalid_applications: usize,
    pub evaluation_source_available: bool,
    pub application_source_available: bool,
    pub user_id: &'a str,
    pub session_id: &'a str,
    pub max_candidates: usize,
}

fn decode_trace(
    raw: &str,
) -> Result<Option<astra_turn_types::ToolResultSelectionObservationV1>, ()> {
    if raw.len() > MAX_TRACE_BYTES {
        return Err(());
    }
    let metadata: Value = serde_json::from_str(raw).map_err(|_| ())?;
    if metadata.get("name").and_then(Value::as_str) != Some(TOOL_RESULT_SELECTION_TRACE_NAME) {
        return Ok(None);
    }
    let encoded = metadata
        .get("attrs")
        .and_then(|attrs| attrs.get(astra_turn_types::TOOL_RESULT_SELECTION_TRACE_ATTR))
        .and_then(Value::as_str)
        .ok_or(())?;
    let observation: astra_turn_types::ToolResultSelectionObservationV1 =
        serde_json::from_str(encoded).map_err(|_| ())?;
    observation.validate().map_err(|_| ())?;
    if metadata.get("trace_id").and_then(Value::as_str)
        != Some(observation.correlation.run_id.as_str())
    {
        return Err(());
    }
    Ok(Some(observation))
}

/// Canonical pure projection shared by introspect and reflect.
pub fn project_tool_result_judgments(
    rows: impl IntoIterator<Item = ToolResultSelectionTraceRow>,
    input: ToolResultJudgmentProjectionInput<'_>,
) -> ToolResultJudgmentView {
    use astra_turn_types::{
        ToolResultProjectionDispositionV1 as Disposition,
        ToolResultProjectionWireStateV1 as WireState, ToolResultSelectionOutcomeV1 as Outcome,
    };
    let limit = input.max_candidates.min(MAX_CANDIDATES);
    let mut view = ToolResultJudgmentView {
        evaluation_coverage: ToolResultJudgmentCoverage::Available,
        application_coverage: if input.applications_truncated {
            ToolResultJudgmentCoverage::CaptureTruncated
        } else {
            ToolResultJudgmentCoverage::Available
        },
        ..Default::default()
    };
    let mut started = BTreeMap::new();
    let mut terminal = BTreeMap::new();
    let mut conflicts = BTreeSet::new();
    for (index, row) in rows.into_iter().enumerate() {
        if index >= limit {
            view.evaluation_coverage = ToolResultJudgmentCoverage::CaptureTruncated;
            break;
        }
        if row.user_id != input.user_id || row.session_id != input.session_id {
            continue;
        }
        if row.metadata_oversized {
            view.invalid_or_oversized += 1;
            if view.evaluation_coverage != ToolResultJudgmentCoverage::CaptureTruncated {
                view.evaluation_coverage = ToolResultJudgmentCoverage::CaptureIncomplete;
            }
            continue;
        }
        let observation = match row.metadata_json.as_deref().map(decode_trace) {
            Some(Ok(Some(observation))) => observation,
            Some(Ok(None)) => continue,
            _ => {
                view.invalid_or_oversized += 1;
                if view.evaluation_coverage != ToolResultJudgmentCoverage::CaptureTruncated {
                    view.evaluation_coverage = ToolResultJudgmentCoverage::CaptureIncomplete;
                }
                continue;
            }
        };
        let key = (
            observation.correlation.run_id.clone(),
            observation.correlation.turn,
            observation.correlation.evaluation_id.clone(),
        );
        if matches!(observation.outcome, Outcome::Started) {
            match started.get(&key) {
                Some(previous) if previous != &observation => {
                    started.remove(&key);
                    conflicts.insert(key);
                }
                Some(_) => {}
                None => {
                    started.insert(key, observation);
                }
            }
            continue;
        }
        if conflicts.contains(&key) {
            continue;
        }
        match terminal.get(&key) {
            Some(previous) if previous != &observation => {
                terminal.remove(&key);
                conflicts.insert(key);
            }
            Some(_) => {}
            None => {
                terminal.insert(key, observation);
            }
        }
    }
    for (key, initial) in &started {
        if let Some(completed) = terminal.get(key)
            && (initial.coverage != completed.coverage
                || initial.correlation.round != completed.correlation.round
                || initial.correlation.owner_generation != completed.correlation.owner_generation)
        {
            conflicts.insert(key.clone());
        }
    }
    terminal.retain(|key, _| !conflicts.contains(key));
    view.conflicting += conflicts.len();
    if !conflicts.is_empty()
        && view.evaluation_coverage != ToolResultJudgmentCoverage::CaptureTruncated
    {
        view.evaluation_coverage = ToolResultJudgmentCoverage::CaptureIncomplete;
    }
    view.terminal_missing = started
        .keys()
        .filter(|key| !terminal.contains_key(*key) && !conflicts.contains(*key))
        .count();
    view.evaluations = terminal.len() + view.terminal_missing + conflicts.len();
    let mut invocation_models = BTreeMap::<String, (String, String)>::new();
    let mut invocation_conflicts = BTreeSet::new();
    for observation in terminal.values() {
        match &observation.outcome {
            Outcome::Decided {
                disposition,
                execution,
                ..
            } => {
                match disposition {
                    Disposition::Selected => view.selected += 1,
                    Disposition::Baseline => view.baseline += 1,
                }
                let identity = (execution.provider.clone(), execution.model_name.clone());
                match invocation_models.get(&execution.invocation_id) {
                    Some(previous) if previous != &identity => {
                        invocation_models.remove(&execution.invocation_id);
                        invocation_conflicts.insert(execution.invocation_id.clone());
                    }
                    Some(_) => {}
                    None if !invocation_conflicts.contains(&execution.invocation_id) => {
                        invocation_models.insert(execution.invocation_id.clone(), identity);
                    }
                    None => {}
                }
            }
            Outcome::Baseline { .. } => view.baseline += 1,
            Outcome::NotDispatched { .. } => view.not_dispatched += 1,
            Outcome::Unavailable { execution, .. } => {
                view.unavailable += 1;
                if let Some(execution) = execution {
                    let identity = (execution.provider.clone(), execution.model_name.clone());
                    match invocation_models.get(&execution.invocation_id) {
                        Some(previous) if previous != &identity => {
                            invocation_models.remove(&execution.invocation_id);
                            invocation_conflicts.insert(execution.invocation_id.clone());
                        }
                        Some(_) => {}
                        None if !invocation_conflicts.contains(&execution.invocation_id) => {
                            invocation_models.insert(execution.invocation_id.clone(), identity);
                        }
                        None => {}
                    }
                }
            }
            Outcome::Started => unreachable!("terminal map excludes started observations"),
        }
    }
    view.conflicting = view.conflicting.saturating_add(invocation_conflicts.len());
    if !invocation_conflicts.is_empty()
        && view.evaluation_coverage != ToolResultJudgmentCoverage::CaptureTruncated
    {
        view.evaluation_coverage = ToolResultJudgmentCoverage::CaptureIncomplete;
    }
    let mut models = BTreeMap::<(String, String), usize>::new();
    for (_, identity) in invocation_models {
        *models.entry(identity).or_default() += 1;
    }
    view.models = models
        .into_iter()
        .map(
            |((provider, model), observed_invocations)| ToolResultJudgmentModel {
                provider,
                model,
                observed_invocations,
            },
        )
        .collect();

    let mut receipts = BTreeMap::new();
    let mut receipt_conflicts = BTreeSet::new();
    for application in input.applications {
        let binding = &application.binding;
        let key = (
            application.attempt_id.clone(),
            binding.decision.freeze_key_sha256.clone(),
        );
        if binding.validate().is_err() || receipt_conflicts.contains(&key) {
            receipt_conflicts.insert(key.clone());
            receipts.remove(&key);
        } else if let Some(previous) = receipts.get(&key) {
            if previous != binding {
                receipts.remove(&key);
                receipt_conflicts.insert(key);
            }
        } else {
            receipts.insert(key, binding.clone());
        }
    }
    view.applications.invalid_or_conflicting = receipt_conflicts.len();
    view.applications.invalid_or_conflicting = view
        .applications
        .invalid_or_conflicting
        .saturating_add(input.invalid_applications);
    if input.invalid_applications > 0 && !input.applications_truncated {
        view.application_coverage = ToolResultJudgmentCoverage::CaptureIncomplete;
    }
    if !receipt_conflicts.is_empty() && !input.applications_truncated {
        view.application_coverage = ToolResultJudgmentCoverage::CaptureIncomplete;
    }
    for binding in receipts.values() {
        match binding.receipt.state {
            WireState::Included => view.applications.included += 1,
            WireState::PartiallyIncluded => view.applications.partially_included += 1,
            WireState::Omitted => view.applications.omitted += 1,
            WireState::Unknown => view.applications.unknown += 1,
        }
    }
    if view.evaluations == 0 && view.evaluation_coverage == ToolResultJudgmentCoverage::Available {
        view.evaluation_coverage = ToolResultJudgmentCoverage::NotObserved;
    }
    if receipts.is_empty()
        && receipt_conflicts.is_empty()
        && view.application_coverage == ToolResultJudgmentCoverage::Available
    {
        view.application_coverage = ToolResultJudgmentCoverage::NotObserved;
    }
    if !input.evaluation_source_available {
        view.evaluation_coverage = ToolResultJudgmentCoverage::SourceUnavailable;
    }
    if !input.application_source_available {
        view.application_coverage = ToolResultJudgmentCoverage::SourceUnavailable;
    }
    view
}

const LOAD_SQL: &str = "SELECT user_id, session_id, CASE WHEN OCTET_LENGTH(CAST(metadata AS CHAR)) <= ? THEN CAST(metadata AS CHAR) ELSE NULL END AS metadata_json, CASE WHEN OCTET_LENGTH(CAST(metadata AS CHAR)) > ? THEN 1 ELSE 0 END AS metadata_oversized FROM agent_events WHERE user_id = ? AND session_id = ? AND event_type = 'trace_span' ORDER BY created_at DESC, event_id DESC LIMIT ?";

async fn independently_bounded_sources<T, A, TF, AF>(
    trace_read: TF,
    application_read: AF,
    timeout: std::time::Duration,
) -> (Result<T, ()>, Result<A, ()>)
where
    TF: std::future::Future<Output = ServiceResult<T>>,
    AF: std::future::Future<Output = ServiceResult<A>>,
{
    let (trace, applications) = tokio::join!(
        tokio::time::timeout(timeout, trace_read),
        tokio::time::timeout(timeout, application_read),
    );
    (
        trace.ok().and_then(Result::ok).ok_or(()),
        applications.ok().and_then(Result::ok).ok_or(()),
    )
}

/// Authenticated, bounded service read used by both server introspection and reflection.
pub async fn load_tool_result_judgment_view(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    max_candidates: usize,
) -> ServiceResult<ToolResultJudgmentView> {
    if user_id.trim().is_empty()
        || user_id.len() > 128
        || session_id.trim().is_empty()
        || session_id.len() > 64
    {
        return Err(ServiceError::invalid(
            "invalid tool-result judgment subject",
        ));
    }
    let owned = tokio::time::timeout(std::time::Duration::from_millis(500), async {
        let mut connection = CancellationSafePoolConnection::acquire(pool.get())
            .await
            .map_err(|_| {
                ServiceError::persistence("tool-result judgment connection unavailable")
            })?;
        let owned = crate::storage::agent_session_exists_for_user(
            connection.connection_mut(),
            session_id,
            user_id,
        )
        .await
        .map_err(|_| ServiceError::persistence("tool-result judgment scope unavailable"))?;
        connection.release();
        Ok::<_, ServiceError>(owned)
    })
    .await
    .map_err(|_| ServiceError::persistence("tool-result judgment scope timed out"))??;
    if !owned {
        return Err(ServiceError::not_found("session not found"));
    }
    let limit = max_candidates.min(MAX_CANDIDATES);
    let trace_read = async {
        // The future is deadline-bound below. Keep an in-flight MySQL exchange
        // out of the shared pool if cancellation drops this future.
        let mut connection = CancellationSafePoolConnection::acquire(pool.get())
            .await
            .map_err(|_| ServiceError::persistence("tool-result trace connection unavailable"))?;
        let rows = sqlx::query(LOAD_SQL)
            .bind(MAX_TRACE_BYTES as i64)
            .bind(MAX_TRACE_BYTES as i64)
            .bind(user_id)
            .bind(session_id)
            .bind((limit + 1) as i64)
            .fetch_all(connection.connection_mut())
            .await
            .map_err(|_| {
                ServiceError::persistence("tool-result judgment observations unavailable")
            })?;
        connection.release();
        Ok(rows
            .into_iter()
            .map(|row| ToolResultSelectionTraceRow {
                user_id: row.try_get("user_id").unwrap_or_default(),
                session_id: row.try_get("session_id").unwrap_or_default(),
                metadata_json: row.try_get("metadata_json").ok(),
                metadata_oversized: row.try_get::<i64, _>("metadata_oversized").unwrap_or(1) != 0,
            })
            .collect::<Vec<_>>())
    };
    let application_read =
        crate::inference_execution::load_session_tool_result_projection_applications(
            pool, user_id, session_id, limit,
        );
    let (trace_result, application_result) = independently_bounded_sources(
        trace_read,
        application_read,
        std::time::Duration::from_millis(1_000),
    )
    .await;
    let evaluation_source_available = trace_result.is_ok();
    let trace_rows = trace_result.unwrap_or_default();
    let application_source_available = application_result.is_ok();
    let (applications, applications_truncated, invalid_applications) =
        application_result.unwrap_or_default();
    Ok(project_tool_result_judgments(
        trace_rows,
        ToolResultJudgmentProjectionInput {
            applications: &applications,
            applications_truncated,
            invalid_applications,
            evaluation_source_available,
            application_source_available,
            user_id,
            session_id,
            max_candidates: limit,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(
        evaluation_id: &str,
        outcome: astra_turn_types::ToolResultSelectionOutcomeV1,
    ) -> astra_turn_types::ToolResultSelectionObservationV1 {
        astra_turn_types::ToolResultSelectionObservationV1 {
            schema_version: astra_turn_types::TOOL_RESULT_SELECTION_OBSERVATION_SCHEMA_VERSION,
            correlation: astra_turn_types::ToolResultSelectionCorrelationV1 {
                run_id: "run-1".into(),
                turn: 1,
                round: 2,
                owner_generation: Some(3),
                evaluation_id: evaluation_id.into(),
            },
            coverage: astra_turn_types::ToolResultSelectionCoverageV1 {
                source_bytes: 100,
                scanned_bytes: 100,
                candidate_chunks: 2,
                source_complete: true,
                goal_complete: true,
            },
            outcome,
        }
    }

    fn row(
        user_id: &str,
        session_id: &str,
        span_id: &str,
        observation: &astra_turn_types::ToolResultSelectionObservationV1,
    ) -> ToolResultSelectionTraceRow {
        let event = tool_result_selection_trace(span_id, observation, 10)
            .unwrap()
            .session_id(Some(session_id))
            .build();
        ToolResultSelectionTraceRow {
            user_id: user_id.into(),
            session_id: session_id.into(),
            metadata_json: event.metadata.map(|value| value.to_string()),
            metadata_oversized: false,
        }
    }

    fn projection_input<'a>(
        applications: &'a [crate::inference_execution::ToolResultProjectionApplicationFact],
    ) -> ToolResultJudgmentProjectionInput<'a> {
        ToolResultJudgmentProjectionInput {
            applications,
            applications_truncated: false,
            invalid_applications: 0,
            evaluation_source_available: true,
            application_source_available: true,
            user_id: "user-1",
            session_id: "session-1",
            max_candidates: 32,
        }
    }

    #[test]
    fn trace_contains_only_the_typed_bounded_observation() {
        let observation = astra_turn_types::ToolResultSelectionObservationV1 {
            schema_version: astra_turn_types::TOOL_RESULT_SELECTION_OBSERVATION_SCHEMA_VERSION,
            correlation: astra_turn_types::ToolResultSelectionCorrelationV1 {
                run_id: "run-1".into(),
                turn: 1,
                round: 2,
                owner_generation: Some(3),
                evaluation_id: "evaluation-1".into(),
            },
            coverage: astra_turn_types::ToolResultSelectionCoverageV1 {
                source_bytes: 100,
                scanned_bytes: 100,
                candidate_chunks: 2,
                source_complete: true,
                goal_complete: true,
            },
            outcome: astra_turn_types::ToolResultSelectionOutcomeV1::NotDispatched {
                reason: astra_turn_types::ToolResultSelectionNotDispatchedReasonV1::NoOffering,
            },
        };
        let event = tool_result_selection_trace("selection-1", &observation, 10)
            .unwrap()
            .session_id(Some("session-1"))
            .build();
        let metadata = event.metadata.unwrap();
        assert_eq!(metadata["name"], TOOL_RESULT_SELECTION_TRACE_NAME);
        let decoded: astra_turn_types::ToolResultSelectionObservationV1 = serde_json::from_str(
            metadata["attrs"][astra_turn_types::TOOL_RESULT_SELECTION_TRACE_ATTR]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(decoded, observation);
    }

    #[test]
    fn projection_reconciles_started_and_terminal_and_names_actual_model() {
        let started = observation(
            "evaluation-1",
            astra_turn_types::ToolResultSelectionOutcomeV1::Started,
        );
        let decided = observation(
            "evaluation-1",
            astra_turn_types::ToolResultSelectionOutcomeV1::Decided {
                decision_sha256: "a".repeat(64),
                disposition: astra_turn_types::ToolResultProjectionDispositionV1::Selected,
                selected_chunks: 1,
                relevant_chunks: 1,
                uncertain_chunks: 0,
                irrelevant_chunks: 1,
                fallback: None,
                execution: astra_turn_types::ToolResultSelectionExecutionV1 {
                    invocation_id: "invocation-1".into(),
                    model_name: "jev-1.13.0".into(),
                    provider: "jet".into(),
                },
            },
        );
        let mut repeated_invocation = decided.clone();
        repeated_invocation.correlation.evaluation_id = "evaluation-2".into();
        let view = project_tool_result_judgments(
            [
                row(
                    "user-1",
                    "session-1",
                    "span-repeated-invocation",
                    &repeated_invocation,
                ),
                row("user-1", "session-1", "span-terminal", &decided),
                row("user-1", "session-1", "span-started", &started),
            ],
            projection_input(&[]),
        );
        assert_eq!(view.evaluations, 2);
        assert_eq!(view.selected, 2);
        assert_eq!(view.terminal_missing, 0);
        assert_eq!(view.models[0].model, "jev-1.13.0");
        assert_eq!(view.models[0].observed_invocations, 1);
        assert!(view.render().contains("jev-1.13.0 via jet"));
        let compact = view.render_compact();
        assert!(compact.contains("2 terminal results"), "{compact}");
        assert!(compact.contains("2 selected"), "{compact}");
        assert!(!compact.contains("2 evaluated"), "{compact}");
    }

    #[test]
    fn projection_keeps_missing_terminal_conflict_and_scope_explicit() {
        let started = observation(
            "evaluation-open",
            astra_turn_types::ToolResultSelectionOutcomeV1::Started,
        );
        let skipped = observation(
            "evaluation-conflict",
            astra_turn_types::ToolResultSelectionOutcomeV1::NotDispatched {
                reason: astra_turn_types::ToolResultSelectionNotDispatchedReasonV1::NoOffering,
            },
        );
        let unavailable = observation(
            "evaluation-conflict",
            astra_turn_types::ToolResultSelectionOutcomeV1::Unavailable {
                reason: astra_turn_types::ToolResultSelectionUnavailableReasonV1::ExecutionError,
                execution: None,
            },
        );
        let view = project_tool_result_judgments(
            [
                row("other", "session-1", "wrong-owner", &started),
                row("user-1", "session-1", "span-started", &started),
                row("user-1", "session-1", "span-a", &skipped),
                row("user-1", "session-1", "span-b", &unavailable),
            ],
            projection_input(&[]),
        );
        assert_eq!(view.terminal_missing, 1);
        assert_eq!(view.conflicting, 1); // terminal contradiction; other owners stay invisible
        assert_eq!(view.evaluations, 2);
        assert_eq!(view.not_dispatched, 0);
        assert_eq!(view.unavailable, 0);
        let compact = view.render_compact();
        assert!(
            compact.contains("1 started; terminal result unknown"),
            "{compact}"
        );
        assert!(compact.contains("1 conflicting"), "{compact}");
    }

    #[test]
    fn application_receipt_is_reported_without_inventing_an_evaluation() {
        use astra_turn_types::{
            ToolResultProjectionDecisionV1, ToolResultProjectionDispositionV1,
            ToolResultProjectionRangeV1, ToolResultProjectionReceiptV1,
            ToolResultProjectionWireStateV1,
        };
        let body = b"selected evidence";
        let decision = ToolResultProjectionDecisionV1::new(
            "run-1",
            "call-1",
            "1".repeat(64),
            100,
            "2".repeat(64),
            "3".repeat(64),
            ToolResultProjectionDispositionV1::Selected,
            vec![ToolResultProjectionRangeV1 {
                chunk_id: "chunk-1".into(),
                start_byte: 0,
                end_byte: 10,
            }],
            Some("invocation-1".into()),
            None,
            body,
        )
        .unwrap();
        let receipt = ToolResultProjectionReceiptV1 {
            decision_sha256: decision.decision_sha256.clone(),
            provider_wire_sha256: "4".repeat(64),
            state: ToolResultProjectionWireStateV1::Included,
            actual_ranges: decision.selected_ranges.clone(),
            actual_body_sha256: Some(decision.rendered_body_sha256.clone()),
            reason: None,
        };
        let applications = vec![
            crate::inference_execution::ToolResultProjectionApplicationFact {
                attempt_id: "attempt-1".into(),
                binding: astra_turn_types::ToolResultProjectionBindingV1 { decision, receipt },
            },
        ];
        let view = project_tool_result_judgments([], projection_input(&applications));
        assert_eq!(view.evaluations, 0);
        assert_eq!(view.applications.included, 1);
        assert_eq!(
            view.evaluation_coverage,
            ToolResultJudgmentCoverage::NotObserved
        );
        assert_eq!(
            view.application_coverage,
            ToolResultJudgmentCoverage::Available
        );
        assert!(view.render().contains("0 evaluation(s)"));
    }

    #[test]
    fn malformed_selection_trace_and_incompatible_phases_are_not_silent() {
        let malformed = ToolResultSelectionTraceRow {
            user_id: "user-1".into(),
            session_id: "session-1".into(),
            metadata_json: Some(
                serde_json::json!({
                    "name": TOOL_RESULT_SELECTION_TRACE_NAME,
                    "attrs": {"tool_result_selection.v1": "not-json"}
                })
                .to_string(),
            ),
            metadata_oversized: false,
        };
        let started = observation(
            "evaluation-1",
            astra_turn_types::ToolResultSelectionOutcomeV1::Started,
        );
        let mut incompatible_started = started.clone();
        incompatible_started.correlation.round = 9;
        let terminal = observation(
            "evaluation-1",
            astra_turn_types::ToolResultSelectionOutcomeV1::NotDispatched {
                reason: astra_turn_types::ToolResultSelectionNotDispatchedReasonV1::NoOffering,
            },
        );
        let view = project_tool_result_judgments(
            [
                malformed,
                row("user-1", "session-1", "started", &incompatible_started),
                row("user-1", "session-1", "terminal", &terminal),
            ],
            projection_input(&[]),
        );
        assert_eq!(view.invalid_or_oversized, 1);
        assert_eq!(view.conflicting, 1);
        assert_eq!(view.not_dispatched, 0);
        assert_eq!(
            view.evaluation_coverage,
            ToolResultJudgmentCoverage::CaptureIncomplete
        );
    }

    #[test]
    fn conflict_alone_downgrades_evaluation_coverage() {
        let first = observation(
            "evaluation-1",
            astra_turn_types::ToolResultSelectionOutcomeV1::NotDispatched {
                reason: astra_turn_types::ToolResultSelectionNotDispatchedReasonV1::NoOffering,
            },
        );
        let second = observation(
            "evaluation-1",
            astra_turn_types::ToolResultSelectionOutcomeV1::Unavailable {
                reason: astra_turn_types::ToolResultSelectionUnavailableReasonV1::ExecutionError,
                execution: None,
            },
        );
        let view = project_tool_result_judgments(
            [
                row("user-1", "session-1", "first", &first),
                row("user-1", "session-1", "second", &second),
            ],
            projection_input(&[]),
        );
        assert_eq!(view.conflicting, 1);
        assert_eq!(
            view.evaluation_coverage,
            ToolResultJudgmentCoverage::CaptureIncomplete
        );
    }

    #[test]
    fn asymmetric_source_failures_remain_visible() {
        let evaluations_missing = ToolResultJudgmentView {
            evaluation_coverage: ToolResultJudgmentCoverage::SourceUnavailable,
            application_coverage: ToolResultJudgmentCoverage::NotObserved,
            ..Default::default()
        }
        .render();
        assert!(evaluations_missing.contains("Evaluation evidence unavailable"));
        let applications_missing = ToolResultJudgmentView {
            evaluation_coverage: ToolResultJudgmentCoverage::NotObserved,
            application_coverage: ToolResultJudgmentCoverage::SourceUnavailable,
            ..Default::default()
        }
        .render();
        assert!(applications_missing.contains("Application evidence unavailable"));
        assert!(!applications_missing.contains("none observed"));

        let incomplete = ToolResultJudgmentView {
            evaluation_coverage: ToolResultJudgmentCoverage::CaptureIncomplete,
            ..Default::default()
        }
        .render();
        assert!(incomplete.contains("evaluation evidence incomplete"));
        assert!(!incomplete.contains("evaluation capture truncated"));

        let truncated = ToolResultJudgmentView {
            evaluation_coverage: ToolResultJudgmentCoverage::CaptureTruncated,
            ..Default::default()
        }
        .render();
        assert!(truncated.contains("evaluation capture truncated"));
        assert!(!truncated.contains("evaluation evidence incomplete"));

        let mixed = ToolResultJudgmentView {
            evaluation_coverage: ToolResultJudgmentCoverage::Available,
            evaluations: 5,
            selected: 1,
            baseline: 1,
            not_dispatched: 1,
            unavailable: 1,
            terminal_missing: 1,
            ..Default::default()
        }
        .render_compact();
        for expected in [
            "4 terminal results",
            "1 selected",
            "1 kept baseline",
            "1 skipped",
            "1 unavailable",
            "1 started; terminal result unknown",
        ] {
            assert!(mixed.contains(expected), "missing {expected}: {mixed}");
        }
    }

    #[tokio::test]
    async fn source_deadlines_preserve_the_other_completed_source() {
        let (trace, applications) = independently_bounded_sources(
            async { Ok::<_, ServiceError>(7u8) },
            std::future::pending::<ServiceResult<u8>>(),
            std::time::Duration::from_millis(1),
        )
        .await;
        assert_eq!(trace, Ok(7));
        assert_eq!(applications, Err(()));

        let (trace, applications) = independently_bounded_sources(
            std::future::pending::<ServiceResult<u8>>(),
            async { Ok::<_, ServiceError>(9u8) },
            std::time::Duration::from_millis(1),
        )
        .await;
        assert_eq!(trace, Err(()));
        assert_eq!(applications, Ok(9));
    }
}
