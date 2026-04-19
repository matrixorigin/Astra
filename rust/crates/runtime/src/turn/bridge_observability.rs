//! Bridge-specific observability: context trace signals and quality assessment persistence.

use std::collections::HashSet;

use serde_json::json;

use astra_services::evaluation::SessionQualityAssessmentRequest;
use astra_services::session_workspace::{
    ContextTraceBudgetSignal, ContextTraceSignal, ContextTraceTimingSignal,
    ContextTraceToolSelection,
};

use crate::{
    DatabaseEvaluationService, DatabaseEventService, EvaluationService, EventCreateRequestData,
    EventService, MatrixOneSettings,
};
use astra_core::SharedPool;

/// Build a legacy context trace signal for bridge turns.
pub(crate) fn build_legacy_context_trace_signal(
    turn: u32,
    turn_id: String,
    tools_available: usize,
    selected_tools: Vec<String>,
    selection_confidence: f64,
    measured_prompt_tokens: Option<u64>,
    model_limit: usize,
    tool_execution_ms: u64,
    total_ms: u64,
) -> ContextTraceSignal {
    let unique_selected = selected_tools.iter().cloned().collect::<HashSet<_>>().len();
    let budget = measured_prompt_tokens.map(|total_used| ContextTraceBudgetSignal {
        max_tokens: model_limit.min(u32::MAX as usize) as u32,
        total_used: total_used.min(u32::MAX as u64) as u32,
        budget_pressure: if model_limit > 0 {
            total_used as f64 / model_limit as f64
        } else {
            0.0
        },
        compression_triggered: false,
    });
    let llm_total_ms = total_ms.saturating_sub(tool_execution_ms);

    ContextTraceSignal {
        turn_id,
        captured_at: Some(chrono::Utc::now().to_rfc3339()),
        tool_selection: Some(ContextTraceToolSelection {
            tools_available: tools_available.min(u32::MAX as usize) as u32,
            selected_tools,
            rejected_tools: tools_available.saturating_sub(unique_selected),
            strategy: "inprocess_bridge".to_string(),
            confidence: selection_confidence,
            latency_ms: 0,
        }),
        memory: None,
        history: None,
        budget,
        timing: Some(ContextTraceTimingSignal {
            turn,
            context_assembly_ms: 0,
            ttft_ms: 0,
            llm_total_ms,
            tool_execution_ms,
            total_ms,
        }),
        explanations: Vec::new(),
    }
}

/// Persist the context trace signal and optional quality assessment for a bridge turn.
pub(crate) async fn persist_legacy_bridge_trace_and_quality(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<SharedPool>,
    user_id: String,
    session_id: String,
    agent_id: Option<String>,
    turn_chain_id: String,
    signal: ContextTraceSignal,
    evaluation: Option<crate::pipeline::evaluation::TurnEvaluation>,
    step_count: usize,
) {
    let Some(shared_pool) = shared_pool else {
        return;
    };

    if let Some(evaluation) = evaluation {
        let evaluation_service =
            DatabaseEvaluationService::new(matrixone.clone()).with_pool(shared_pool.clone());
        let assessment = SessionQualityAssessmentRequest {
            session_id: session_id.clone(),
            score: evaluation.quality,
            step_count: i32::try_from(step_count).unwrap_or(i32::MAX),
        };
        if let Err((status, response)) = evaluation_service
            .record_session_quality_assessment(&user_id, assessment)
            .await
        {
            astra_core::agent_warn!(
                "legacy-bridge",
                "Failed to persist session quality assessment for {}: {} {}",
                session_id,
                status,
                response.0.detail
            );
        }
    }

    let event_service = DatabaseEventService::new(matrixone.clone()).with_pool(shared_pool);
    let mut metadata = match serde_json::to_value(&signal) {
        Ok(metadata) => metadata,
        Err(err) => {
            astra_core::agent_warn!(
                "legacy-bridge",
                "Failed to serialize context trace signal for {}: {}",
                session_id,
                err
            );
            return;
        }
    };
    if let Some(metadata_obj) = metadata.as_object_mut() {
        if let Some(duration_ms) = signal.timing.as_ref().map(|timing| timing.total_ms) {
            metadata_obj.insert(
                "duration_ms".to_string(),
                json!(duration_ms.min(i32::MAX as u64)),
            );
        }
        if let Some(tool_name) = signal
            .tool_selection
            .as_ref()
            .and_then(|selection| selection.selected_tools.first())
        {
            metadata_obj.insert("tool_name".to_string(), json!(tool_name));
        }
    }

    let content = {
        let preview = signal.preview();
        if preview.is_empty() {
            "context trace signal".to_string()
        } else {
            preview
        }
    };
    let turn_id = if signal.turn_id.is_empty() {
        "latest".to_string()
    } else {
        signal.turn_id.clone()
    };
    if let Err((status, response)) = event_service
        .create_event(
            user_id,
            EventCreateRequestData {
                session_id,
                event_type: "context_trace_signal".to_string(),
                content,
                agent_id,
                agent_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                parent_event_id: None,
                parent_event_ids: Some(Vec::new()),
                causal_chain_id: Some(format!("{turn_chain_id}:context-trace:{turn_id}")),
                metadata: Some(metadata),
            },
        )
        .await
    {
        astra_core::agent_warn!(
            "legacy-bridge",
            "Failed to persist context trace signal: {} {}",
            status,
            response.0.detail
        );
    }
}
