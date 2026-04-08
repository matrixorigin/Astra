pub use astra_services::storage::*;

use std::time::Duration;

use sqlx::{MySql, query};

use crate::turn::contracts::{
    TurnCoreEventRecord, TurnDecisionAuditRecord, TurnImplicitFeedbackRecord,
    TurnSkillSelectionRecord, TurnToolEventRecord,
};
use crate::turn::hook_plans::SnapshotLinkPlan;

fn metadata_tool_name(metadata: Option<&serde_json::Value>) -> Option<String> {
    metadata
        .and_then(|v| v.get("tool_name").or_else(|| v.get("name")))
        .and_then(|v| v.as_str())
        .map(|s| s.trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
}

fn metadata_duration_ms(metadata: Option<&serde_json::Value>) -> Option<i32> {
    metadata
        .and_then(|v| v.get("duration_ms"))
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
}

pub(crate) async fn insert_core_turn_event(
    tx: &mut sqlx::Transaction<'_, MySql>,
    event: &TurnCoreEventRecord,
) -> Result<(), sqlx::Error> {
    query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, agent_id, agent_version, event_type, content, \
          parent_event_id, causal_chain_id, token_usage, llm_model_used, llm_params, reasoning_content, \
          token_input, token_output, token_total, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(&event.event_id)
    .bind(&event.session_id)
    .bind(&event.user_id)
    .bind(event.agent_id.as_deref().unwrap_or("astra-cli"))
    .bind(env!("CARGO_PKG_VERSION"))
    .bind(&event.event_type)
    .bind(&event.content)
    .bind(&event.parent_event_id)
    .bind(&event.causal_chain_id)
    .bind(event.token_usage.as_ref().map(serde_json::Value::to_string))
    .bind(&event.llm_model_used)
    .bind(event.llm_params.as_ref().map(serde_json::Value::to_string))
    .bind(&event.reasoning_content)
    .bind(event.token_usage.as_ref().and_then(|v| v.get("input")).and_then(|v| v.as_i64()))
    .bind(event.token_usage.as_ref().and_then(|v| v.get("output")).and_then(|v| v.as_i64()))
    .bind(event.token_usage.as_ref().and_then(|v| v.get("total")).and_then(|v| v.as_i64()))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn insert_tool_turn_event(
    tx: &mut sqlx::Transaction<'_, MySql>,
    event: &TurnToolEventRecord,
    skill_version: Option<&String>,
) -> Result<(), sqlx::Error> {
    query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, agent_id, agent_version, event_type, content, \
          parent_event_id, causal_chain_id, metadata, skill_name, skill_version, reasoning_content, \
          meta_tool_name, meta_duration_ms, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(&event.event_id)
    .bind(&event.session_id)
    .bind(&event.user_id)
    .bind(event.agent_id.as_deref().unwrap_or("astra-cli"))
    .bind(env!("CARGO_PKG_VERSION"))
    .bind(&event.event_type)
    .bind(&event.content)
    .bind(&event.parent_event_id)
    .bind(&event.causal_chain_id)
    .bind(event.metadata.as_ref().map(serde_json::Value::to_string))
    .bind(&event.skill_name)
    .bind(skill_version.cloned().or_else(|| event.skill_version.clone()))
    .bind(&event.reasoning_content)
    .bind(metadata_tool_name(event.metadata.as_ref()))
    .bind(metadata_duration_ms(event.metadata.as_ref()))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn insert_turn_decision_audit(
    tx: &mut sqlx::Transaction<'_, MySql>,
    record: &TurnDecisionAuditRecord,
) -> Result<(), sqlx::Error> {
    query(
        "INSERT INTO ctx_decision_audits \
         (decision_id, session_id, event_id, decision_type, decision_output, model_used, context_capture_id, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(&record.decision_id)
    .bind(&record.session_id)
    .bind(&record.event_id)
    .bind(&record.decision_type)
    .bind(record.decision_output.to_string())
    .bind(&record.model_used)
    .bind(&record.context_capture_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn insert_turn_skill_selection(
    tx: &mut sqlx::Transaction<'_, MySql>,
    record: &TurnSkillSelectionRecord,
) -> Result<(), sqlx::Error> {
    query(
        "INSERT INTO skill_selection_events \
         (event_id, session_id, user_id, agent_id, user_query, selected_skills, skill_name, skill_version, selection_method, execution_success, execution_time_ms, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(&record.event_id)
    .bind(&record.session_id)
    .bind(&record.user_id)
    .bind(&record.agent_id)
    .bind(&record.user_query)
    .bind(serde_json::json!(record.selected_skills).to_string())
    .bind(&record.skill_name)
    .bind(&record.skill_version)
    .bind(&record.selection_method)
    .bind(record.execution_success)
    .bind(record.execution_time_ms)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn insert_turn_implicit_feedback(
    tx: &mut sqlx::Transaction<'_, MySql>,
    record: &TurnImplicitFeedbackRecord,
) -> Result<(), sqlx::Error> {
    query(
        "INSERT INTO eval_llm_feedback \
         (feedback_id, prompt_template_id, prompt_version, llm_request_id, rating, comment, `metadata`, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(&record.feedback_id)
    .bind(&record.prompt_template_id)
    .bind(&record.prompt_version)
    .bind(&record.llm_request_id)
    .bind(record.rating)
    .bind(&record.comment)
    .bind(record.metadata.as_ref().map(serde_json::Value::to_string))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn update_snapshot_llm_ids(
    pool: &sqlx::Pool<MySql>,
    plan: &SnapshotLinkPlan,
) -> Result<(), sqlx::Error> {
    for _ in 0..5 {
        let rows_affected = query(
            "UPDATE ctx_snapshots \
             SET llm_request_id = ?, llm_response_id = COALESCE(?, llm_response_id) \
             WHERE context_capture_id = ?",
        )
        .bind(&plan.llm_request_id)
        .bind(&plan.llm_response_id)
        .bind(&plan.context_capture_id)
        .execute(pool)
        .await?
        .rows_affected();
        if rows_affected > 0 {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}
