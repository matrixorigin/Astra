//! Server-owned Explain Analyze artifact publication and recovery.
//!
//! Explain facts are produced by the server run owner.  Clients may render a
//! local companion, but the model-facing artifact must live beside the server
//! introspect reader for explicit, lazy discovery and fixed-handle pagination.

use super::run::engine::RunEngine;
use astra_core::SharedPool;
use astra_services::{
    DatabaseSessionArtifactStore, SessionArtifactJsonRecord, SessionArtifactJsonStore,
    SessionArtifactListCursor, SessionArtifactListPage, StoredSessionArtifact,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const ARTIFACT_URI_PREFIX: &str = "artifact://session/explain-analyze/";
pub(crate) const ARTIFACT_KIND: &str = "explain_analyze_snapshot";
const ARTIFACT_TYPE: &str = "explain_analyze_snapshot";
const CONTENT_TYPE: &str = "application/json";
const STORAGE: &str = "database_session";
const REPRESENTATION: &str = "canonical";
const ARTIFACT_SCHEMA_VERSION: u16 = 1;
const MAX_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_WINDOW_BYTES: usize = 8 * 1024;
const MAX_WINDOW_BYTES: usize = 64 * 1024;
const EXACT_RUN_OBSERVATION_EVENTS: usize = astra_services::runs::MAX_RUN_OBSERVATION_EVENTS;

#[cfg(test)]
tokio::task_local! {
    static ARTIFACT_FETCHES: ArtifactFetchCounters;
}

#[cfg(test)]
#[derive(Clone, Default)]
struct ArtifactFetchCounters {
    discovery: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    recovery: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ArtifactFetchCounts {
    pub(crate) total: usize,
    pub(crate) discovery: usize,
    pub(crate) recovery: usize,
}

#[cfg(test)]
pub(crate) async fn count_explain_artifact_fetches<F>(future: F) -> (F::Output, ArtifactFetchCounts)
where
    F: std::future::Future,
{
    let counters = ArtifactFetchCounters::default();
    let output = ARTIFACT_FETCHES.scope(counters.clone(), future).await;
    let load = |counter: &std::sync::atomic::AtomicUsize| {
        counter.load(std::sync::atomic::Ordering::Relaxed)
    };
    (
        output,
        ArtifactFetchCounts {
            discovery: load(&counters.discovery),
            recovery: load(&counters.recovery),
            total: load(&counters.discovery) + load(&counters.recovery),
        },
    )
}

#[derive(Clone, Copy)]
enum ArtifactFetchPurpose {
    Discovery,
    Recovery,
}

#[cfg(test)]
fn record_artifact_fetch(purpose: ArtifactFetchPurpose) {
    ARTIFACT_FETCHES
        .try_with(|counters| {
            use std::sync::atomic::Ordering;
            match purpose {
                ArtifactFetchPurpose::Discovery => {
                    counters.discovery.fetch_add(1, Ordering::Relaxed);
                }
                ArtifactFetchPurpose::Recovery => {
                    counters.recovery.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
        .ok();
}

#[cfg(not(test))]
fn record_artifact_fetch(_purpose: ArtifactFetchPurpose) {}

/// The run is the immutable observation identity.  A run may be resumed by
/// another owner generation, but it must never publish a second logical
/// Explain artifact under a different ID.  The exact turn and generation are
/// retained in the envelope and used for validation.
fn artifact_id(run_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"astra.explain-analyze.server-artifact.v1\0");
    hasher.update((run_id.len() as u64).to_be_bytes());
    hasher.update(run_id.as_bytes());
    // session_artifacts.artifact_id is VARCHAR(64). The URI supplies the
    // human-readable namespace; adding it here breaks every database write.
    format!("{:x}", hasher.finalize())
}

async fn load_snapshot_artifact(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    artifact_id: &str,
    purpose: ArtifactFetchPurpose,
) -> Result<Option<StoredSessionArtifact>, String> {
    record_artifact_fetch(purpose);
    DatabaseSessionArtifactStore::new(pool.settings().clone())
        .with_pool(pool.clone())
        .load_json_artifact(user_id, session_id, artifact_id)
        .await
        .map_err(|error| error.to_string())
}

/// Repair a missing snapshot only from the exact, completed durable run.
/// Paused captures are still mutable and must not acquire an immutable report.
pub(crate) async fn recover_completed_snapshot(
    pool: Option<&SharedPool>,
    run: &astra_services::runs::DurableRunRecord,
) -> Result<Option<String>, String> {
    if run.status != "completed" || !astra_services::runs::run_requested_explain_analyze(run) {
        return Ok(None);
    }
    let Some(pool) = pool else { return Ok(None) };
    let id = artifact_id(&run.run_id);
    if let Some(existing) = load_snapshot_artifact(
        pool,
        &run.user_id,
        &run.session_id,
        &id,
        ArtifactFetchPurpose::Recovery,
    )
    .await
    .map_err(|error| format!("check recovered Explain artifact: {error}"))?
    {
        let status = validate_snapshot_payload(
            &existing,
            &run.session_id,
            Some(&run.run_id),
            Some(run.run_generation),
        )?;
        if status == "unavailable" {
            return Err(
                "the stored report is unavailable; it is not a readable snapshot".to_string(),
            );
        }
        return Ok(Some(artifact_handle(&id)));
    }
    // Use the canonical durable-to-wire projection, including retained gaps.
    let events: Vec<_> = run
        .events
        .iter()
        .cloned()
        .map(astra_services::runs::transform_run_event_for_client)
        .filter(|event| !event.is_null())
        .collect();
    let turns: std::collections::BTreeSet<_> = events
        .iter()
        .filter_map(|event| astra_turn_types::decode_explain_analyze_wire(event).ok())
        .filter(|fact| fact.run_id == run.run_id)
        .map(|fact| fact.turn_id)
        .collect();
    if turns.len() != 1 {
        return Err("completed Explain capture has missing or ambiguous turn identity".to_string());
    }
    let turn = turns.first().expect("one turn");
    persist_snapshot(
        Some(pool),
        &run.user_id,
        &run.session_id,
        &run.run_id,
        turn,
        run.run_generation,
        &events,
    )
    .await
}

pub(crate) async fn record_explain_publication(
    run_engine: &RunEngine,
    user_id: &str,
    session_id: &str,
    mut outcome: astra_turn_types::ArtifactPublicationV1,
) -> Value {
    let run_id = outcome.run_id.as_str();
    let owner_generation = outcome.execution_owner_generation;
    let payload = serde_json::to_value(&outcome).expect("serializable publication outcome");
    let mut identity = Sha256::new();
    identity.update(b"astra.artifact-publication.v1\0");
    identity.update(astra_core::canonical_json_string(&payload).as_bytes());
    let event = json!({ "event_type":"artifact_publication",
        "idempotency_key":format!("{:x}", identity.finalize()), "data":payload });

    let recorded = run_engine
        .append_events_if_current_generation_and_status(
            user_id,
            session_id,
            run_id,
            owner_generation,
            &["completed", "failed", "cancelled", "delegated"],
            &[event],
        )
        .await;
    if recorded != Ok(true) {
        outcome.recorded = false;
        tracing::warn!(run_id, result = ?recorded,
            "could not retain Explain artifact publication outcome; notifying live client");
    }
    outcome.to_wire()
}

pub(crate) async fn publish_recovered_explain(
    pool: Option<&SharedPool>,
    run_engine: &RunEngine,
    run: &astra_services::runs::DurableRunRecord,
) -> Option<Value> {
    use astra_turn_types::{ArtifactPublicationResult, ArtifactPublicationV1};
    if run.status != "completed" || !astra_services::runs::run_requested_explain_analyze(run) {
        return None;
    }
    let turn_id = run
        .events
        .iter()
        .cloned()
        .map(astra_services::runs::transform_run_event_for_client)
        .filter_map(|event| astra_turn_types::decode_explain_analyze_wire(&event).ok())
        .find(|fact| fact.run_id == run.run_id)
        .map(|fact| fact.turn_id)
        .unwrap_or_else(|| "unknown".to_string());
    let result = match crate::server::explain_analyze_artifact::recover_completed_snapshot(
        pool, run,
    )
    .await
    {
        Ok(Some(handle)) => ArtifactPublicationResult::Published { handle },
        failure => {
            tracing::warn!(run_id = %run.run_id, result = ?failure, "Explain report recovery unavailable");
            ArtifactPublicationResult::Unavailable {
                reason_code: "recovery_failed".into(),
                message: "The server could not recover a readable report for this run.".into(),
            }
        }
    };
    let outcome = ArtifactPublicationV1 {
        schema_version: 1,
        run_id: run.run_id.clone(),
        turn_id,
        execution_owner_generation: run.run_generation,
        artifact_type: "explain_analyze_snapshot".into(),
        recorded: true,
        result,
    };
    if let Some(existing) = run.events.iter().rev().find_map(|event| {
        ArtifactPublicationV1::from_wire(&astra_services::runs::transform_run_event_for_client(
            event.clone(),
        ))
        .ok()
    }) && existing == outcome
    {
        return Some(existing.to_wire());
    }
    let wire = record_explain_publication(run_engine, &run.user_id, &run.session_id, outcome).await;
    Some(wire)
}

pub(crate) fn artifact_handle(artifact_id: &str) -> String {
    format!("{ARTIFACT_URI_PREFIX}{artifact_id}")
}

fn artifact_id_from_handle(handle: &str) -> Option<&str> {
    let id = handle.strip_prefix(ARTIFACT_URI_PREFIX)?;
    (!id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    .then_some(id)
}

struct CapturedExplainEvents {
    events: Vec<Value>,
    facts: Vec<astra_turn_types::ExplainAnalyzeEventV1>,
    invalid_event_count: usize,
    foreign_event_count: usize,
}

fn explain_events(
    events: &[Value],
    expected_run_id: &str,
    expected_turn_id: &str,
) -> CapturedExplainEvents {
    let mut captured = CapturedExplainEvents {
        events: Vec::new(),
        facts: Vec::new(),
        invalid_event_count: 0,
        foreign_event_count: 0,
    };
    for event in events.iter().filter(|event| {
        event.get("type").and_then(Value::as_str)
            == Some(astra_turn_types::EXPLAIN_ANALYZE_EVENT_TYPE)
    }) {
        let Ok(fact) = astra_turn_types::decode_explain_analyze_wire(event) else {
            captured.invalid_event_count = captured.invalid_event_count.saturating_add(1);
            continue;
        };
        if fact.run_id != expected_run_id || fact.turn_id != expected_turn_id {
            captured.foreign_event_count = captured.foreign_event_count.saturating_add(1);
            continue;
        }
        captured.events.push(event.clone());
        captured.facts.push(fact);
    }
    captured
}

struct CaptureAssessment {
    status: &'static str,
    delivery_degraded: bool,
    graph_diagnostics: Vec<String>,
    coverage_gaps: Vec<String>,
    open_node_count: usize,
}

struct GraphProjection {
    has_terminal_turn: bool,
    graph_diagnostics: Vec<String>,
    coverage_gaps: Vec<String>,
    open_node_count: usize,
}

fn graph_projection(facts: &[astra_turn_types::ExplainAnalyzeEventV1]) -> GraphProjection {
    let mut graph = astra_turn_types::ExplainAnalyzeGraphV1::default();
    for fact in facts {
        graph.apply(fact.clone());
    }
    graph.finish_ingest();
    project_graph(&graph)
}

fn project_graph(graph: &astra_turn_types::ExplainAnalyzeGraphV1) -> GraphProjection {
    GraphProjection {
        has_terminal_turn: graph.nodes().iter().any(|node| {
            node.kind == astra_turn_types::ExplainAnalyzeNodeKindV1::Turn && node.terminal_observed
        }),
        open_node_count: graph
            .nodes()
            .iter()
            .filter(|node| !node.terminal_observed)
            .count(),
        graph_diagnostics: graph
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.code.as_str().to_string())
            .collect(),
        coverage_gaps: graph
            .coverage_gaps()
            .into_iter()
            .map(|gap| gap.as_str().to_string())
            .collect(),
    }
}

fn assess_capture(events: &[Value], captured: &CapturedExplainEvents) -> CaptureAssessment {
    let delivery_degraded = events.iter().any(|event| {
        event.get("type").and_then(Value::as_str) == Some("stream_gap")
            && event
                .get("explain_analyze_recovered")
                .and_then(Value::as_bool)
                != Some(true)
    });
    let projection = graph_projection(&captured.facts);
    let complete = projection.has_terminal_turn
        && projection.open_node_count == 0
        && !delivery_degraded
        && captured.invalid_event_count == 0
        && captured.foreign_event_count == 0
        && projection.graph_diagnostics.is_empty()
        && projection.coverage_gaps.is_empty();
    CaptureAssessment {
        status: if complete { "complete" } else { "partial" },
        delivery_degraded,
        graph_diagnostics: projection.graph_diagnostics,
        coverage_gaps: projection.coverage_gaps,
        open_node_count: projection.open_node_count,
    }
}

fn turn_number(turn_id: &str) -> Option<u32> {
    turn_id.strip_prefix("turn-")?.parse().ok()
}

async fn persist_record_if_absent(
    store: &DatabaseSessionArtifactStore,
    record: SessionArtifactJsonRecord,
) -> Result<(), String> {
    let same_record = |existing: &StoredSessionArtifact| {
        existing.status.as_deref() == Some("active")
            && existing.user_id == record.user_id
            && existing.session_id == record.session_id
            && existing.artifact_id == record.artifact_id
            && existing.artifact_kind == record.artifact_kind
            && existing.content == record.content
            && existing.metadata == record.metadata
    };
    if let Some(existing) = store
        .load_json_artifact(&record.user_id, &record.session_id, &record.artifact_id)
        .await
        .map_err(|error| format!("check Explain Analyze artifact: {error}"))?
    {
        return if same_record(&existing) {
            Ok(())
        } else {
            Err("immutable Explain Analyze artifact identity has conflicting content".to_string())
        };
    }
    match store.persist_json_artifact(record.clone()).await {
        Ok(_) => Ok(()),
        Err(error) => {
            // A concurrent publisher may have won the immutable insert. Only
            // accept that race after verifying the winner is byte-for-byte the
            // same snapshot; an unavailable marker or another generation is
            // an explicit identity conflict.
            if let Some(existing) = store
                .load_json_artifact(&record.user_id, &record.session_id, &record.artifact_id)
                .await
                .map_err(|load_error| {
                    format!(
                        "persist Explain Analyze artifact: {error}; verify concurrent publication: {load_error}"
                    )
                })?
                && same_record(&existing)
            {
                Ok(())
            } else {
                Err(format!("persist Explain Analyze artifact: {error}"))
            }
        }
    }
}

fn record_for_payload(
    artifact_id: String,
    user_id: &str,
    session_id: &str,
    turn_id: &str,
    payload: Value,
    status: &str,
    owner_generation: Option<u64>,
) -> Result<SessionArtifactJsonRecord, String> {
    let bytes = serde_json::to_vec_pretty(&payload)
        .map_err(|error| format!("encode Explain Analyze artifact: {error}"))?;
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(format!(
            "Explain Analyze artifact exceeds the {MAX_ARTIFACT_BYTES} byte bound"
        ));
    }
    let checksum_sha256 = format!("{:x}", Sha256::digest(&bytes));
    let mut metadata = json!({
        "artifact_schema_version": ARTIFACT_SCHEMA_VERSION,
        "artifact_type": ARTIFACT_TYPE,
        "content_type": CONTENT_TYPE,
        "storage": STORAGE,
        "representation": REPRESENTATION,
        "status": status,
        "size_bytes": bytes.len(),
        "checksum_sha256": checksum_sha256,
    });
    if let Some(owner_generation) = owner_generation {
        metadata["execution_owner_generation"] = Value::from(owner_generation);
    }
    Ok(SessionArtifactJsonRecord {
        artifact_id,
        session_id: session_id.to_string(),
        user_id: user_id.to_string(),
        artifact_kind: ARTIFACT_KIND.to_string(),
        source: Some("server_runtime".to_string()),
        turn: turn_number(turn_id),
        round: None,
        content: payload,
        metadata: Some(metadata),
        references: Vec::new(),
    })
}

/// Persist the server-owned canonical snapshot and return its opaque reader
/// handle. The operation is deterministic per run; the exact turn and owner
/// generation remain part of the immutable envelope, so a retry cannot create
/// a second logical artifact or move discovery to a duplicate identity.
pub(crate) async fn persist_snapshot(
    pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
    expected_run_id: &str,
    expected_turn_id: &str,
    owner_generation: u64,
    events: &[Value],
) -> Result<Option<String>, String> {
    let Some(pool) = pool else {
        return Ok(None);
    };
    let captured = explain_events(events, expected_run_id, expected_turn_id);
    if captured.events.is_empty() {
        return Ok(None);
    }
    let artifact_id = artifact_id(expected_run_id);
    let handle = artifact_handle(&artifact_id);
    let store = DatabaseSessionArtifactStore::new(pool.settings().clone()).with_pool(pool.clone());

    let assessment = assess_capture(events, &captured);
    let payload = json!({
        "artifact_schema_version": ARTIFACT_SCHEMA_VERSION,
        "artifact_kind": "explain_analyze",
        "artifact_type": ARTIFACT_TYPE,
        "content_type": CONTENT_TYPE,
        "storage": STORAGE,
        "representation": REPRESENTATION,
        "schema_version": astra_turn_types::EXPLAIN_ANALYZE_SCHEMA_VERSION,
        "session_id": session_id,
        "run_id": expected_run_id,
        "turn_id": expected_turn_id,
        "capture_status": assessment.status,
        "delivery_degraded": assessment.delivery_degraded,
        "invalid_event_count": captured.invalid_event_count,
        "foreign_event_count": captured.foreign_event_count,
        "open_node_count": assessment.open_node_count,
        "graph_diagnostics": assessment.graph_diagnostics,
        "coverage_gaps": assessment.coverage_gaps,
        "execution_owner_generation": owner_generation,
        "events": captured.events,
    });
    let record = record_for_payload(
        artifact_id,
        user_id,
        session_id,
        expected_turn_id,
        payload,
        assessment.status,
        Some(owner_generation),
    )?;
    persist_record_if_absent(&store, record).await?;
    Ok(Some(handle))
}

fn envelope_status(artifact: &StoredSessionArtifact) -> Result<&str, String> {
    if artifact.status.as_deref() != Some("active") {
        return Err(format!(
            "server Explain Analyze artifact is not active (storage status: {})",
            artifact.status.as_deref().unwrap_or("unknown")
        ));
    }
    let content = &artifact.content;
    if content
        .get("artifact_schema_version")
        .and_then(Value::as_u64)
        != Some(u64::from(ARTIFACT_SCHEMA_VERSION))
        || content.get("artifact_kind").and_then(Value::as_str) != Some("explain_analyze")
        || content.get("artifact_type").and_then(Value::as_str) != Some(ARTIFACT_TYPE)
        || content.get("content_type").and_then(Value::as_str) != Some(CONTENT_TYPE)
        || content.get("storage").and_then(Value::as_str) != Some(STORAGE)
        || content.get("representation").and_then(Value::as_str) != Some(REPRESENTATION)
        || content.get("schema_version").and_then(Value::as_u64)
            != Some(u64::from(astra_turn_types::EXPLAIN_ANALYZE_SCHEMA_VERSION))
    {
        return Err(
            "server Explain Analyze artifact has an unsupported envelope or schema".to_string(),
        );
    }
    let status = content
        .get("capture_status")
        .and_then(Value::as_str)
        .ok_or_else(|| "server Explain Analyze artifact has no capture status".to_string())?;
    if !matches!(status, "complete" | "partial" | "unavailable") {
        return Err(format!(
            "server Explain Analyze artifact has an unknown capture status: {status}"
        ));
    }
    let metadata_status = artifact
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("status"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "server Explain Analyze artifact has no storage status metadata".to_string()
        })?;
    if metadata_status != status {
        return Err(format!(
            "server Explain Analyze artifact storage status {metadata_status} disagrees with capture status {status}"
        ));
    }
    Ok(status)
}

fn metadata_string_array(metadata: &Value, field: &str) -> Result<Vec<String>, String> {
    metadata
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("Explain Analyze artifact has no {field} projection"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("Explain Analyze artifact {field} contains a non-string"))
        })
        .collect()
}

fn validate_storage_integrity(artifact: &StoredSessionArtifact) -> Result<Vec<u8>, String> {
    let metadata = artifact
        .metadata
        .as_ref()
        .ok_or_else(|| "server Explain Analyze artifact has no storage metadata".to_string())?;
    if metadata
        .get("artifact_schema_version")
        .and_then(Value::as_u64)
        != Some(u64::from(ARTIFACT_SCHEMA_VERSION))
        || metadata.get("artifact_type").and_then(Value::as_str) != Some(ARTIFACT_TYPE)
        || metadata.get("content_type").and_then(Value::as_str) != Some(CONTENT_TYPE)
        || metadata.get("storage").and_then(Value::as_str) != Some(STORAGE)
        || metadata.get("representation").and_then(Value::as_str) != Some(REPRESENTATION)
    {
        return Err("server Explain Analyze artifact storage metadata is unsupported".to_string());
    }
    let expected_size = metadata
        .get("size_bytes")
        .and_then(Value::as_u64)
        .and_then(|size| usize::try_from(size).ok())
        .ok_or_else(|| "server Explain Analyze artifact has no valid size metadata".to_string())?;
    let expected_checksum = metadata
        .get("checksum_sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| "server Explain Analyze artifact has no checksum metadata".to_string())?;
    if expected_checksum.len() != 64
        || !expected_checksum
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(
            "server Explain Analyze artifact has an invalid checksum metadata value".to_string(),
        );
    }
    let bytes = serde_json::to_vec_pretty(&artifact.content)
        .map_err(|error| format!("encode Explain Analyze artifact for integrity check: {error}"))?;
    if bytes.len() != expected_size {
        return Err(format!(
            "server Explain Analyze artifact size metadata {} disagrees with {} bytes",
            expected_size,
            bytes.len()
        ));
    }
    let actual_checksum = format!("{:x}", Sha256::digest(&bytes));
    if !expected_checksum.eq_ignore_ascii_case(&actual_checksum) {
        return Err(
            "server Explain Analyze artifact checksum metadata disagrees with its content"
                .to_string(),
        );
    }
    Ok(bytes)
}

fn validate_capture_projection(
    content: &Value,
    status: &str,
    graph: &astra_turn_types::ExplainAnalyzeGraphV1,
) -> Result<(), String> {
    let projection = project_graph(graph);
    let delivery_degraded = content
        .get("delivery_degraded")
        .and_then(Value::as_bool)
        .ok_or_else(|| "Explain Analyze artifact has no delivery projection".to_string())?;
    let invalid_event_count = content
        .get("invalid_event_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| "Explain Analyze artifact has no invalid-event projection".to_string())?;
    let foreign_event_count = content
        .get("foreign_event_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| "Explain Analyze artifact has no foreign-event projection".to_string())?;
    let open_node_count = content
        .get("open_node_count")
        .and_then(Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
        .ok_or_else(|| "Explain Analyze artifact has no valid open-node projection".to_string())?;
    let graph_diagnostics = metadata_string_array(content, "graph_diagnostics")?;
    let coverage_gaps = metadata_string_array(content, "coverage_gaps")?;
    if open_node_count != projection.open_node_count
        || graph_diagnostics != projection.graph_diagnostics
        || coverage_gaps != projection.coverage_gaps
    {
        return Err(
            "Explain Analyze artifact graph projection disagrees with its typed facts".to_string(),
        );
    }
    if status == "complete"
        && (!projection.has_terminal_turn
            || projection.open_node_count != 0
            || delivery_degraded
            || invalid_event_count != 0
            || foreign_event_count != 0
            || !projection.graph_diagnostics.is_empty()
            || !projection.coverage_gaps.is_empty())
    {
        return Err(
            "Explain Analyze artifact claims complete capture despite incomplete graph facts"
                .to_string(),
        );
    }
    Ok(())
}

fn validate_snapshot_payload<'a>(
    artifact: &'a StoredSessionArtifact,
    reader_session_id: &str,
    expected_run_id: Option<&str>,
    expected_owner_generation: Option<u64>,
) -> Result<&'a str, String> {
    validate_snapshot_payload_with_bytes(
        artifact,
        reader_session_id,
        expected_run_id,
        expected_owner_generation,
    )
    .map(|(status, _, _)| status)
}

fn validate_snapshot_payload_with_bytes<'a>(
    artifact: &'a StoredSessionArtifact,
    reader_session_id: &str,
    expected_run_id: Option<&str>,
    expected_owner_generation: Option<u64>,
) -> Result<(&'a str, Vec<u8>, astra_turn_types::ExplainAnalyzeGraphV1), String> {
    if artifact.artifact_kind != ARTIFACT_KIND {
        return Err("artifact handle does not name a server Explain Analyze snapshot".into());
    }
    if artifact.status.as_deref() != Some("active") {
        return Err(format!(
            "server Explain Analyze artifact is not active (storage status: {})",
            artifact.status.as_deref().unwrap_or("unknown")
        ));
    }
    if artifact.session_id != reader_session_id {
        return Err(
            "Explain Analyze artifact session ownership does not match the reader".to_string(),
        );
    }
    let status = envelope_status(artifact)?;
    let bytes = validate_storage_integrity(artifact)?;
    let content = &artifact.content;
    if content.get("session_id").and_then(Value::as_str) != Some(reader_session_id) {
        return Err(
            "Explain Analyze artifact payload session ownership does not match the reader"
                .to_string(),
        );
    }
    let run_id = content
        .get("run_id")
        .and_then(Value::as_str)
        .filter(|run_id| !run_id.trim().is_empty())
        .ok_or_else(|| "Explain Analyze artifact has no run identity".to_string())?;
    if expected_run_id.is_some_and(|expected| expected != run_id) {
        return Err(
            "Explain Analyze artifact run identity does not match the requested Explain run"
                .to_string(),
        );
    }
    let turn_id = content
        .get("turn_id")
        .and_then(Value::as_str)
        .filter(|turn_id| !turn_id.trim().is_empty())
        .ok_or_else(|| "Explain Analyze artifact has no turn identity".to_string())?;
    let owner_generation = content
        .get("execution_owner_generation")
        .and_then(Value::as_u64)
        .ok_or_else(|| "Explain Analyze artifact has no execution owner generation".to_string())?;
    if expected_owner_generation.is_some_and(|expected| expected != owner_generation) {
        return Err(
            "Explain Analyze artifact owner generation does not match the requested Explain run"
                .to_string(),
        );
    }
    let metadata_generation = artifact
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("execution_owner_generation"))
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            "server Explain Analyze artifact has no owner generation metadata".to_string()
        })?;
    if metadata_generation != owner_generation {
        return Err(
            "server Explain Analyze artifact owner generation metadata disagrees with its payload"
                .to_string(),
        );
    }
    let Some(events) = content.get("events").and_then(Value::as_array) else {
        return Err("Explain Analyze artifact has no typed event array".to_string());
    };
    if status == "unavailable" {
        if !events.is_empty() {
            return Err("unavailable Explain Analyze artifact contains runtime facts".to_string());
        }
        return Ok((
            status,
            bytes,
            astra_turn_types::ExplainAnalyzeGraphV1::default(),
        ));
    }
    if events.is_empty() {
        return Err("readable Explain Analyze artifact contains no runtime facts".to_string());
    }
    let mut graph = astra_turn_types::ExplainAnalyzeGraphV1::default();
    for event in events {
        let fact = astra_turn_types::decode_explain_analyze_wire(event).map_err(|error| {
            format!("Explain Analyze artifact contains an invalid typed event: {error}")
        })?;
        if fact.run_id != run_id || fact.turn_id != turn_id {
            return Err(
                "Explain Analyze artifact contains a fact outside its declared run/turn"
                    .to_string(),
            );
        }
        graph.apply(fact);
    }
    graph.finish_ingest();
    validate_capture_projection(content, status, &graph)?;
    Ok((status, bytes, graph))
}

/// Explicit discovery resolves one exact identity. Pagination uses its concrete handle.
#[derive(serde::Deserialize)]
#[serde(tag = "target", rename_all = "snake_case", deny_unknown_fields)]
enum ExplainSelector {
    Previous {},
    Run { run_id: String },
}

pub(crate) async fn resolve_selector(
    pool: Option<&SharedPool>,
    store: Option<&dyn SessionArtifactJsonStore>,
    engine: &RunEngine,
    user_id: &str,
    session_id: &str,
    current_root: &str,
    args: &Value,
) -> Result<String, String> {
    if args.get("artifact").is_some() {
        return Err("explain and artifact are mutually exclusive".into());
    }
    let selector: ExplainSelector = serde_json::from_value(
        args.get("explain")
            .cloned()
            .ok_or("explain selector is required")?,
    )
    .map_err(|error| format!("invalid Explain selector: {error}"))?;
    let (offset, max_bytes) = window_arguments(args)?;
    if offset != 0 {
        return Err(
            "Explain selectors require offset 0. Only returned artifact handles support pagination; ordinary run projections do not."
                .into(),
        );
    }
    if matches!(
        args.get("source_policy").and_then(Value::as_str),
        Some("live_only" | "local_only")
    ) {
        return Err("server Explain snapshots require a durable server source".into());
    }
    let (run_id, generation) = match selector {
        ExplainSelector::Previous {} => engine
            .find_latest_explain_analyze_root(user_id, session_id, Some(current_root))
            .await?
            .ok_or("no previous Explain Analyze root exists in this session")?,
        ExplainSelector::Run { run_id } => {
            let observation = engine
                .load_run_observation(user_id, &run_id, EXACT_RUN_OBSERVATION_EVENTS)
                .await?
                .filter(|observation| observation.run.session_id == session_id)
                .ok_or("Explain Analyze run was not found in the active session")?;
            if !astra_services::runs::run_requested_explain_analyze(&observation.run) {
                return render_exact_run_projection(&observation, args, max_bytes);
            }
            (observation.run.run_id, observation.run.run_generation)
        }
    };
    let store = store.ok_or("server Explain Analyze artifact reader is unavailable")?;
    let id = artifact_id(&run_id);
    record_artifact_fetch(ArtifactFetchPurpose::Discovery);
    let mut artifact = store
        .load_json_artifact(user_id, session_id, &id)
        .await
        .map_err(|error| format!("load Explain Analyze artifact: {error}"))?;
    // Only physical absence enters recovery. Integrity/status failures are
    // handled by the same validator as handle reads and never select older runs.
    if artifact.is_none() {
        let run = engine
            .load_run(user_id, &run_id)
            .await?
            .filter(|run| run.session_id == session_id && run.run_generation == generation)
            .ok_or("selected Explain run identity changed or is unavailable")?;
        publish_recovered_explain(pool, engine, &run).await;
        record_artifact_fetch(ArtifactFetchPurpose::Discovery);
        artifact = store
            .load_json_artifact(user_id, session_id, &id)
            .await
            .map_err(|error| format!("load recovered Explain Analyze artifact: {error}"))?;
    }
    let artifact = artifact.ok_or("selected Explain Analyze capture is unavailable")?;
    if !matches!(
        args.get("depth").and_then(Value::as_str),
        Some("diagnostic" | "forensic")
    ) {
        return render_summary(&artifact, session_id, &run_id, generation, max_bytes);
    }
    render_window(
        &artifact,
        session_id,
        Some(&run_id),
        Some(generation),
        offset,
        max_bytes,
    )
}

fn render_summary(
    artifact: &StoredSessionArtifact,
    session_id: &str,
    run_id: &str,
    generation: u64,
    max_bytes: usize,
) -> Result<String, String> {
    let (status, _, graph) =
        validate_snapshot_payload_with_bytes(artifact, session_id, Some(run_id), Some(generation))?;
    if status == "unavailable" {
        return Err("the selected Explain Analyze capture is unavailable".into());
    }
    let auxiliary = graph.auxiliary_usage_snapshot();
    let auxiliary_attempts = graph.auxiliary_attempts();
    let scopes = graph.execution_scope_coverage();
    let mut turn_outcomes = std::collections::BTreeMap::<String, usize>::new();
    for node in graph
        .nodes()
        .iter()
        .filter(|node| node.kind == astra_turn_types::ExplainAnalyzeNodeKindV1::Turn)
    {
        let outcome = node
            .outcome
            .map(|outcome| {
                serde_json::to_value(outcome)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .unwrap_or_else(|| "unknown".into());
        *turn_outcomes.entry(outcome).or_default() += 1;
    }
    let mut diagnostics = std::collections::BTreeMap::<&str, usize>::new();
    for diagnostic in graph.diagnostics().iter() {
        *diagnostics.entry(diagnostic.code.as_str()).or_default() += 1;
    }
    let mut nodes = graph
        .nodes()
        .iter()
        .map(|node| {
            json!({
                "node_id": node.node_id, "parent_node_id": node.parent_node_id,
                "clock_domain_id": node.clock_domain_id, "kind": node.kind,
                "label": node.label, "start_elapsed_ms": node.start_elapsed_ms,
                "duration_ms": node.duration_ms, "outcome": node.outcome,
                "terminal_observed": node.terminal_observed, "conflicted": node.conflicted,
                "round_index": node.round_index, "attempt_index": node.attempt_index,
                "usage": node.usage,
                "context": node.context.as_ref().map(|context| json!({
                    "budget": context.budget,
                    "assembly": context.assembly.as_ref().map(|assembly| json!({
                        "basis": assembly.basis, "sources": assembly.sources,
                    })),
                })),
            })
        })
        .collect::<Vec<_>>();
    let mut summary = json!({
        "observation": "explain_analyze_summary", "run_id": run_id,
        "turn_id": artifact.content["turn_id"], "execution_owner_generation": generation,
        "artifact": artifact_handle(&artifact.artifact_id), "capture_status": status,
        "graph_integrity": match graph.integrity() {
            astra_turn_types::ExplainAnalyzeGraphIntegrityV1::Consistent => "consistent",
            astra_turn_types::ExplainAnalyzeGraphIntegrityV1::Unknown => "unknown",
        },
        "delivery_degraded": artifact.content["delivery_degraded"],
        "invalid_event_count": artifact.content["invalid_event_count"],
        "foreign_event_count": artifact.content["foreign_event_count"],
        "open_node_count": artifact.content["open_node_count"],
        "turn_outcome_counts": turn_outcomes,
        "coverage_gaps": graph.coverage_gaps(), "diagnostics": diagnostics,
        "auxiliary": {
            "available": auxiliary.available,
            "unavailable": graph.auxiliary_usage_unavailable(),
            "truncated": graph.auxiliary_usage_truncated(),
            "conflicted": graph.auxiliary_capture_conflicted(),
            "observed_attempt_count": auxiliary_attempts.len(),
            "shown_attempt_count": 0, "omitted_attempt_count": auxiliary_attempts.len(),
            "identity_conflict_count": graph.auxiliary_usage_conflict_count(),
            "scope_coverage": {
                "total_scope_count": scopes.len(),
                "missing_turn_count": scopes.iter().filter(|scope| !scope.turn_observed).count(),
                "nonterminal_turn_count": scopes.iter().filter(|scope| !scope.terminal_turn_observed).count(),
                "conflicted_turn_count": scopes.iter().filter(|scope| scope.turn_conflicted).count(),
                "snapshot_observed_count": scopes.iter().filter(|scope| scope.auxiliary_snapshot_observed).count(),
                "snapshot_missing_count": scopes.iter().filter(|scope| !scope.auxiliary_snapshot_observed).count(),
            },
        },
        "total_node_count": nodes.len(), "shown_node_count": 0, "omitted_node_count": nodes.len(),
        "nodes": [],
        "auxiliary_attempts": [],
        "note": "Bounded projected facts; null outcomes and measurements are unknown. Detail is available through the artifact handle at offset 0; this summary is not a pagination cursor. Child execution needs its own run evidence.",
    });
    // Reserve the mandatory envelope first, then select whole rows in one pass.
    // Keep factual identity/coverage even when the caller cannot afford any rows.
    let minimum = serde_json::to_string(&summary)
        .map_err(|error| format!("encode Explain summary: {error}"))?
        .len();
    if minimum > max_bytes {
        return Err(format!(
            "max_bytes is too small for an Explain summary (minimum {minimum})"
        ));
    }
    // Four count fields can grow by at most 20 decimal digits each on a
    // 64-bit target. Reserve that envelope growth independently of row bytes.
    let mut budget = max_bytes.saturating_sub(minimum + 80);
    let mut priority = (0..nodes.len()).collect::<Vec<_>>();
    priority.sort_by_key(|index| {
        let node = &graph.nodes()[*index];
        std::cmp::Reverse((
            node.conflicted
                || !node.terminal_observed
                || matches!(
                    node.outcome,
                    Some(
                        astra_turn_types::ExplainAnalyzeOutcomeV1::Failed
                            | astra_turn_types::ExplainAnalyzeOutcomeV1::Cancelled
                            | astra_turn_types::ExplainAnalyzeOutcomeV1::Blocked
                            | astra_turn_types::ExplainAnalyzeOutcomeV1::Interrupted
                    )
                ),
            matches!(
                node.kind,
                astra_turn_types::ExplainAnalyzeNodeKindV1::Run
                    | astra_turn_types::ExplainAnalyzeNodeKindV1::Turn
            ),
            matches!(
                node.kind,
                astra_turn_types::ExplainAnalyzeNodeKindV1::ToolCall
                    | astra_turn_types::ExplainAnalyzeNodeKindV1::ChildRun
                    | astra_turn_types::ExplainAnalyzeNodeKindV1::Wait
            ),
        ))
    });
    let mut retained = std::collections::HashSet::new();
    for index in priority {
        let size = serde_json::to_vec(&nodes[index])
            .map_err(|error| error.to_string())?
            .len()
            + 1;
        if size <= budget {
            retained.insert(index);
            budget -= size;
        }
    }
    nodes = nodes
        .into_iter()
        .enumerate()
        .filter_map(|(index, node)| retained.contains(&index).then_some(node))
        .collect();
    let mut attempts = Vec::new();
    for attempt in &auxiliary_attempts {
        let row = serde_json::to_value(attempt).map_err(|error| error.to_string())?;
        let size = serde_json::to_vec(&row)
            .map_err(|error| error.to_string())?
            .len()
            + 1;
        if size <= budget {
            attempts.push(row);
            budget -= size;
        }
    }
    summary["shown_node_count"] = json!(nodes.len());
    summary["omitted_node_count"] = json!(graph.nodes().len() - nodes.len());
    summary["nodes"] = json!(nodes);
    summary["auxiliary"]["shown_attempt_count"] = json!(attempts.len());
    summary["auxiliary"]["omitted_attempt_count"] =
        json!(auxiliary_attempts.len() - attempts.len());
    summary["auxiliary_attempts"] = json!(attempts);
    let output = serde_json::to_string(&summary)
        .map_err(|error| format!("encode Explain summary: {error}"))?;
    debug_assert!(output.len() <= max_bytes);
    Ok(output)
}

fn window_arguments(args: &Value) -> Result<(usize, usize), String> {
    let offset = match args.get("offset") {
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or("offset must be a non-negative integer")?,
        None => 0,
    };
    let max_bytes = match args.get("max_bytes") {
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| (1..=MAX_WINDOW_BYTES).contains(value))
            .ok_or_else(|| format!("max_bytes must be an integer from 1 to {MAX_WINDOW_BYTES}"))?,
        None => DEFAULT_WINDOW_BYTES,
    };
    Ok((offset, max_bytes))
}

fn render_window(
    artifact: &StoredSessionArtifact,
    session_id: &str,
    expected_run: Option<&str>,
    expected_generation: Option<u64>,
    offset: usize,
    max_bytes: usize,
) -> Result<String, String> {
    let (status, bytes, _) = validate_snapshot_payload_with_bytes(
        artifact,
        session_id,
        expected_run,
        expected_generation,
    )?;
    if status == "unavailable" {
        return Err("the selected Explain Analyze capture is unavailable".into());
    }
    let handle = artifact_handle(&artifact.artifact_id);
    let content = String::from_utf8(bytes)
        .map_err(|error| format!("Explain Analyze artifact is not valid UTF-8: {error}"))?;
    let (window, total_bytes, next_offset) = read_window(&content, offset, max_bytes)?;
    let continuation = if next_offset >= total_bytes {
        "Byte window complete; capture status is reported separately.".to_string()
    } else {
        format!(
            "Continue with introspect(artifact=\"{handle}\", offset={next_offset}, max_bytes={max_bytes})."
        )
    };
    let run = artifact.content["run_id"].as_str().unwrap_or_default();
    let turn = artifact.content["turn_id"].as_str().unwrap_or_default();
    let generation = &artifact.content["execution_owner_generation"];
    Ok(format!(
        "<explain-analyze-artifact>\nArtifact handle: {handle}\nRun: {run}\nTurn: {turn}\nGeneration: {generation}\nCapture status: {status}\nBytes: [{offset}..{next_offset}) of {total_bytes}\n\n{window}\n\n{continuation}\n</explain-analyze-artifact>"
    ))
}

/// Render exact durable run evidence when the run did not opt into the full
/// Explain Analyze capture.
///
/// `introspect(explain={target:"run"})` promises exact active-session
/// execution evidence, not only an Explain artifact. A normal run already
/// owns a bounded, server-authorized event projection; using it here avoids
/// turning every run into an artifact-writing run and avoids a second run
/// lookup. Full Explain runs continue through the immutable artifact path
/// above and retain pagination handles.
fn render_exact_run_projection(
    observation: &astra_services::runs::DurableRunObservation,
    args: &Value,
    max_bytes: usize,
) -> Result<String, String> {
    let run = &observation.run;
    let (events, omitted_events) = bounded_exact_run_events(observation)?;

    let projection = json!({
        "schema_version": 1,
        "observation": "exact_run_projection",
        "capture_status": "durable_event_projection",
        "explain_analyze_requested": astra_services::runs::run_requested_explain_analyze(run),
        "run": {
            "run_id": run.run_id,
            "session_id": run.session_id,
            "parent_run_id": run.parent_run_id,
            "root_run_id": run.root_run_id,
            "depth": run.depth,
            "agent_id": run.agent_id,
            "status": run.status,
            "waiting_for": run.waiting_for,
            "resolved_model_name": run.resolved_model_name,
            "total_prompt_tokens": run.total_prompt_tokens,
            "total_completion_tokens": run.total_completion_tokens,
            "total_tool_calls": run.total_tool_calls,
            "error_code": run.error_code,
            "error_message": run.error_message,
        },
        "events": events,
        "omitted_event_count": omitted_events,
        "observed_event_count": run.events.len(),
        "total_event_count": observation.total_event_count,
        "note": "This is the exact bounded durable run projection. A full Explain Analyze artifact is only created when the run requests Explain.",
    });

    let format_json = args
        .get("format")
        .and_then(Value::as_str)
        .is_some_and(|format| format == "json");
    if format_json {
        return render_bounded_exact_run_json(projection, max_bytes, 0);
    }

    let prefix = format!(
        "<exact-run-projection>\nRun: {}\nStatus: {}\n\n",
        run.run_id, run.status
    );
    let suffix = "\n\nBounded evidence without pagination; check omitted_event_count.\n</exact-run-projection>";
    let body = render_bounded_exact_run_json(projection, max_bytes, prefix.len() + suffix.len())?;
    Ok(format!("{prefix}{body}{suffix}"))
}

fn exact_run_event_priority(event: &Value) -> u8 {
    match astra_services::runs::extract_event_type(event).as_str() {
        "run_finished" | "run_error" | "run_cancelled" | "run_interrupted" | "run_paused"
        | "run_waiting" | "turn_complete" | "turn_done" => 2,
        "run_created" | "run_started" | "tool_call_start" | "tool_result" | "tool_call_end" => 1,
        _ => 0,
    }
}

fn bounded_exact_run_events(
    observation: &astra_services::runs::DurableRunObservation,
) -> Result<(Vec<Value>, u64), String> {
    const MAX_EVENTS: usize = 256;
    const MAX_EVENT_BYTES: usize = 48 * 1024;
    // One deliverable must not consume the whole execution-evidence budget.
    const MAX_SINGLE_EVENT_BYTES: usize = MAX_EVENT_BYTES / 4;

    let projected = observation
        .run
        .events
        .iter()
        .map(project_exact_run_observation_event)
        .filter(|event| !event.is_null())
        .collect::<Vec<_>>();
    let observed = projected.len();
    // Allocate the bounded evidence budget to final lifecycle events first,
    // then the newest ordinary events. The final list is sorted again so the
    // model sees a chronological projection without losing the run outcome
    // when an old run has a long stream of deltas.
    let mut priority = (0..observed).collect::<Vec<_>>();
    priority.sort_unstable_by_key(|index| {
        std::cmp::Reverse((exact_run_event_priority(&projected[*index]), *index))
    });

    let mut retained = Vec::new();
    let mut event_bytes = 0usize;
    for index in priority {
        if retained.len() >= MAX_EVENTS {
            break;
        }
        let bytes = serde_json::to_vec(&projected[index])
            .map_err(|error| format!("encode exact run event projection: {error}"))?;
        if bytes.len() > MAX_SINGLE_EVENT_BYTES
            || event_bytes.saturating_add(bytes.len()) > MAX_EVENT_BYTES
        {
            continue;
        }
        event_bytes += bytes.len();
        retained.push(index);
    }
    retained.sort_unstable();
    retained.dedup();
    let events = retained
        .into_iter()
        .map(|index| projected[index].clone())
        .collect::<Vec<_>>();
    let omitted = observation
        .total_event_count
        .saturating_sub(events.len() as u64);
    Ok((events, omitted))
}

fn project_exact_run_observation_event(event: &Value) -> Value {
    match astra_services::runs::extract_event_type(event).as_str() {
        "reasoning_message_content" | "thinking_delta" | "reasoning_delta" => return Value::Null,
        _ => {}
    }
    let projected = astra_services::runs::transform_run_event_for_client(event.clone());
    let call = match projected["type"].as_str() {
        Some("tool_call") => json!({
            "type": "tool_call_start",
            "call_id": projected.pointer("/tool_call/id"),
            "tool": projected.pointer("/tool_call/function/name"),
        }),
        Some("tool_call_start") => json!({
            "type": "tool_call_start",
            "call_id": projected["call_id"],
            "tool": projected["tool"],
        }),
        Some("tool_call_end") => {
            let mut receipt = projected.as_object().unwrap().clone();
            receipt.remove("arguments");
            receipt.remove("args");
            receipt.insert("arguments_omitted".into(), Value::Bool(true));
            return Value::Object(receipt);
        }
        _ => return projected,
    };
    let mut call = call.as_object().unwrap().clone();
    for field in [
        "run_id",
        "producer_run_id",
        "parent_run_id",
        "parent_tool_use_id",
        "turn",
        "round",
    ] {
        if let Some(value) = projected.get(field) {
            call.insert(field.into(), value.clone());
        }
    }
    call.insert("arguments_omitted".into(), Value::Bool(true));
    Value::Object(call)
}

fn render_bounded_exact_run_json(
    mut projection: Value,
    max_bytes: usize,
    envelope_bytes: usize,
) -> Result<String, String> {
    loop {
        let encoded = serde_json::to_string(&projection)
            .map_err(|error| format!("encode exact run projection: {error}"))?;
        if encoded.len().saturating_add(envelope_bytes) <= max_bytes {
            return Ok(encoded);
        }
        let Some(events) = projection["events"].as_array_mut() else {
            break;
        };
        if events.is_empty() {
            break;
        }
        let remove_at = events
            .iter()
            .enumerate()
            .min_by_key(|(_, event)| exact_run_event_priority(event))
            .map(|(index, _)| index)
            .unwrap_or(0);
        events.remove(remove_at);
        projection["omitted_event_count"] = projection["omitted_event_count"]
            .as_u64()
            .unwrap_or_default()
            .saturating_add(1)
            .into();
    }

    // A tiny caller window must still receive valid JSON. Keep the identity
    // and terminal status, but do not return an oversized or sliced object.
    let compact = json!({
        "observation": projection["observation"],
        "capture_status": projection["capture_status"],
        "run": {
            "run_id": projection["run"]["run_id"],
            "status": projection["run"]["status"],
        },
        "events": [],
        "omitted_event_count": projection["omitted_event_count"],
        "note": "event window omitted; increase max_bytes",
    });
    let encoded = serde_json::to_string(&compact)
        .map_err(|error| format!("encode compact exact run projection: {error}"))?;
    let minimum_bytes = encoded.len().saturating_add(envelope_bytes);
    if minimum_bytes > max_bytes {
        return Err(format!(
            "max_bytes is too small for a valid exact run projection (minimum {})",
            minimum_bytes
        ));
    }
    Ok(encoded)
}

fn read_window(
    content: &str,
    offset: usize,
    max_bytes: usize,
) -> Result<(String, usize, usize), String> {
    let total_bytes = content.len();
    if total_bytes > MAX_ARTIFACT_BYTES {
        return Err("Explain Analyze artifact exceeds the read bound".to_string());
    }
    if offset > total_bytes {
        return Err("offset is past the end of the Explain Analyze artifact".to_string());
    }
    if offset < total_bytes && !content.is_char_boundary(offset) {
        return Err("offset must be a UTF-8 boundary".to_string());
    }
    let available = total_bytes.saturating_sub(offset);
    let mut end = (offset + available.min(max_bytes)).min(total_bytes);
    while end > offset && !content.is_char_boundary(end) {
        end -= 1;
    }
    if end == offset && available > 0 {
        return Err(
            "max_bytes is too small to advance one UTF-8 character; increase max_bytes".to_string(),
        );
    }
    Ok((content[offset..end].to_string(), total_bytes, end))
}

/// Resolve only server-owned Explain handles. `None` means the handle belongs
/// to another artifact reader (for example a tool-result or edge-local copy).
pub(crate) async fn resolve_request(
    store: Option<&dyn SessionArtifactJsonStore>,
    user_id: &str,
    session_id: &str,
    args: &Value,
) -> Option<Result<String, String>> {
    let handle = args.get("artifact")?.as_str().map(str::to_owned);
    let Some(handle) = handle else {
        return Some(Err("artifact must be a string handle".to_string()));
    };
    let artifact_id = artifact_id_from_handle(&handle).map(str::to_owned)?;
    let Some(store) = store else {
        return Some(Err(
            "server Explain Analyze artifact reader is unavailable".to_string()
        ));
    };
    Some(
        async move {
            let (offset, max_bytes) = window_arguments(args)?;
            let artifact = store
                .load_json_artifact(user_id, session_id, &artifact_id)
                .await
                .map_err(|error| format!("load Explain Analyze artifact: {error}"))?
                .ok_or_else(|| {
                    "Explain Analyze artifact was not found for this session".to_string()
                })?;
            render_window(&artifact, session_id, None, None, offset, max_bytes)
        }
        .await,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MemoryStore {
        artifacts: Mutex<HashMap<(String, String, String), StoredSessionArtifact>>,
    }

    fn stored(record: SessionArtifactJsonRecord) -> StoredSessionArtifact {
        StoredSessionArtifact {
            artifact_id: record.artifact_id,
            session_id: record.session_id,
            user_id: record.user_id,
            artifact_kind: record.artifact_kind,
            source: record.source,
            turn: record.turn,
            round: record.round,
            content: record.content,
            metadata: record.metadata,
            retention_policy: Some("default".to_string()),
            retention_until: None,
            status: Some("active".to_string()),
            referenced_by_manifest_count: 0,
            referenced_by_state_items_count: 0,
            referenced_by_citation_count: 0,
            referenced_by_durable_count: 0,
            created_at: None,
        }
    }

    #[async_trait]
    impl SessionArtifactJsonStore for MemoryStore {
        async fn persist_json_artifact(
            &self,
            record: SessionArtifactJsonRecord,
        ) -> Result<StoredSessionArtifact, astra_services::SessionArtifactStoreError> {
            let artifact = stored(record);
            self.artifacts.lock().unwrap().insert(
                (
                    artifact.user_id.clone(),
                    artifact.session_id.clone(),
                    artifact.artifact_id.clone(),
                ),
                artifact.clone(),
            );
            Ok(artifact)
        }

        async fn upsert_json_artifact_projection(
            &self,
            record: SessionArtifactJsonRecord,
        ) -> Result<StoredSessionArtifact, astra_services::SessionArtifactStoreError> {
            self.persist_json_artifact(record).await
        }

        async fn load_json_artifact(
            &self,
            user_id: &str,
            session_id: &str,
            artifact_id: &str,
        ) -> Result<Option<StoredSessionArtifact>, astra_services::SessionArtifactStoreError>
        {
            Ok(self
                .artifacts
                .lock()
                .unwrap()
                .get(&(
                    user_id.to_string(),
                    session_id.to_string(),
                    artifact_id.to_string(),
                ))
                .cloned())
        }

        async fn load_latest_json_artifact(
            &self,
            user_id: &str,
            session_id: &str,
            artifact_kind: &str,
        ) -> Result<Option<StoredSessionArtifact>, astra_services::SessionArtifactStoreError>
        {
            Ok(self
                .artifacts
                .lock()
                .unwrap()
                .values()
                .find(|artifact| {
                    artifact.user_id == user_id
                        && artifact.session_id == session_id
                        && artifact.artifact_kind == artifact_kind
                })
                .cloned())
        }

        async fn list_json_artifacts(
            &self,
            _user_id: &str,
            _session_id: &str,
            _artifact_kind: Option<&str>,
            _limit: usize,
            _cursor: Option<SessionArtifactListCursor>,
        ) -> Result<SessionArtifactListPage, astra_services::SessionArtifactStoreError> {
            Ok(SessionArtifactListPage {
                artifacts: Vec::new(),
                limit: 0,
                next_cursor: None,
            })
        }
    }

    fn explain_record(
        artifact_id: &str,
        user_id: &str,
        session_id: &str,
        status: &str,
    ) -> SessionArtifactJsonRecord {
        let events = if status == "unavailable" {
            json!([])
        } else {
            json!([{
                "type": "explain_analyze",
                "schema_version": 1,
                "event_id": "event-1",
                "run_id": "run-1",
                "turn_id": "turn-1",
                "node_id": "turn-1",
                "producer_id": "server",
                "clock_domain_id": "clock-1",
                "kind": "turn",
                "label": "User turn",
                "transition": "finished",
                "elapsed_ms": 1,
                "start_elapsed_ms": 0,
                "duration_ms": 1,
                "outcome": "completed"
            }])
        };
        let content = json!({
            "artifact_schema_version": ARTIFACT_SCHEMA_VERSION,
            "artifact_kind": "explain_analyze",
            "artifact_type": ARTIFACT_TYPE,
            "content_type": CONTENT_TYPE,
            "storage": STORAGE,
            "representation": REPRESENTATION,
            "schema_version": astra_turn_types::EXPLAIN_ANALYZE_SCHEMA_VERSION,
            "session_id": session_id,
            "run_id": "run-1",
            "turn_id": "turn-1",
            "execution_owner_generation": 1,
            "capture_status": status,
            "delivery_degraded": false,
            "invalid_event_count": 0,
            "foreign_event_count": 0,
            "open_node_count": 0,
            "graph_diagnostics": [],
            "coverage_gaps": [],
            "events": events,
        });
        let content_bytes = serde_json::to_vec_pretty(&content).unwrap();
        SessionArtifactJsonRecord {
            artifact_id: artifact_id.to_string(),
            session_id: session_id.to_string(),
            user_id: user_id.to_string(),
            artifact_kind: ARTIFACT_KIND.to_string(),
            source: Some("server_runtime".to_string()),
            turn: Some(1),
            round: None,
            content,
            metadata: Some(json!({
                "artifact_schema_version": ARTIFACT_SCHEMA_VERSION,
                "artifact_type": ARTIFACT_TYPE,
                "content_type": CONTENT_TYPE,
                "storage": STORAGE,
                "representation": REPRESENTATION,
                "size_bytes": content_bytes.len(),
                "checksum_sha256": format!("{:x}", Sha256::digest(&content_bytes)),
                "status": status,
                "execution_owner_generation": 1,
            })),
            references: Vec::new(),
        }
    }

    #[test]
    fn summary_preserves_unknown_capture_and_whole_evidence_with_tiny_budget() {
        let mut artifact = stored(explain_record(
            "summary-test",
            "user-a",
            "session-a",
            "complete",
        ));
        let terminal = artifact.content["events"][0].clone();
        for index in 0..300 {
            let mut event = terminal.clone();
            event["event_id"] = json!(format!("stage-event-{index}"));
            event["node_id"] = json!(format!("stage-{index}"));
            event["parent_node_id"] = json!("turn-1");
            event["kind"] = json!("preparation");
            event["label"] = json!("上下文准备".repeat(10));
            artifact.content["events"]
                .as_array_mut()
                .unwrap()
                .push(event);
        }
        refresh_integrity_metadata(&mut artifact);
        assert!(serde_json::to_vec(&artifact.content).unwrap().len() > 32768);
        let output = render_summary(&artifact, "session-a", "run-1", 1, 8192).unwrap();
        assert!(output.len() <= 8192);
        let summary: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(summary["total_node_count"], 301);
        assert_eq!(
            summary["shown_node_count"].as_u64().unwrap()
                + summary["omitted_node_count"].as_u64().unwrap(),
            301
        );
        assert!(summary["omitted_node_count"].as_u64().unwrap() > 0);
        assert_eq!(summary["nodes"][0]["node_id"], "turn-1");
        assert_eq!(summary["turn_outcome_counts"]["completed"], 1);
        assert_eq!(summary["auxiliary"]["available"], false);
        assert_eq!(
            summary["auxiliary"]["scope_coverage"]["snapshot_missing_count"],
            1
        );
        let error = render_summary(&artifact, "session-a", "run-1", 1, 1).unwrap_err();
        let minimum: usize = error
            .split("minimum ")
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .parse()
            .unwrap();
        assert!(render_summary(&artifact, "session-a", "run-1", 1, minimum - 1).is_err());
        let minimum_output = render_summary(&artifact, "session-a", "run-1", 1, minimum).unwrap();
        assert_eq!(minimum_output.len(), minimum);
        let minimum_summary: Value = serde_json::from_str(&minimum_output).unwrap();
        assert_eq!(minimum_summary["shown_node_count"], 0);
        assert_eq!(
            minimum_summary["turn_outcome_counts"],
            summary["turn_outcome_counts"]
        );
        assert_eq!(minimum_summary["auxiliary"], summary["auxiliary"]);
        assert_eq!(minimum_summary["diagnostics"], summary["diagnostics"]);
        assert_eq!(minimum_summary["artifact"], artifact_handle("summary-test"));
        artifact.content["events"][0]["auxiliary_usage"] =
            json!({"available": true, "attempts": []});
        refresh_integrity_metadata(&mut artifact);
        let captured: Value = serde_json::from_str(
            &render_summary(&artifact, "session-a", "run-1", 1, 8192).unwrap(),
        )
        .unwrap();
        assert_eq!(captured["auxiliary"]["available"], true);
        assert_eq!(
            captured["auxiliary"]["scope_coverage"]["snapshot_observed_count"],
            1
        );
        for event in artifact.content["events"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .skip(1)
        {
            event["kind"] = json!("tool_call");
        }
        artifact.content["events"][299]["kind"] = json!("child_run");
        artifact.content["events"][299]["outcome"] = json!("failed");
        artifact.content["events"][300]["kind"] = json!("wait");
        artifact.content["events"][300]["transition"] = json!("started");
        for field in ["outcome", "duration_ms", "start_elapsed_ms"] {
            artifact.content["events"][300]
                .as_object_mut()
                .unwrap()
                .remove(field);
        }
        artifact.content["capture_status"] = json!("partial");
        artifact.metadata.as_mut().unwrap()["status"] = json!("partial");
        let facts = artifact.content["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| astra_turn_types::decode_explain_analyze_wire(event).unwrap())
            .collect::<Vec<_>>();
        let projection = graph_projection(&facts);
        artifact.content["open_node_count"] = json!(projection.open_node_count);
        artifact.content["graph_diagnostics"] = json!(projection.graph_diagnostics);
        artifact.content["coverage_gaps"] = json!(projection.coverage_gaps);
        refresh_integrity_metadata(&mut artifact);
        let partial: Value = serde_json::from_str(
            &render_summary(&artifact, "session-a", "run-1", 1, 8192).unwrap(),
        )
        .unwrap();
        assert_eq!(partial["capture_status"], "partial");
        assert_eq!(partial["open_node_count"], 1);
        assert!(
            partial["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|node| node["node_id"] == "stage-298" && node["outcome"] == "failed")
        );
        assert!(
            partial["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|node| node["node_id"] == "stage-299" && node["outcome"].is_null())
        );
        artifact.artifact_kind = "unrelated".into();
        assert!(
            render_summary(&artifact, "session-a", "run-1", 1, 8192)
                .unwrap_err()
                .contains("does not name")
        );
    }

    #[test]
    fn summary_bounds_scope_cardinality_and_keeps_nonconflicting_auxiliary_attempts() {
        let mut artifact = stored(explain_record(
            "scope-test",
            "user-a",
            "session-a",
            "partial",
        ));
        let template = artifact.content["events"][0].clone();
        let attempt = json!({
            "attempt_id": "conflicting", "usage_status": "provider_exact",
            "provider": "test", "offering_id": "offering-1", "model_name": "test-model",
            "purpose": "introspection", "operation_id": "request_judgment",
            "usage": {"basis": "provider_exact", "fresh_input_tokens": 10},
        });
        let mut valid = attempt.clone();
        valid["attempt_id"] = json!("nonconflicting");
        artifact.content["events"][0]["auxiliary_usage"] =
            json!({"available": true, "attempts": [attempt.clone(), valid]});
        for index in 0..1000 {
            let mut event = template.clone();
            event["event_id"] = json!(format!("scope-event-{index}"));
            event["node_id"] = json!(format!("scope-turn-{index}"));
            event["clock_domain_id"] = json!(format!("scope-clock-{index}"));
            if index == 0 {
                let mut conflicting = attempt.clone();
                conflicting["usage"]["fresh_input_tokens"] = json!(20);
                event["auxiliary_usage"] =
                    json!({"available": true, "truncated": true, "attempts": [conflicting]});
            }
            artifact.content["events"]
                .as_array_mut()
                .unwrap()
                .push(event);
        }
        let facts = artifact.content["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| astra_turn_types::decode_explain_analyze_wire(event).unwrap())
            .collect::<Vec<_>>();
        let projection = graph_projection(&facts);
        artifact.content["open_node_count"] = json!(projection.open_node_count);
        artifact.content["graph_diagnostics"] = json!(projection.graph_diagnostics);
        artifact.content["coverage_gaps"] = json!(projection.coverage_gaps);
        refresh_integrity_metadata(&mut artifact);
        let output = render_summary(&artifact, "session-a", "run-1", 1, 8192).unwrap();
        assert!(output.len() <= 8192);
        let summary: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(
            summary["auxiliary"]["scope_coverage"]["total_scope_count"],
            1001
        );
        assert_eq!(
            summary["auxiliary"]["scope_coverage"]["snapshot_missing_count"],
            999
        );
        assert_eq!(summary["auxiliary"]["identity_conflict_count"], 1);
        assert_eq!(summary["auxiliary"]["observed_attempt_count"], 1);
        assert_eq!(summary["auxiliary"]["available"], false);
        assert_eq!(summary["auxiliary"]["truncated"], true);
        assert_eq!(summary["turn_outcome_counts"]["completed"], 1001);
        assert_eq!(
            summary["auxiliary"]["shown_attempt_count"]
                .as_u64()
                .unwrap()
                + summary["auxiliary"]["omitted_attempt_count"]
                    .as_u64()
                    .unwrap(),
            1
        );
        let error = render_summary(&artifact, "session-a", "run-1", 1, 1).unwrap_err();
        let minimum: usize = error
            .split("minimum ")
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .parse()
            .unwrap();
        let minimal: Value = serde_json::from_str(
            &render_summary(&artifact, "session-a", "run-1", 1, minimum).unwrap(),
        )
        .unwrap();
        assert_eq!(minimal["nodes"], json!([]));
        assert_eq!(minimal["auxiliary_attempts"], json!([]));
        assert_eq!(minimal["auxiliary"]["shown_attempt_count"], 0);
        assert_eq!(minimal["auxiliary"]["omitted_attempt_count"], 1);
        for field in [
            "available",
            "truncated",
            "identity_conflict_count",
            "observed_attempt_count",
            "scope_coverage",
        ] {
            assert_eq!(minimal["auxiliary"][field], summary["auxiliary"][field]);
        }
        assert_eq!(minimal["artifact"], summary["artifact"]);
    }

    async fn handler_fixture() -> (
        crate::server::runtime_tool_executor::RuntimeToolExecutor,
        Arc<MemoryStore>,
        RunEngine,
    ) {
        let engine = RunEngine::new(Arc::new(astra_services::runs::InMemoryRunStateStore::new()));
        for run in ["run-1", "current-root"] {
            engine.start_run(run, "user-a", "session-a").await.unwrap();
            engine
                .append_event(
                    "user-a",
                    "session-a",
                    run,
                    json!({
                        "event_type": "run_started", "data": {"explain_analyze_requested": true}
                    }),
                )
                .await
                .unwrap();
            if run == "run-1" {
                engine
                    .persist_status("user-a", "session-a", run, "completed", None, None)
                    .await
                    .unwrap();
            }
        }
        let store = Arc::new(MemoryStore::default());
        let mut artifact = stored(explain_record(
            &artifact_id("run-1"),
            "user-a",
            "session-a",
            "complete",
        ));
        let generation = engine
            .load_run("user-a", "run-1")
            .await
            .unwrap()
            .unwrap()
            .run_generation;
        artifact.content["execution_owner_generation"] = json!(generation);
        artifact.metadata.as_mut().unwrap()["execution_owner_generation"] = json!(generation);
        refresh_integrity_metadata(&mut artifact);
        store.artifacts.lock().unwrap().insert(
            (
                "user-a".into(),
                "session-a".into(),
                artifact.artifact_id.clone(),
            ),
            artifact,
        );
        let executor = crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
            std::env::temp_dir(),
            "user-a".into(),
            "session-a".into(),
            None,
            None,
        )
        .with_explain_root(engine.clone(), "current-root".into())
        .with_test_session_artifact_store(store.clone());
        (executor, store, engine)
    }

    #[tokio::test]
    async fn lazy_handler_discovers_previous_without_prompt_handle_and_paginates() {
        let (executor, store, engine) = handler_fixture().await;
        let summary = executor
            .execute_with_metadata("introspect", &json!({"explain": {"target": "previous"}}))
            .await;
        assert!(!summary.is_error, "{summary:?}");
        let summary: Value = serde_json::from_str(&summary.output).unwrap();
        assert_eq!(summary["observation"], "explain_analyze_summary");
        assert_eq!(summary["run_id"], "run-1");
        assert_eq!(summary["shown_node_count"], 1);
        assert_eq!(summary["nodes"][0]["outcome"], "completed");
        assert_eq!(summary["auxiliary"]["available"], false);
        let (first, fetches) = count_explain_artifact_fetches(executor.execute_with_metadata(
            "introspect",
            &json!({"explain": {"target": "previous"}, "depth": "diagnostic", "max_bytes": 128}),
        ))
        .await;
        assert!(!first.is_error, "{first:?}");
        let handle = artifact_handle(&artifact_id("run-1"));
        assert!(first.output.contains(&handle), "{first:?}");
        assert!(first.output.contains("Run: run-1"));
        assert!(!first.output.contains("current-root"));
        assert_eq!(fetches.total, 1);
        // Advance durable selection after page one; the returned handle remains fixed.
        engine
            .persist_status(
                "user-a",
                "session-a",
                "current-root",
                "completed",
                None,
                None,
            )
            .await
            .unwrap();
        engine
            .start_run("new-root", "user-a", "session-a")
            .await
            .unwrap();
        engine
            .append_event(
                "user-a",
                "session-a",
                "new-root",
                json!({
                    "event_type": "run_started", "data": {"explain_analyze_requested": true}
                }),
            )
            .await
            .unwrap();
        let page = executor
            .execute_with_metadata(
                "introspect",
                &json!({
                    "artifact": handle, "offset": 128, "max_bytes": 65536
                }),
            )
            .await;
        assert!(!page.is_error, "{page:?}");
        assert!(page.output.contains("Run: run-1"));
        assert!(page.output.contains("Byte window complete"));
        for (user, session) in [("other-user", "session-a"), ("user-a", "other-session")] {
            let foreign = crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
                std::env::temp_dir(),
                user.into(),
                session.into(),
                None,
                None,
            )
            .with_explain_root(engine.clone(), "current-root".into())
            .with_test_session_artifact_store(store.clone());
            for args in [
                json!({"explain": {"target": "run", "run_id": "run-1"}}),
                json!({"artifact": handle}),
            ] {
                let denied = foreign.execute_with_metadata("introspect", &args).await;
                assert!(denied.is_error, "{denied:?}");
            }
        }
    }

    #[tokio::test]
    async fn exact_run_returns_durable_projection_without_explain_artifact() {
        let (executor, store, engine) = handler_fixture().await;
        engine
            .persist_status(
                "user-a",
                "session-a",
                "current-root",
                "completed",
                None,
                None,
            )
            .await
            .unwrap();
        engine
            .start_run("plain-run", "user-a", "session-a")
            .await
            .unwrap();
        engine
            .append_event(
                "user-a",
                "session-a",
                "plain-run",
                json!({
                    "event_type": "tool_call_start",
                    "data": {
                        "name": "bash",
                        "tool_call_id": "call-1",
                        "args": {"command": "private-argument-marker"}
                    }
                }),
            )
            .await
            .unwrap();
        engine
            .append_event(
                "user-a",
                "session-a",
                "plain-run",
                json!({
                    "event_type": "tool_result",
                    "data": {
                        "name": "bash",
                        "tool_call_id": "call-1",
                        "output": "42",
                        "success": true
                    }
                }),
            )
            .await
            .unwrap();
        for index in 0..300 {
            engine
                .append_event(
                    "user-a",
                    "session-a",
                    "plain-run",
                    json!({
                        "event_type": "agent_progress",
                        "data": {"index": index}
                    }),
                )
                .await
                .unwrap();
        }
        for event_type in [
            "reasoning_message_content",
            "thinking_delta",
            "reasoning_delta",
        ] {
            engine.append_event("user-a", "session-a", "plain-run", json!({
                "event_type": event_type,
                "data": {"content": "private-reasoning-marker", "chunk": "private-reasoning-marker"}
            })).await.unwrap();
        }
        engine.append_event("user-a", "session-a", "plain-run", json!({
            "type": "tool_call",
            "tool_call": {"id":"call-modern", "type":"function", "function": {
                "name":"read_file", "arguments":"{\"path\":\"private-modern-argument-marker\"}"
            }}
        })).await.unwrap();
        engine
            .append_event(
                "user-a",
                "session-a",
                "plain-run",
                json!({
                    "type":"tool_call_end", "call_id":"call-terminal", "tool":"read_file",
                    "arguments":{"path":"private-terminal-argument-marker"},
                    "result":"public-result", "status":"completed", "success":true,
                    "executed":true, "disposition":"executed"
                }),
            )
            .await
            .unwrap();
        engine.append_event("user-a", "session-a", "plain-run", json!({
            "event_type": "tool_result",
            "data": {"name": "large_output", "tool_call_id": "large-call", "output": "x".repeat(48 * 1024 - 128), "success": true}
        })).await.unwrap();
        engine
            .append_event(
                "user-a",
                "session-a",
                "plain-run",
                json!({
                    "event_type": "run_finished",
                    "data": {"run_id": "plain-run", "status": "completed"}
                }),
            )
            .await
            .unwrap();
        engine
            .persist_status("user-a", "session-a", "plain-run", "completed", None, None)
            .await
            .unwrap();

        let output = executor
            .execute_with_metadata(
                "introspect",
                &json!({
                    "explain": {"target": "run", "run_id": "plain-run"},
                    "format": "json"
                }),
            )
            .await;
        assert!(!output.is_error, "{output:?}");
        let projection: Value = serde_json::from_str(&output.output).unwrap();
        assert_eq!(projection["observation"], "exact_run_projection");
        assert_eq!(projection["capture_status"], "durable_event_projection");
        assert_eq!(projection["run"]["run_id"], "plain-run");
        assert_eq!(projection["run"]["status"], "completed");
        assert!(output.output.contains("bash"));
        assert!(output.output.contains("42"));
        assert!(output.output.contains("run_finished"));
        assert!(!output.output.contains("private-reasoning-marker"));
        assert!(!output.output.contains("private-argument-marker"));
        assert!(!output.output.contains("private-modern-argument-marker"));
        assert!(!output.output.contains("private-terminal-argument-marker"));
        assert!(
            projection["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| {
                    event["call_id"] == "call-terminal"
                        && event["tool"] == "read_file"
                        && event["result"] == "public-result"
                        && event["success"] == true
                        && event["executed"] == true
                        && event["disposition"] == "executed"
                        && event["arguments_omitted"] == true
                })
        );
        assert!(
            projection["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| {
                    event["call_id"] == "call-1"
                        && event["tool"] == "bash"
                        && event["arguments_omitted"] == true
                })
        );
        assert!(
            projection["omitted_event_count"]
                .as_u64()
                .unwrap_or_default()
                > 0
        );

        let compact = executor
            .execute_with_metadata(
                "introspect",
                &json!({
                    "explain": {"target": "run", "run_id": "plain-run"},
                    "format": "json",
                    "max_bytes": 512
                }),
            )
            .await;
        assert!(!compact.is_error, "{compact:?}");
        assert!(
            compact.output.len() <= 512,
            "{} bytes",
            compact.output.len()
        );
        let compact_projection: Value = serde_json::from_str(&compact.output).unwrap();
        assert_eq!(compact_projection["run"]["run_id"], "plain-run");
        let default_text = executor
            .execute_with_metadata(
                "introspect",
                &json!({"explain": {"target": "run", "run_id": "plain-run"}}),
            )
            .await;
        assert!(!default_text.is_error, "{default_text:?}");
        let default_projection: Value =
            serde_json::from_str(default_text.output.split("\n\n").nth(1).unwrap()).unwrap();
        assert!(default_text.output.len() <= DEFAULT_WINDOW_BYTES);
        assert!(default_text.output.contains("run_finished"));
        assert_eq!(default_projection["run"]["status"], "completed");
        assert_eq!(
            default_projection["events"].as_array().unwrap().len() as u64
                + default_projection["omitted_event_count"].as_u64().unwrap(),
            default_projection["total_event_count"].as_u64().unwrap()
        );
        let text = executor
            .execute_with_metadata(
                "introspect",
                &json!({
                    "explain": {"target": "run", "run_id": "plain-run"},
                    "max_bytes": 512
                }),
            )
            .await;
        assert!(!text.is_error, "{text:?}");
        let body = text.output.split("\n\n").nth(1).unwrap();
        let text_projection: Value = serde_json::from_str(body).unwrap();
        assert_eq!(text_projection["run"], compact_projection["run"]);
        assert!(text.output.len() <= 512);
        assert!(text.output.contains("check omitted_event_count"));
        let too_small = executor
            .execute_with_metadata(
                "introspect",
                &json!({"explain": {"target": "run", "run_id": "plain-run"}, "max_bytes": 1}),
            )
            .await;
        assert!(too_small.is_error);
        let minimum: usize = too_small
            .output
            .split("minimum ")
            .nth(1)
            .unwrap()
            .split(')')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let retry = executor
            .execute_with_metadata(
                "introspect",
                &json!({"explain": {"target": "run", "run_id": "plain-run"}, "max_bytes": minimum}),
            )
            .await;
        assert!(!retry.is_error, "{retry:?}");
        assert_eq!(retry.output.len(), minimum);
        assert!(
            store
                .artifacts
                .lock()
                .unwrap()
                .get(&(
                    "user-a".into(),
                    "session-a".into(),
                    artifact_id("plain-run")
                ))
                .is_none()
        );

        let foreign = crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
            std::env::temp_dir(),
            "other-user".into(),
            "session-a".into(),
            None,
            None,
        )
        .with_explain_root(engine, "current-root".into())
        .with_test_session_artifact_store(store);
        let denied = foreign
            .execute_with_metadata(
                "introspect",
                &json!({"explain": {"target": "run", "run_id": "plain-run"}}),
            )
            .await;
        assert!(denied.is_error, "{denied:?}");
    }

    #[tokio::test]
    async fn lazy_handler_fails_closed_for_newer_unreadable_capture() {
        for failure in [
            "missing",
            "cancelled",
            "corrupt",
            "expired",
            "generation",
            "unavailable",
        ] {
            let (executor, store, engine) = handler_fixture().await;
            engine
                .persist_status(
                    "user-a",
                    "session-a",
                    "current-root",
                    "completed",
                    None,
                    None,
                )
                .await
                .unwrap();
            engine
                .start_run("new-root", "user-a", "session-a")
                .await
                .unwrap();
            engine
                .append_event(
                    "user-a",
                    "session-a",
                    "new-root",
                    json!({
                        "event_type": "run_started", "data": {"explain_analyze_requested": true}
                    }),
                )
                .await
                .unwrap();
            if failure == "cancelled" {
                let run = engine
                    .load_run("user-a", "new-root")
                    .await
                    .unwrap()
                    .unwrap();
                engine
                    .cancel_if_exact_live_owner(
                        "user-a",
                        "session-a",
                        "new-root",
                        run.run_generation,
                        &["running"],
                        astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
                        "Explain fixture cancellation",
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    engine
                        .load_run("user-a", "new-root")
                        .await
                        .unwrap()
                        .unwrap()
                        .status,
                    "cancelled"
                );
            } else {
                engine
                    .persist_status("user-a", "session-a", "new-root", "paused", None, None)
                    .await
                    .unwrap();
            }
            if !matches!(failure, "missing" | "cancelled") {
                let mut artifact = stored(explain_record(
                    &artifact_id("new-root"),
                    "user-a",
                    "session-a",
                    if failure == "unavailable" {
                        "unavailable"
                    } else {
                        "complete"
                    },
                ));
                artifact.content["run_id"] = json!("new-root");
                if let Some(events) = artifact.content["events"].as_array_mut() {
                    for event in events {
                        event["run_id"] = json!("new-root");
                    }
                }
                let generation = engine
                    .load_run("user-a", "new-root")
                    .await
                    .unwrap()
                    .unwrap()
                    .run_generation;
                artifact.content["execution_owner_generation"] = json!(generation);
                artifact.metadata.as_mut().unwrap()["execution_owner_generation"] =
                    json!(generation);
                match failure {
                    "corrupt" => artifact.content = json!({}),
                    "expired" => artifact.status = Some("expired".into()),
                    "generation" => {
                        artifact.content["execution_owner_generation"] = json!(generation + 1);
                        artifact.metadata.as_mut().unwrap()["execution_owner_generation"] =
                            json!(generation + 1);
                    }
                    _ => {}
                }
                refresh_integrity_metadata(&mut artifact);
                store.artifacts.lock().unwrap().insert(
                    (
                        "user-a".into(),
                        "session-a".into(),
                        artifact.artifact_id.clone(),
                    ),
                    artifact,
                );
            }
            let (result, fetches) =
                count_explain_artifact_fetches(executor.execute_with_metadata(
                    "introspect",
                    &json!({"explain": {"target": "previous"}}),
                ))
                .await;
            assert!(result.is_error, "{failure}: {result:?}");
            assert!(
                !result
                    .output
                    .contains(&artifact_handle(&artifact_id("run-1")))
            );
            assert_eq!(fetches.recovery, 0, "{failure}");
            assert_eq!(
                fetches.discovery,
                if matches!(failure, "missing" | "cancelled") {
                    2
                } else {
                    1
                }
            );
        }
    }

    #[tokio::test]
    async fn lazy_handler_rejects_ambiguous_selectors_before_discovery() {
        let (executor, _, _) = handler_fixture().await;
        for args in [
            json!({"explain": {"target": "previous"}, "artifact": "artifact://session/explain-analyze/x"}),
            json!({"explain": {"target": "run"}}),
            json!({"explain": {"target": "previous", "run_id": "run-1"}}),
            json!({"explain": {"target": "previous"}, "offset": 1}),
            json!({"explain": {"target": "previous"}, "max_bytes": 65537}),
            json!({"explain": {"target": "previous"}, "source_policy": "local_only"}),
        ] {
            let (result, fetches) =
                count_explain_artifact_fetches(executor.execute_with_metadata("introspect", &args))
                    .await;
            assert!(result.is_error, "{result:?}");
            assert_eq!(fetches.total, 0);
        }
    }

    #[tokio::test]
    async fn partial_capture_stays_partial_across_utf8_windows() {
        let (executor, store, _) = handler_fixture().await;
        let id = artifact_id("run-1");
        {
            let mut artifacts = store.artifacts.lock().unwrap();
            let artifact = artifacts
                .get_mut(&("user-a".into(), "session-a".into(), id.clone()))
                .unwrap();
            artifact.content["capture_status"] = json!("partial");
            artifact.content["delivery_degraded"] = json!(true);
            artifact.content["events"][0]["label"] = json!("观测");
            artifact.metadata.as_mut().unwrap()["status"] = json!("partial");
            refresh_integrity_metadata(artifact);
        }
        let first = executor
            .execute(
                "introspect",
                &json!({
                    "explain": {"target": "previous"}, "depth": "forensic", "max_bytes": 128
                }),
            )
            .await;
        assert!(first.contains("Capture status: partial"), "{first}");
        let last = executor
            .execute(
                "introspect",
                &json!({
                    "artifact": artifact_handle(&id), "offset": 128, "max_bytes": 65536
                }),
            )
            .await;
        assert!(last.contains("Capture status: partial"), "{last}");
        assert!(last.contains("Byte window complete"), "{last}");
        assert!(
            !last.contains("total_tokens"),
            "unknown usage must not become zero"
        );
        assert_eq!(read_window("a观测z", 1, 4).unwrap(), ("观".into(), 8, 4));
        assert!(read_window("a观测z", 2, 4).is_err());
        assert!(read_window("观", 0, 2).is_err());
    }

    #[tokio::test]
    async fn reader_is_server_owned_and_session_scoped() {
        let store = Arc::new(MemoryStore::default());
        let artifact_id = "explain-analyze-test";
        store
            .persist_json_artifact(explain_record(
                artifact_id,
                "user-a",
                "session-a",
                "complete",
            ))
            .await
            .unwrap();
        let handle = artifact_handle(artifact_id);
        let args = json!({"artifact": handle, "max_bytes": 256});

        let readable = resolve_request(Some(store.as_ref()), "user-a", "session-a", &args)
            .await
            .expect("server handle should dispatch")
            .expect("owner should read its artifact");
        assert!(readable.contains("<explain-analyze-artifact>"));
        assert!(readable.contains("explain_analyze"));

        let denied = resolve_request(Some(store.as_ref()), "user-b", "session-a", &args)
            .await
            .expect("server handle should dispatch")
            .expect_err("cross-user reads must not resolve");
        assert!(denied.contains("not found for this session"));
    }

    #[tokio::test]
    async fn reader_refuses_an_unavailable_capture() {
        let store = Arc::new(MemoryStore::default());
        let artifact_id = "explain-analyze-unavailable";
        store
            .persist_json_artifact(explain_record(
                artifact_id,
                "user-a",
                "session-a",
                "unavailable",
            ))
            .await
            .unwrap();
        let args = json!({"artifact": artifact_handle(artifact_id)});
        let error = resolve_request(Some(store.as_ref()), "user-a", "session-a", &args)
            .await
            .expect("server handle should dispatch")
            .expect_err("unavailable captures must not be presented as data");
        assert!(error.contains("unavailable"));
    }

    fn valid_fact(
        event_id: &str,
        run_id: &str,
        turn_id: &str,
        transition: astra_turn_types::ExplainAnalyzeTransitionV1,
    ) -> astra_turn_types::ExplainAnalyzeEventV1 {
        let finished = transition == astra_turn_types::ExplainAnalyzeTransitionV1::Finished;
        astra_turn_types::ExplainAnalyzeEventV1 {
            auxiliary_usage: None,
            auxiliary_details: None,
            schema_version: astra_turn_types::EXPLAIN_ANALYZE_SCHEMA_VERSION,
            event_id: event_id.to_string(),
            run_id: run_id.to_string(),
            turn_id: turn_id.to_string(),
            node_id: turn_id.to_string(),
            parent_node_id: None,
            dependency_node_ids: Vec::new(),
            producer_id: "server".to_string(),
            clock_domain_id: "clock-1".to_string(),
            kind: astra_turn_types::ExplainAnalyzeNodeKindV1::Turn,
            round_index: None,
            attempt_index: None,
            label: "User turn".to_string(),
            transition,
            elapsed_ms: u64::from(finished),
            start_elapsed_ms: finished.then_some(0),
            duration_ms: finished.then_some(1),
            outcome: finished.then_some(astra_turn_types::ExplainAnalyzeOutcomeV1::Completed),
            decision_detail: None,
            usage: None,
            context: None,
            coverage_gaps: Vec::new(),
        }
    }

    #[test]
    fn capture_assessment_uses_transport_gaps_and_the_typed_graph() {
        let started = serde_json::to_value(valid_fact(
            "event-start",
            "run-1",
            "turn-1",
            astra_turn_types::ExplainAnalyzeTransitionV1::Started,
        ))
        .unwrap();
        let finished = serde_json::to_value(valid_fact(
            "event-finish",
            "run-1",
            "turn-1",
            astra_turn_types::ExplainAnalyzeTransitionV1::Finished,
        ))
        .unwrap();
        let mut started = started;
        let mut finished = finished;
        started["type"] = Value::String(astra_turn_types::EXPLAIN_ANALYZE_EVENT_TYPE.to_string());
        finished["type"] = Value::String(astra_turn_types::EXPLAIN_ANALYZE_EVENT_TYPE.to_string());
        let mut gap = json!({"type": "stream_gap", "explain_analyze_recovered": false});
        gap["run_id"] = Value::String("run-1".to_string());
        let all = vec![started, finished, gap];
        let captured = explain_events(&all, "run-1", "turn-1");
        let assessment = assess_capture(&all, &captured);
        assert_eq!(assessment.status, "partial");
        assert!(assessment.delivery_degraded);
        assert_eq!(assessment.open_node_count, 0);

        let only_started = vec![all[0].clone()];
        let captured = explain_events(&only_started, "run-1", "turn-1");
        let assessment = assess_capture(&only_started, &captured);
        assert_eq!(assessment.status, "partial");
        assert_eq!(assessment.open_node_count, 1);

        let mut covered = all[1].clone();
        covered["coverage_gaps"] = json!(["child_run_intervals"]);
        let covered_events = vec![all[0].clone(), covered];
        let captured = explain_events(&covered_events, "run-1", "turn-1");
        let assessment = assess_capture(&covered_events, &captured);
        assert_eq!(assessment.status, "partial");
        assert_eq!(assessment.coverage_gaps, vec!["child_run_intervals"]);
    }

    #[test]
    fn capture_identity_is_explicit_and_envelope_lifecycle_is_fail_closed() {
        let foreign = serde_json::to_value(valid_fact(
            "foreign",
            "run-other",
            "turn-1",
            astra_turn_types::ExplainAnalyzeTransitionV1::Finished,
        ))
        .unwrap();
        let mut foreign = foreign;
        foreign["type"] = Value::String(astra_turn_types::EXPLAIN_ANALYZE_EVENT_TYPE.to_string());
        let captured = explain_events(&[foreign], "run-1", "turn-1");
        assert!(captured.events.is_empty());
        assert_eq!(captured.foreign_event_count, 1);

        let mut artifact = stored(explain_record(
            "explain-analyze-envelope",
            "user-a",
            "session-a",
            "complete",
        ));
        assert_eq!(envelope_status(&artifact).unwrap(), "complete");
        artifact.status = Some("expired".to_string());
        assert!(envelope_status(&artifact).is_err());
        artifact.status = Some("active".to_string());
        artifact.content["capture_status"] = Value::String("future".to_string());
        assert!(envelope_status(&artifact).is_err());
        artifact.content["capture_status"] = Value::String("complete".to_string());
        artifact.content["artifact_schema_version"] = Value::from(999_u64);
        assert!(envelope_status(&artifact).is_err());
    }

    #[test]
    fn snapshot_validation_rejects_run_generation_and_fact_scope_mismatch() {
        let mut artifact = stored(explain_record(
            "explain-analyze-scope",
            "user-a",
            "session-a",
            "complete",
        ));
        assert!(validate_snapshot_payload(&artifact, "session-a", Some("run-1"), Some(1)).is_ok());
        assert!(
            validate_snapshot_payload(&artifact, "session-a", Some("run-other"), Some(1)).is_err()
        );
        assert!(validate_snapshot_payload(&artifact, "session-a", Some("run-1"), Some(2)).is_err());

        artifact.content["events"][0]["run_id"] = Value::String("run-other".to_string());
        assert!(validate_snapshot_payload(&artifact, "session-a", Some("run-1"), Some(1)).is_err());

        artifact.content["events"][0]["run_id"] = Value::String("run-1".to_string());
        artifact.metadata.as_mut().unwrap()["status"] = Value::String("partial".to_string());
        assert!(envelope_status(&artifact).is_err());
    }

    fn refresh_integrity_metadata(artifact: &mut StoredSessionArtifact) {
        let bytes = serde_json::to_vec_pretty(&artifact.content).unwrap();
        let metadata = artifact.metadata.as_mut().unwrap();
        metadata["size_bytes"] = Value::from(bytes.len());
        metadata["checksum_sha256"] = Value::String(format!("{:x}", Sha256::digest(&bytes)));
    }

    #[test]
    fn snapshot_validation_rejects_complete_capture_with_open_or_uncovered_graph() {
        let mut open = stored(explain_record(
            "explain-analyze-open",
            "user-a",
            "session-a",
            "complete",
        ));
        open.content["events"][0]["transition"] = Value::String("started".to_string());
        open.content["events"][0]
            .as_object_mut()
            .unwrap()
            .remove("outcome");
        open.content["events"][0]
            .as_object_mut()
            .unwrap()
            .remove("duration_ms");
        open.content["events"][0]
            .as_object_mut()
            .unwrap()
            .remove("start_elapsed_ms");
        open.content["open_node_count"] = Value::from(1_u64);
        refresh_integrity_metadata(&mut open);
        let error = validate_snapshot_payload(&open, "session-a", Some("run-1"), Some(1))
            .expect_err("complete captures must not contain open nodes");
        assert!(error.contains("claims complete"));

        let mut uncovered = stored(explain_record(
            "explain-analyze-uncovered",
            "user-a",
            "session-a",
            "complete",
        ));
        uncovered.content["events"][0]["coverage_gaps"] = json!(["child_run_intervals"]);
        uncovered.content["coverage_gaps"] = json!(["child_run_intervals"]);
        refresh_integrity_metadata(&mut uncovered);
        let error = validate_snapshot_payload(&uncovered, "session-a", Some("run-1"), Some(1))
            .expect_err("complete captures must not hide coverage gaps");
        assert!(error.contains("claims complete"));
    }

    #[test]
    fn snapshot_validation_rejects_storage_size_and_checksum_mismatch() {
        let mut artifact = stored(explain_record(
            "explain-analyze-integrity",
            "user-a",
            "session-a",
            "complete",
        ));
        artifact.metadata.as_mut().unwrap()["size_bytes"] = Value::from(1_u64);
        let error = validate_snapshot_payload(&artifact, "session-a", Some("run-1"), Some(1))
            .expect_err("size metadata must be checked");
        assert!(error.contains("size metadata"));

        let mut artifact = stored(explain_record(
            "explain-analyze-integrity-checksum",
            "user-a",
            "session-a",
            "complete",
        ));
        artifact.metadata.as_mut().unwrap()["checksum_sha256"] = Value::String("0".repeat(64));
        let error = validate_snapshot_payload(&artifact, "session-a", Some("run-1"), Some(1))
            .expect_err("checksum metadata must be checked");
        assert!(error.contains("checksum metadata"));
    }
}
