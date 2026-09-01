//! Canonical, bounded Work state for the current model boundary.
//!
//! Conversation text is useful history, but it is not a Work-state authority.
//! This module projects the existing owner-scoped Task Graph read model into a
//! small required-context payload.  The same projection can be consumed by the
//! model, Introspect, and interactive clients without inventing another source
//! of truth.

use astra_core::SharedPool;
use astra_services::work::{
    GraphRevision, WORK_TASK_GRAPH_ITEM_PAGE_MAX_ITEMS, WorkBranchId, WorkId, WorkOwnerId,
    WorkRepository, WorkRepositoryError, WorkTaskGraphPage, WorkTaskGraphQuery,
};
use astra_turn_core::chat_turn_edge_profile::{
    EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS, RuntimeVolatileInjection, VolatileDeliveryClass,
};
use serde_json::{Map, Value, json};

pub(crate) const CANONICAL_WORK_CONTEXT_KIND: &str = "canonical_work_state";
const CANONICAL_WORK_CONTEXT_SCHEMA: &str = "canonical_work_state.v1";
const WORK_CONTEXT_TEXT_MAX_BYTES: usize = astra_server_types::WORK_TASK_BOARD_TEXT_MAX_BYTES;

#[derive(Clone, Debug)]
pub(crate) struct CanonicalWorkContextBinding {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
}

fn bounded_text(text: &str) -> String {
    if text.len() <= WORK_CONTEXT_TEXT_MAX_BYTES {
        return text.to_string();
    }
    let mut end = WORK_CONTEXT_TEXT_MAX_BYTES.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}…", &text[..end])
}

pub(crate) async fn load_canonical_work_context(
    pool: SharedPool,
    binding: &CanonicalWorkContextBinding,
    expected_graph_revision: Option<GraphRevision>,
) -> Result<Value, WorkRepositoryError> {
    let repository = astra_services::work::DatabaseWorkRepository::new(pool);
    let query = WorkTaskGraphQuery::new(
        binding.owner_id.clone(),
        binding.work_id.clone(),
        binding.branch_id.clone(),
        expected_graph_revision,
        0,
        WORK_TASK_GRAPH_ITEM_PAGE_MAX_ITEMS,
        0,
        1,
    )
    .map_err(|source| WorkRepositoryError::InvalidMutation { source })?;
    let page = repository.load_task_graph_page(query).await?;
    Ok(canonical_work_context_payload(&page))
}

fn canonical_work_context_payload(page: &WorkTaskGraphPage) -> Value {
    let basis = page.basis();
    let tasks = page
        .items()
        .entries
        .iter()
        .map(|item| {
            json!({
                "item_id": item.item_id,
                "revision": item.revision,
                "kind": item.kind,
                "objective": bounded_text(item.objective.as_str()),
                "expected_result": bounded_text(item.expected_result.as_str()),
                "declaration_state": item.declaration_state,
                "execution_status": item.execution.status,
                "delivery_status": item.delivery.status,
                "delivery_summary": item.delivery.summary.as_deref().map(bounded_text),
                "delivery_summary_authority": "model_derived_non_authoritative",
                "blocker_kind": item.delivery.blocker_kind,
                "unavailable_capabilities": item.delivery.unavailable_capabilities,
                "verification_status": item.verification.status,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "schema": CANONICAL_WORK_CONTEXT_SCHEMA,
        "authority": "server_work_repository",
        "authority_scope": "work_identity_graph_and_lifecycle_state",
        "delivery_summary_authority": "model_derived_non_authoritative",
        "status": "available",
        "work_id": basis.work_id,
        "branch_id": basis.branch_id,
        "graph_revision": basis.graph_revision,
        "goal": bounded_text(basis.goal.as_str()),
        "criteria_member_count": basis.criteria_member_count,
        "task_count": page.items().total,
        "tasks": tasks,
        "has_more_tasks": page.next_cursor().is_some(),
        "instruction": "This is authoritative for Work identity, graph, and lifecycle state only. Delivery summaries are model-derived progress notes, not factual evidence; direct tool and artifact observations outrank them. Historical assistant prose and checklists are not Work mutations. Inspect the canonical plan before changing it, and claim a change only after a durable accepted Work receipt. Use the Work planning tools for deeper or truncated state.",
    })
}

pub(crate) fn unavailable_canonical_work_context(binding: &CanonicalWorkContextBinding) -> Value {
    json!({
        "schema": CANONICAL_WORK_CONTEXT_SCHEMA,
        "authority": "server_work_repository",
        "authority_scope": "work_identity_graph_and_lifecycle_state",
        "delivery_summary_authority": "model_derived_non_authoritative",
        "status": "unavailable",
        "work_id": binding.work_id,
        "branch_id": binding.branch_id,
        "instruction": "Canonical Work state could not be refreshed. Do not infer Work state from assistant prose or claim a mutation without a durable accepted Work receipt.",
    })
}

pub(crate) fn install_canonical_work_context(
    edge_profile: &mut Map<String, Value>,
    payload: Value,
) {
    let entry = edge_profile
        .entry(EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let items = match entry {
        Value::Array(items) => items,
        _ => {
            *entry = Value::Array(Vec::new());
            entry.as_array_mut().expect("just installed array")
        }
    };
    items.retain(|item| {
        item.get("kind").and_then(Value::as_str) != Some(CANONICAL_WORK_CONTEXT_KIND)
    });
    let injection = RuntimeVolatileInjection {
        kind: CANONICAL_WORK_CONTEXT_KIND.to_string(),
        delivery_class: VolatileDeliveryClass::RequiredContext,
        payload,
        round_index: 0,
    };
    items.push(serde_json::to_value(injection).expect("canonical Work context must serialize"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_work_context_replaces_only_its_own_typed_lane() {
        let mut profile = Map::from_iter([(
            EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS.to_string(),
            json!([{
                "kind": "policy_advisory",
                "delivery_class": "advisory_evidence",
                "payload": {"keep": true},
                "round_index": 2
            }]),
        )]);
        install_canonical_work_context(&mut profile, json!({"graph_revision": 1}));
        install_canonical_work_context(&mut profile, json!({"graph_revision": 2}));
        let injections =
            astra_turn_core::chat_turn_edge_profile::edge_profile_runtime_volatile_injections(
                &profile,
            );
        assert_eq!(injections.len(), 2);
        let work = injections
            .iter()
            .find(|entry| entry.kind == CANONICAL_WORK_CONTEXT_KIND)
            .expect("canonical Work injection");
        assert_eq!(work.delivery_class, VolatileDeliveryClass::RequiredContext);
        assert_eq!(work.payload["graph_revision"], 2);
        assert!(
            injections
                .iter()
                .any(|entry| entry.kind == "policy_advisory")
        );
    }

    #[test]
    fn bounded_text_is_utf8_safe() {
        let text = "界".repeat(WORK_CONTEXT_TEXT_MAX_BYTES);
        let projected = bounded_text(&text);
        assert!(projected.len() <= WORK_CONTEXT_TEXT_MAX_BYTES);
        assert!(projected.ends_with('…'));
    }

    #[test]
    fn work_context_scopes_repository_authority_away_from_delivery_prose() {
        let binding = CanonicalWorkContextBinding {
            owner_id: WorkOwnerId::parse("owner-1").unwrap(),
            work_id: WorkId::parse("work-1").unwrap(),
            branch_id: WorkBranchId::parse("branch-1").unwrap(),
        };
        let payload = unavailable_canonical_work_context(&binding);
        assert_eq!(
            payload["authority_scope"],
            "work_identity_graph_and_lifecycle_state"
        );
        assert_eq!(
            payload["delivery_summary_authority"],
            "model_derived_non_authoritative"
        );
    }
}
