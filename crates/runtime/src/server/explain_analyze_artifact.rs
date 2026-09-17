//! Server-owned Explain Analyze artifact publication and recovery.
//!
//! Explain facts are produced by the server run owner.  Clients may render a
//! local companion, but the model-facing artifact must live beside the server
//! introspect reader so a handle advertised in the next turn is truthful.

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
    let store = DatabaseSessionArtifactStore::new(pool.settings().clone()).with_pool(pool.clone());
    let id = artifact_id(&run.run_id);
    if let Some(existing) = store
        .load_json_artifact(&run.user_id, &run.session_id, &id)
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

pub(crate) fn artifact_handle(artifact_id: &str) -> String {
    format!("{ARTIFACT_URI_PREFIX}{artifact_id}")
}

pub(crate) async fn snapshot_missing(
    pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
) -> Result<bool, String> {
    let Some(pool) = pool else { return Ok(false) };
    DatabaseSessionArtifactStore::new(pool.settings().clone())
        .with_pool(pool.clone())
        .load_json_artifact(user_id, session_id, &artifact_id(run_id))
        .await
        .map(|artifact| artifact.is_none())
        .map_err(|error| error.to_string())
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

fn validate_storage_integrity(artifact: &StoredSessionArtifact) -> Result<(), String> {
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
    Ok(())
}

fn validate_capture_projection(
    content: &Value,
    status: &str,
    facts: &[astra_turn_types::ExplainAnalyzeEventV1],
) -> Result<(), String> {
    let projection = graph_projection(facts);
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
    validate_storage_integrity(artifact)?;
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
        return Ok(status);
    }
    if events.is_empty() {
        return Err("readable Explain Analyze artifact contains no runtime facts".to_string());
    }
    let mut facts = Vec::with_capacity(events.len());
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
        facts.push(fact);
    }
    validate_capture_projection(content, status, &facts)?;
    Ok(status)
}

pub(crate) fn unavailable_context_notice(reason: &str) -> String {
    format!(
        "[Explain Analyze artifact discovery]\nThe latest Explain Analyze capture is unavailable. Reason: {reason}. This is background capability metadata, not a user request. Mention this limitation only when the user asks to analyze this capture. Do not infer missing timing, token, wait, or graph facts from an older artifact or from the renderer."
    )
}

/// Return the short, model-facing discovery notice for one authoritative
/// Explain run. The caller resolves the latest requested Explain run from the
/// durable run ledger; this function never falls back to an older artifact.
pub(crate) async fn context_notice_for_run(
    pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
    expected_run_id: &str,
    expected_owner_generation: u64,
) -> Result<Option<String>, String> {
    let Some(pool) = pool else {
        return Ok(Some(unavailable_context_notice(
            "server Explain Analyze artifact storage is not configured",
        )));
    };
    let store = DatabaseSessionArtifactStore::new(pool.settings().clone()).with_pool(pool.clone());
    let artifact_id = artifact_id(expected_run_id);
    let Some(artifact) = store
        .load_json_artifact(user_id, session_id, &artifact_id)
        .await
        .map_err(|error| {
            format!("load Explain Analyze artifact for run {expected_run_id}: {error}")
        })?
    else {
        return Ok(Some(unavailable_context_notice(&format!(
            "no artifact was published for Explain run {expected_run_id}"
        ))));
    };
    if artifact.artifact_kind != ARTIFACT_KIND {
        return Ok(Some(unavailable_context_notice(
            "the stored artifact kind does not match Explain Analyze",
        )));
    }
    let status = match validate_snapshot_payload(
        &artifact,
        session_id,
        Some(expected_run_id),
        Some(expected_owner_generation),
    ) {
        Ok(status) => status,
        Err(error) => return Ok(Some(unavailable_context_notice(&error))),
    };
    if status == "unavailable" {
        let reason = artifact
            .content
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("the server did not publish readable Explain Analyze facts");
        return Ok(Some(unavailable_context_notice(reason)));
    }
    let handle = artifact_handle(&artifact.artifact_id);
    let size = artifact
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("size_bytes"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    Ok(Some(format!(
        "[Explain Analyze artifact discovery]\nExplain Analyze artifact · type={ARTIFACT_TYPE} · content={CONTENT_TYPE} · storage={STORAGE} · status={status} · size={size} bytes\nHandle: {handle}\nIf the user asks about the previous/latest Explain Analyze run, call introspect(artifact=\"{handle}\", offset=0, max_bytes=65536) before drawing conclusions. The handle is server-session scoped and the bounded reader is the model-facing authority; a local TUI/Edge rendered path is only a human presentation copy. If the artifact is partial or the reader reports unavailable, state that limitation and do not infer missing timing or token facts."
    )))
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
    let offset = match args.get("offset") {
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| "offset must be a non-negative integer".to_string()),
        None => Ok(0),
    };
    let max_bytes = match args.get("max_bytes") {
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| (1..=MAX_WINDOW_BYTES).contains(value))
            .ok_or_else(|| format!("max_bytes must be an integer from 1 to {MAX_WINDOW_BYTES}")),
        None => Ok(DEFAULT_WINDOW_BYTES),
    };
    Some(async move {
        let offset = offset?;
        let max_bytes = max_bytes?;
        let artifact = store
            .load_json_artifact(user_id, session_id, &artifact_id)
            .await
            .map_err(|error| format!("load Explain Analyze artifact: {error}"))?
            .ok_or_else(|| "Explain Analyze artifact was not found for this session".to_string())?;
        if artifact.artifact_kind != ARTIFACT_KIND {
            return Err("artifact handle does not name a server Explain Analyze snapshot".to_string());
        }
        let status = validate_snapshot_payload(&artifact, session_id, None, None)?;
        if status == "unavailable" {
            return Err("the latest Explain Analyze capture is unavailable".to_string());
        }
        let bytes = serde_json::to_vec_pretty(&artifact.content)
            .map_err(|error| format!("encode Explain Analyze artifact window: {error}"))?;
        let content = String::from_utf8(bytes)
            .map_err(|error| format!("Explain Analyze artifact is not valid UTF-8: {error}"))?;
        let (window, total_bytes, next_offset) = read_window(&content, offset, max_bytes)?;
        let continuation = if next_offset >= total_bytes {
            "Complete.".to_string()
        } else {
            format!(
                "Continue with introspect(artifact=\"{handle}\", offset={next_offset}, max_bytes={max_bytes})."
            )
        };
        Ok(format!(
            "<explain-analyze-artifact>\nArtifact handle: {handle}\nBytes: [{offset}..{next_offset}) of {total_bytes}\n\n{window}\n\n{continuation}\n</explain-analyze-artifact>"
        ))
    }
    .await)
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
