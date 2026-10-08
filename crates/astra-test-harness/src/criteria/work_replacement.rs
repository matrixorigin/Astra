//! Verify canonical replacement receipts; never infer lifecycle from prose.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::session_capture::{JournalToolCall, SessionCapture};

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("canonical replacement evidence is missing {key}"))
}

fn revision(value: &Value, key: &str) -> Result<u64, String> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("canonical replacement evidence has invalid {key}"))
}

#[derive(Clone)]
struct Item {
    revision: u64,
    declaration: String,
    execution: String,
    delivery: String,
}

fn board_items(board: &Value) -> Result<BTreeMap<String, Item>, String> {
    let rows = board
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or("canonical replacement evidence lacks a complete task board")?;
    let mut items = BTreeMap::new();
    for row in rows {
        let id = string(row, "item_id")?;
        // `root` is the canonical Work milestone, not an executable task.
        // This is the same reserved identity used by graph-patch evidence.
        if id == "root" {
            continue;
        }
        let item = Item {
            revision: revision(row, "item_revision")?,
            declaration: string(row, "declaration_state")?.into(),
            execution: string(row, "execution_status")?.into(),
            delivery: string(row, "delivery_status")?.into(),
        };
        if items.insert(id.to_owned(), item).is_some() {
            return Err("duplicate item identity in canonical task board".into());
        }
    }
    Ok(items)
}

#[derive(Default)]
pub(super) struct Timing {
    pub cancellation_after_deliveries: usize,
    pub added_execution_after_initial_deliveries: usize,
    pub require_added_at_start: bool,
    pub require_added_before_execution: bool,
}

fn validate_added_before_execution(
    calls: &[JournalToolCall],
    boundary: usize,
    scope: &(String, String),
    declared: &BTreeMap<String, u64>,
    added_count: usize,
) -> Result<BTreeMap<String, u64>, String> {
    let producer = calls[boundary]
        .run_id
        .as_deref()
        .ok_or("first execution lacks producer")?;
    let prefix = &calls[..boundary];
    let start = prefix
        .iter()
        .find(|call| {
            call.name == "start_work"
                && call.ok == Some(true)
                && call.run_id.as_deref() == Some(producer)
                && call
                    .result
                    .as_ref()
                    .is_some_and(|result| result["status"] == "started")
        })
        .and_then(|call| call.result.as_ref())
        .ok_or("addition timing lacks genesis")?;
    let mut added: BTreeMap<_, _> = board_items(&start["task_board_update"])?
        .into_iter()
        .filter(|(id, _)| !declared.contains_key(id))
        .map(|(id, item)| (id, item.revision))
        .collect();
    for (index, call) in prefix.iter().enumerate() {
        if call.name != "propose_work_plan" || call.ok != Some(true) {
            continue;
        }
        let result = call.result.as_ref().ok_or("proposal lacks receipt")?;
        if result["status"] != "accepted" {
            continue;
        }
        let args = call
            .arguments
            .as_ref()
            .ok_or("accepted addition lacks typed arguments")?;
        let additions = args["additions"]
            .as_array()
            .ok_or("accepted addition lacks additions")?;
        if additions.is_empty() {
            continue;
        }
        let context = string(args, "context_id")?;
        let inspection = prefix[..index]
            .iter()
            .rev()
            .find_map(|inspection| {
                if inspection.name != "inspect_work_plan"
                    || inspection.ok != Some(true)
                    || inspection.run_id.as_deref() != Some(producer)
                {
                    return None;
                }
                inspection
                    .result
                    .as_ref()
                    .filter(|result| result["context_id"] == context)
            })
            .ok_or("accepted addition lacks its prior inspection context")?;
        let basis = &inspection["basis"];
        if call.run_id.as_deref() != Some(producer)
            || string(basis, "work_id")? != scope.0
            || string(basis, "branch_id")? != scope.1
            || revision(result, "result_graph_revision")? <= revision(basis, "graph_revision")?
        {
            return Err(
                "accepted addition crosses scope or does not advance its inspection basis".into(),
            );
        }
        let mut requested = BTreeSet::new();
        for addition in additions {
            let id = string(addition, "item_id")?;
            if id == "root" || declared.contains_key(id) || !requested.insert(id.to_owned()) {
                return Err("accepted addition does not use unique fresh identities".into());
            }
        }
        let mut applied = BTreeMap::new();
        for addition in result["applied_mutations"]["added_items"]
            .as_array()
            .ok_or("accepted addition lacks applied item receipts")?
        {
            let id = string(addition, "item_id")?.to_owned();
            if applied
                .insert(id, revision(addition, "revision")?)
                .is_some()
            {
                return Err("accepted addition repeats an applied identity".into());
            }
        }
        if requested != applied.keys().cloned().collect() {
            return Err("accepted addition arguments disagree with applied receipts".into());
        }
        for (id, revision) in applied {
            if added
                .insert(id, revision)
                .is_some_and(|old| old != revision)
            {
                return Err("accepted addition changes a previously committed identity".into());
            }
        }
    }
    if added.len() != added_count {
        return Err("every required addition must be committed before first execution".into());
    }
    Ok(added)
}

pub(super) fn verify(
    session: &SessionCapture,
    initial_count: usize,
    cancelled_count: usize,
    added_count: usize,
    delivered_count: usize,
    timing: Timing,
) -> Result<String, String> {
    let Timing {
        cancellation_after_deliveries: after_deliveries,
        added_execution_after_initial_deliveries: added_after_initial_deliveries,
        require_added_at_start,
        require_added_before_execution,
    } = timing;
    if session.has_integrity_errors() || session.skipped_lines > 0 || session.dropped_lines > 0 {
        return Err("replacement lifecycle requires an intact, complete journal".into());
    }
    let calls = session.journal_tool_calls();
    let mut identity: Option<(String, String)> = None;
    let mut declared = BTreeMap::new();
    let mut previous: BTreeMap<String, Item> = BTreeMap::new();
    let mut previous_graph = 0;
    let mut executed = BTreeSet::new();
    let mut pre_execution_additions = None;
    let mut assigned_attempts = BTreeMap::new();
    let mut cancelled = BTreeSet::new();
    // One item may have an idempotently replayed settlement, but cannot count
    // as delivered again under a different execution attempt or revision.
    let mut delivered: BTreeMap<String, (u64, String)> = BTreeMap::new();
    let mut intervening_mutation = false;
    for (call_index, call) in calls.iter().enumerate() {
        // Claims and board projections are not evidence of tool execution.
        // Only the executor's frozen attribution can assign an invocation to
        // an attempt, including executions that returned an error.
        if !matches!(
            call.name.as_str(),
            "start_work" | "run_next_work_item" | "settle_work_item" | "propose_work_plan"
        ) {
            let disposition = call
                .runtime_metadata
                .get("disposition")
                .filter(|value| !value.is_null())
                .map(|value| {
                    serde_json::from_value::<astra_services::session_journal::ToolCallDisposition>(
                        value.clone(),
                    )
                })
                .transpose()
                .map_err(|_| "invocation has an invalid execution disposition")?;
            if disposition == Some(astra_services::session_journal::ToolCallDisposition::Executed) {
                let attribution: astra_services::runs::WorkInvocationAttribution =
                    serde_json::from_value(
                        call.runtime_metadata
                            .get("work_attribution")
                            .cloned()
                            .unwrap_or(Value::Null),
                    )
                    .map_err(|_| "executed invocation lacks valid frozen Work attribution")?;
                let producer = call
                    .run_id
                    .as_deref()
                    .ok_or("executed invocation lacks producer run identity")?;
                attribution.validate(Some(producer))?;
                match attribution {
                    astra_services::runs::WorkInvocationAttribution::Attempt {
                        binding, ..
                    } => {
                        if identity.as_ref() != Some(&(binding.work_id, binding.branch_id)) {
                            return Err(
                                "executed invocation crosses Work/branch scope or precedes genesis"
                                    .into(),
                            );
                        }
                        let item = binding.item.expect("validated attempt contains an item");
                        let selected = previous
                            .get(&item.item_id)
                            .ok_or("executed invocation references an absent item")?;
                        if selected.revision != item.item_revision as u64
                            || selected.declaration != "active"
                        {
                            return Err(
                                "executed invocation references a stale or retired item revision"
                                    .into(),
                            );
                        }
                        if assigned_attempts.get(&item.item_id)
                            != Some(&(item.item_revision as u64, item.attempt_id.clone()))
                        {
                            return Err(
                                "executed invocation does not match the assigned attempt".into()
                            );
                        }
                        if require_added_before_execution && executed.is_empty() {
                            pre_execution_additions = Some(validate_added_before_execution(
                                &calls,
                                call_index,
                                identity.as_ref().unwrap(),
                                &declared,
                                added_count,
                            )?);
                        }
                        executed.insert(item.item_id);
                        if delivered
                            .keys()
                            .filter(|id| declared.contains_key(*id))
                            .count()
                            < added_after_initial_deliveries
                            && executed.iter().any(|id| !declared.contains_key(id))
                        {
                            return Err(
                                "added item executed before the required initial deliveries".into(),
                            );
                        }
                    }
                    astra_services::runs::WorkInvocationAttribution::Unknown => {
                        return Err("executed invocation has unknown Work attribution".into());
                    }
                    astra_services::runs::WorkInvocationAttribution::Control
                    | astra_services::runs::WorkInvocationAttribution::Unbound => {}
                }
            } else if identity.is_some() && disposition.is_none() {
                return Err("invocation lacks an executor-authored execution disposition".into());
            }
        }
        if call.ok != Some(true) {
            continue;
        }
        if call.name == "propose_work_plan" {
            intervening_mutation |= call
                .result
                .as_ref()
                .and_then(|result| result.get("status"))
                .and_then(Value::as_str)
                == Some("accepted");
            continue;
        }
        // These producers publish complete canonical boards. Arbitrary
        // inspection pages and partial upserts are not absence evidence.
        if !matches!(
            call.name.as_str(),
            "start_work" | "run_next_work_item" | "settle_work_item"
        ) {
            continue;
        }
        let result = call
            .result
            .as_ref()
            .ok_or("canonical tool receipt has no result")?;
        if call.name == "start_work" && string(result, "status")? != "started" {
            continue;
        }
        if call.name == "settle_work_item" && string(result, "status")? != "recorded" {
            continue;
        }
        let board = result
            .get("task_board_update")
            .ok_or("canonical receipt lacks its complete board")?;
        let scope = (
            string(board, "work_id")?.to_owned(),
            string(board, "branch_id")?.to_owned(),
        );
        if call.name == "run_next_work_item" {
            if !matches!(
                string(result, "status")?,
                "assigned"
                    | "in_flight"
                    | "needs_recovery"
                    | "graph_mutation_pending"
                    | "blocked"
                    | "complete"
            ) {
                return Err("unrecognized canonical run-next receipt status".into());
            }
            for (field, expected) in [("work_id", &scope.0), ("branch_id", &scope.1)] {
                if result.get(field).is_some() && string(result, field)? != expected.as_str() {
                    return Err("run-next top-level scope disagrees with its board".into());
                }
            }
        } else if string(result, "work_id")? != scope.0 || string(result, "branch_id")? != scope.1 {
            return Err("task board crosses Work/branch scope".into());
        }
        if call.name == "start_work" {
            if identity.is_some() {
                return Err("replacement evidence contains multiple Work starts".into());
            }
            let tasks = result
                .get("declared_tasks")
                .and_then(Value::as_array)
                .ok_or("start_work lacks the declared initial item identities")?;
            for task in tasks {
                let id = string(task, "item_id")?;
                if id == "root"
                    || declared
                        .insert(id.to_owned(), revision(task, "item_revision")?)
                        .is_some()
                {
                    return Err("invalid or duplicate declared initial identity".into());
                }
            }
            if declared.len() != initial_count
                || result.get("initial_item_count").and_then(Value::as_u64)
                    != Some(initial_count as u64)
            {
                return Err(
                    "declared initial item count does not match the requested contract".into(),
                );
            }
            identity = Some(scope.clone());
        }
        if identity.as_ref() != Some(&scope) {
            return Err(
                "replacement receipt is missing genesis or crosses Work/branch scope".into(),
            );
        }
        let graph = revision(board, "graph_revision")?;
        if graph < previous_graph {
            return Err("canonical graph revision moved backwards".into());
        }
        let items = board_items(board)?;
        if call.name == "start_work" && (require_added_at_start || require_added_before_execution) {
            let added_at_start: BTreeSet<_> = items
                .keys()
                .filter(|id| !declared.contains_key(*id))
                .cloned()
                .collect();
            if require_added_at_start && added_at_start.len() != added_count {
                return Err(
                    "initial board does not already contain every required added item".into(),
                );
            }
            let applied_admission_mutations = result
                .get("applied_admission_mutations")
                .and_then(Value::as_array)
                .ok_or("start_work does not report applied admission mutations")?;
            let reported_additions: BTreeSet<_> = applied_admission_mutations
                .iter()
                .filter_map(|mutation| mutation.get("added_item_ids").and_then(Value::as_array))
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            if reported_additions != added_at_start {
                return Err(
                    "start_work mutation receipt does not identify each already-added item".into(),
                );
            }
        }
        for (id, base_revision) in &declared {
            if !items
                .get(id)
                .is_some_and(|item| item.revision >= *base_revision)
            {
                return Err(
                    "complete board omits a declared item or predates its declared revision".into(),
                );
            }
        }
        for (id, old) in &previous {
            if !items
                .get(id)
                .is_some_and(|item| item.revision >= old.revision)
            {
                return Err("complete board omitted an item or regressed its revision".into());
            }
        }
        let prior_deliveries = delivered.len();
        let mut new_delivery = false;
        for assignment in std::iter::once(result)
            .chain(result.get("initial_task"))
            .chain(result.get("next_task"))
        {
            if assignment.get("status").and_then(Value::as_str) == Some("assigned") {
                let id = string(assignment, "item_id")?;
                let attempt = string(assignment, "attempt_id")?;
                let selected = items
                    .get(id)
                    .ok_or("assigned item is absent from its board")?;
                if revision(assignment, "item_revision")? != selected.revision {
                    return Err("assignment revision disagrees with its board".into());
                }
                if selected.declaration != "active" {
                    return Err("assignment references a retired item".into());
                }
                assigned_attempts.insert(id.to_owned(), (selected.revision, attempt.to_owned()));
            }
        }
        if call.name == "settle_work_item" {
            let transition = result
                .get("settlement_transition")
                .ok_or("settlement lacks canonical transition")?;
            if string(transition, "authority")? != "canonical_work_state"
                || string(transition, "delivery_status")? != "delivered"
                || string(transition, "execution_status")? != "completed"
            {
                return Err("replacement contains a non-delivered canonical settlement".into());
            }
            let id = string(result, "item_id")?;
            let item_revision = revision(result, "item_revision")?;
            if string(transition, "item_id")? != id
                || revision(transition, "item_revision")? != item_revision
            {
                return Err("settlement identity disagrees with its canonical transition".into());
            }
            let execution = (item_revision, string(result, "attempt_id")?.to_owned());
            if assigned_attempts
                .get(id)
                .is_some_and(|assigned| assigned != &execution)
            {
                return Err("settlement does not match the assigned execution attempt".into());
            }
            if let Some(existing) = delivered.get(id) {
                if existing != &execution {
                    return Err("item delivered under multiple execution identities".into());
                }
            } else {
                delivered.insert(id.to_owned(), execution);
                new_delivery = true;
            }
            executed.insert(id.to_owned());
        }
        for (id, item) in &items {
            if cancelled.contains(id) && item.declaration != "cancelled" {
                return Err("a cancelled identity was revived".into());
            }
            if item.declaration == "cancelled" {
                let base = declared
                    .get(id)
                    .ok_or("cancelled item was not part of the initial declaration")?;
                if item.revision <= *base
                    || item.execution != "not_started"
                    || item.delivery != "unreported"
                    || executed.contains(id)
                {
                    return Err("cancelled initial item has execution/delivery evidence or lacks retirement revision".into());
                }
                if !cancelled.contains(id) && after_deliveries > 0 {
                    let previously_waiting = previous.get(id).is_some_and(|old| {
                        old.declaration == "active"
                            && matches!(old.execution.as_str(), "not_started" | "running")
                            && !executed.contains(id)
                            && old.delivery == "unreported"
                    });
                    // A complete prior snapshot after N deliveries proves the
                    // item was still waiting then. Alternatively this very
                    // settlement delivers, reconciles deferred mutations, and
                    // publishes the complete board in canonical handler order.
                    let delivery_precedes_change = prior_deliveries >= after_deliveries
                        || (new_delivery
                            && delivered.len() >= after_deliveries
                            && !intervening_mutation);
                    if !previously_waiting || graph <= previous_graph || !delivery_precedes_change {
                        return Err("cancellation timing lacks an active prior snapshot and causally earlier delivery".into());
                    }
                }
                cancelled.insert(id.to_owned());
            } else if item.execution == "completed" || item.delivery != "unreported" {
                executed.insert(id.to_owned());
            }
        }
        let initial_deliveries = delivered
            .keys()
            .filter(|id| declared.contains_key(*id))
            .count();
        if initial_deliveries < added_after_initial_deliveries
            && executed.iter().any(|id| !declared.contains_key(id))
        {
            return Err("added item executed before the required initial deliveries".into());
        }
        previous = items;
        previous_graph = graph;
        intervening_mutation = false;
    }
    if identity.is_none() {
        return Err("replacement lifecycle lacks canonical Work genesis".into());
    }
    let added: BTreeSet<_> = previous
        .keys()
        .filter(|id| !declared.contains_key(*id))
        .cloned()
        .collect();
    if require_added_before_execution {
        let proved = pre_execution_additions.ok_or("addition timing lacks scoped execution")?;
        if proved.keys().cloned().collect::<BTreeSet<_>>() != added
            || proved
                .iter()
                .any(|(id, revision)| previous[id].revision != *revision)
        {
            return Err("final complete board does not corroborate pre-execution additions".into());
        }
    }
    let expected: BTreeSet<_> = declared
        .keys()
        .filter(|id| !cancelled.contains(*id))
        .cloned()
        .chain(added.iter().cloned())
        .collect();
    let actual: BTreeSet<_> = delivered.keys().cloned().collect();
    if cancelled.len() != cancelled_count
        || added.len() != added_count
        || actual.len() != delivered_count
        || actual != expected
    {
        return Err(format!(
            "replacement sets disagree: initial={}, cancelled={}, added={}, delivered={}",
            declared.len(),
            cancelled.len(),
            added.len(),
            actual.len()
        ));
    }
    for id in &expected {
        let item = previous
            .get(id)
            .ok_or("final board omits a required delivered item")?;
        if item.declaration != "active"
            || item.execution != "completed"
            || item.delivery != "delivered"
            || delivered.get(id).map(|execution| execution.0) != Some(item.revision)
        {
            return Err("final task state does not corroborate its delivered settlement".into());
        }
    }
    Ok(format!(
        "canonical replacement lifecycle verified: {initial_count} initial, {cancelled_count} cancelled without execution, {added_count} fresh, {delivered_count} delivered"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_capture::JournalEvent;
    use serde_json::json;

    fn verify(
        session: &SessionCapture,
        initial: usize,
        cancelled: usize,
        added: usize,
        delivered: usize,
        after: usize,
    ) -> Result<String, String> {
        super::verify(
            session,
            initial,
            cancelled,
            added,
            delivered,
            Timing {
                cancellation_after_deliveries: after,
                ..Default::default()
            },
        )
    }

    #[test]
    fn added_execution_waits_for_initial_delivery_but_creation_can_be_immediate() {
        let timing = || Timing {
            added_execution_after_initial_deliveries: 1,
            require_added_at_start: true,
            ..Default::default()
        };
        let valid = fixture(false, false);
        assert!(
            super::verify(&fixture(false, true), 2, 1, 1, 2, timing())
                .unwrap_err()
                .contains("initial board")
        );
        assert!(super::verify(&valid, 2, 1, 1, 2, timing()).is_ok());
        let mut unexplained = valid.clone();
        result(&mut unexplained, 0)["applied_admission_mutations"] = json!([]);
        assert!(
            super::verify(&unexplained, 2, 1, 1, 2, timing())
                .unwrap_err()
                .contains("mutation receipt")
        );
        let mut early = valid.clone();
        result(&mut early, 0)["task_board_update"]["tasks"][2]["execution_status"] =
            json!("running");
        assert!(super::verify(&early, 2, 1, 1, 2, timing()).is_ok());
        let mut assigned = valid.clone();
        result(&mut assigned, 0)["initial_task"] = json!({
            "status":"assigned","item_id":"fresh","item_revision":1,"attempt_id":"early-attempt"
        });
        result(&mut assigned, 2)["attempt_id"] = json!("early-attempt");
        assert!(super::verify(&assigned, 2, 1, 1, 2, timing()).is_ok());
        for ok in [true, false] {
            let mut actual = assigned.clone();
            actual.events.insert(1, JournalEvent {
                event_type: "turn".into(),
                raw: json!({"producer_scope":{"run_id":"producer"},"tool_calls":[{
                    "tool_call_id":"fresh-execution","name":"read_file","ok":ok,"disposition":"executed",
                    "work_attribution":{"state":"attempt","producer_run_id":"producer","binding":{
                        "work_id":"work","branch_id":"branch","item":{
                            "item_id":"fresh","item_revision":1,"attempt_id":"early-attempt"
                        }
                    }}
                }]}),
            });
            assert!(
                super::verify(&actual, 2, 1, 1, 2, timing())
                    .unwrap_err()
                    .contains("added item executed")
            );
        }
        let mut after_delivery = valid;
        result(&mut after_delivery, 1)["next_task"] = json!({
            "status":"assigned","item_id":"fresh","item_revision":1,"attempt_id":"attempt-b"
        });
        assert!(super::verify(&after_delivery, 2, 1, 1, 2, timing()).is_ok());
    }

    #[test]
    fn additions_before_first_execution_require_scoped_committed_receipts() {
        let mut valid = fixture(false, true);
        result(&mut valid, 0)["initial_task"] = json!({
            "status":"assigned","item_id":"alpha","item_revision":1,"attempt_id":"attempt-a"
        });
        let first_board = &mut result(&mut valid, 1)["task_board_update"];
        first_board["tasks"][1] = item("beta", 1, "active", "running", "unreported");
        result(&mut valid, 1)["next_task"] = json!({
            "status":"assigned","item_id":"beta","item_revision":1,"attempt_id":"original-b"
        });
        result(&mut valid, 2)["task_board_update"]["graph_revision"] = json!(3);
        let mut selected_board = result(&mut valid, 2)["task_board_update"].clone();
        selected_board["tasks"][2] = item("fresh", 1, "active", "running", "unreported");
        let event = |name: &str, args: Value, result: Value| JournalEvent {
            event_type: "llm_round".into(),
            raw: json!({"tool_calls":[{"name":name,"ok":true,"args_full":args,"result_full":result}]}),
        };
        let inspection = event(
            "inspect_work_plan",
            json!({}),
            json!({
                "context_id":"context", "basis":{"work_id":"work","branch_id":"branch","graph_revision":1}
            }),
        );
        let addition = event(
            "propose_work_plan",
            json!({
                "context_id":"context", "additions":[{"item_id":"fresh"}]
            }),
            json!({"status":"accepted","result_graph_revision":2,
            "applied_mutations":{"added_items":[{"item_id":"fresh","revision":1}]}}),
        );
        let execution = |id: &str, attempt: &str| {
            let mut call = event("read_file", json!({"path":"evidence"}), json!("observed"));
            call.raw["tool_calls"][0]["work_attribution"] = json!({
                "state":"attempt", "producer_run_id":"producer", "binding":{
                    "work_id":"work","branch_id":"branch","item":{
                        "item_id":id,"item_revision":1,"attempt_id":attempt
                    }
                }
            });
            call
        };
        let start = valid.events.remove(0);
        let first_settlement = valid.events.remove(0);
        let last_settlement = valid.events.remove(0);
        valid.events = vec![
            start,
            inspection,
            addition,
            execution("alpha", "attempt-a"),
            first_settlement,
            event(
                "propose_work_plan",
                json!({"additions":[]}),
                json!({"status":"accepted"}),
            ),
            event(
                "run_next_work_item",
                json!({}),
                json!({
                    "status":"assigned","work_id":"work","branch_id":"branch","item_id":"fresh",
                    "item_revision":1,"attempt_id":"attempt-b","task_board_update":selected_board
                }),
            ),
            execution("fresh", "attempt-b"),
            last_settlement,
        ];
        let rounds = |capture: &mut SessionCapture| {
            for (index, event) in capture.events.iter_mut().enumerate() {
                event.event_type = "llm_round".into();
                event.raw["producer_scope"] = json!({"run_id":"producer"});
                event.raw["turn"] = json!(1);
                event.raw["round"] = json!(index);
                let record = &mut event.raw["tool_calls"][0];
                record["tool_call_id"] = json!(format!("call-{index}"));
                record["disposition"] = json!("executed");
                if record.get("work_attribution").is_none() {
                    record["work_attribution"] = json!({"state":"control"});
                }
            }
        };
        rounds(&mut valid);
        let timing = || Timing {
            require_added_before_execution: true,
            cancellation_after_deliveries: 1,
            added_execution_after_initial_deliveries: 1,
            ..Default::default()
        };
        assert!(super::verify(&valid, 2, 1, 1, 2, timing()).is_ok());
        let mut recovered = valid.clone();
        let mut rejected_start = recovered.events[0].clone();
        rejected_start.raw["tool_calls"][0]["ok"] = json!(false);
        rejected_start.raw["tool_calls"][0]["result_full"] = json!({"status":"rejected"});
        recovered.events.insert(0, rejected_start);
        rounds(&mut recovered);
        assert!(super::verify(&recovered, 2, 1, 1, 2, timing()).is_ok());
        for ok in [true, false] {
            let mut late = valid.clone();
            late.events.swap(2, 3);
            late.events[2].raw["tool_calls"][0]["ok"] = json!(ok);
            rounds(&mut late);
            assert!(
                super::verify(&late, 2, 1, 1, 2, timing())
                    .unwrap_err()
                    .contains("before first execution")
            );
        }
        for (event, path, value) in [
            (2, "/result_full/status", json!("rejected")),
            (1, "/result_full/basis/work_id", json!("other-work")),
            (1, "/result_full/basis/branch_id", json!("other-branch")),
            (1, "/result_full/basis/graph_revision", json!(2)),
            (2, "/args_full/context_id", json!("forged-context")),
            (
                2,
                "/result_full/applied_mutations/added_items/0/item_id",
                json!("forged-item"),
            ),
            (
                2,
                "/result_full/applied_mutations/added_items/0/revision",
                json!(2),
            ),
        ] {
            let mut invalid = valid.clone();
            *invalid.events[event].raw["tool_calls"][0]
                .pointer_mut(path)
                .unwrap() = value;
            assert!(
                super::verify(&invalid, 2, 1, 1, 2, timing()).is_err(),
                "{path}"
            );
        }
        let criterion: crate::criteria::Criterion = serde_json::from_value(json!({
            "type":"journal_work_replacement_lifecycle", "initial_items":2,"cancelled_items":1,
            "added_items":1,"delivered_items":2,"cancellation_after_deliveries":1,
            "added_execution_after_initial_deliveries":1,"require_added_before_execution":true
        }))
        .unwrap();
        assert!(
            crate::criteria::evaluate_deterministic_with_session(
                &[criterion],
                &crate::runner::RunOutcome::default(),
                Some(&valid)
            )[0]
            .passed
        );
    }

    fn item(id: &str, revision: u64, declaration: &str, execution: &str, delivery: &str) -> Value {
        json!({"item_id":id,"item_revision":revision,"declaration_state":declaration,
            "execution_status":execution,"delivery_status":delivery})
    }

    fn fixture(cancel_first: bool, deferred: bool) -> SessionCapture {
        let cancelled = if cancel_first { "alpha" } else { "beta" };
        let retained = if cancel_first { "beta" } else { "alpha" };
        let board = |stage: usize| {
            let mut tasks = vec![item(
                retained,
                1,
                "active",
                if stage == 0 { "running" } else { "completed" },
                if stage == 0 {
                    "unreported"
                } else {
                    "delivered"
                },
            )];
            tasks.push(if deferred && stage == 0 {
                item(cancelled, 1, "active", "not_started", "unreported")
            } else {
                item(cancelled, 2, "cancelled", "not_started", "unreported")
            });
            if !deferred || stage > 0 {
                tasks.push(item(
                    "fresh",
                    1,
                    "active",
                    if stage == 2 {
                        "completed"
                    } else {
                        "not_started"
                    },
                    if stage == 2 {
                        "delivered"
                    } else {
                        "unreported"
                    },
                ));
            }
            json!({"work_id":"work","branch_id":"branch","graph_revision":if deferred && stage==0 {1} else {2},"tasks":tasks})
        };
        let settle = |id: &str, attempt: &str, stage| {
            json!({
                "status":"recorded","work_id":"work","branch_id":"branch","item_id":id,
                "item_revision":1,"attempt_id":attempt,"task_board_update":board(stage),
                "settlement_transition":{"authority":"canonical_work_state","item_id":id,"item_revision":1,
                    "execution_status":"completed","delivery_status":"delivered"}
            })
        };
        let applied_admission_mutations = if deferred {
            json!([])
        } else {
            json!([{
                "result_graph_revision": 2,
                "added_item_ids": ["fresh"],
                "revised_items": [{
                    "item_id": cancelled,
                    "from_revision": 1,
                    "declaration_state": "cancelled"
                }],
                "added_dependencies": [],
                "removed_dependencies": [],
            }])
        };
        let values = vec![
            (
                "start_work",
                json!({"status":"started","work_id":"work","branch_id":"branch","initial_item_count":2,
                "declared_tasks":[{"item_id":"alpha","item_revision":1},{"item_id":"beta","item_revision":1}],
                "applied_admission_mutations":applied_admission_mutations,"task_board_update":board(0)}),
            ),
            ("settle_work_item", settle(retained, "attempt-a", 1)),
            ("settle_work_item", settle("fresh", "attempt-b", 2)),
        ];
        SessionCapture { events: values.into_iter().enumerate().map(|(i,(name,result))| JournalEvent {
            event_type:"turn".into(),raw:json!({"tool_calls":[{"tool_call_id":format!("call-{i}"),"name":name,"ok":true,"result_full":result}]})
        }).collect(), ..Default::default() }
    }

    fn result(capture: &mut SessionCapture, index: usize) -> &mut Value {
        &mut capture.events[index].raw["tool_calls"][0]["result_full"]
    }

    #[test]
    fn either_unexecuted_initial_item_can_be_cancelled() {
        for first in [true, false] {
            assert!(verify(&fixture(first, false), 2, 1, 1, 2, 0).is_ok());
        }
    }

    #[test]
    fn cancelled_board_cannot_claim_running_or_receive_an_assignment() {
        let mut running = fixture(false, false);
        result(&mut running, 0)["task_board_update"]["tasks"][1]["execution_status"] =
            json!("running");
        assert!(verify(&running, 2, 1, 1, 2, 0).is_err());
        let mut assigned = fixture(false, false);
        result(&mut assigned, 0)["initial_task"] = json!({"status":"assigned","item_id":"beta","item_revision":1,"attempt_id":"allocated"});
        assert!(verify(&assigned, 2, 1, 1, 2, 0).is_err());
    }

    #[test]
    fn claimed_item_can_be_cancelled_but_actual_failed_execution_cannot() {
        let mut claimed = fixture(false, true);
        result(&mut claimed, 0)["initial_task"] = json!({
            "status":"assigned", "item_id":"beta", "item_revision":1, "attempt_id":"attempt-beta"
        });
        result(&mut claimed, 0)["task_board_update"]["tasks"][1]["execution_status"] =
            json!("running");
        assert!(verify(&claimed, 2, 1, 1, 2, 1).is_ok());
        let execution = |ok, attribution| JournalEvent {
            event_type: "turn".into(),
            raw: json!({"producer_scope":{"run_id":"producer"}, "tool_calls":[{
                "tool_call_id":"actual-call", "name":"read_file", "ok":ok,
                "disposition":"executed", "work_attribution":attribution
            }]}),
        };
        let attribution = json!({"state":"attempt", "producer_run_id":"producer",
        "binding":{"work_id":"work", "branch_id":"branch", "item":{
            "item_id":"beta", "item_revision":1, "attempt_id":"attempt-beta"
        }}});
        for ok in [true, false] {
            let mut actual = claimed.clone();
            actual.events.insert(1, execution(ok, attribution.clone()));
            assert!(
                verify(&actual, 2, 1, 1, 2, 1)
                    .unwrap_err()
                    .contains("execution/delivery evidence")
            );
        }
        for disposition in [json!("future_disposition"), json!(17), Value::Null] {
            let mut actual = claimed.clone();
            let mut event = execution(false, attribution.clone());
            event.raw["tool_calls"][0]["disposition"] = disposition;
            actual.events.insert(1, event);
            assert!(verify(&actual, 2, 1, 1, 2, 1).is_err());
        }
        for invalid in [
            Value::Null,
            json!({"state":"unknown"}),
            {
                let mut wrong = attribution.clone();
                wrong["producer_run_id"] = json!("other-run");
                wrong
            },
            {
                let mut wrong = attribution;
                wrong["binding"]["item"]["attempt_id"] = json!("stale-attempt");
                wrong
            },
        ] {
            let mut actual = claimed.clone();
            actual.events.insert(1, execution(false, invalid));
            assert!(verify(&actual, 2, 1, 1, 2, 1).is_err());
        }
    }

    #[test]
    fn foreign_scope_missing_items_and_revision_regressions_fail() {
        for board_field in ["work_id", "branch_id"] {
            let mut foreign = fixture(false, false);
            result(&mut foreign, 2)["task_board_update"][board_field] = json!("foreign");
            assert!(verify(&foreign, 2, 1, 1, 2, 0).is_err());
        }
        let mut missing = fixture(false, false);
        result(&mut missing, 2)["task_board_update"]["tasks"]
            .as_array_mut()
            .unwrap()
            .remove(1);
        assert!(verify(&missing, 2, 1, 1, 2, 0).is_err());
        let mut regressed = fixture(false, false);
        result(&mut regressed, 2)["task_board_update"]["graph_revision"] = json!(1);
        assert!(verify(&regressed, 2, 1, 1, 2, 0).is_err());
        let mut incomplete_genesis = fixture(false, false);
        result(&mut incomplete_genesis, 0)["task_board_update"]["tasks"]
            .as_array_mut()
            .unwrap()
            .remove(1);
        assert!(verify(&incomplete_genesis, 2, 1, 1, 2, 0).is_err());
        let mut old_genesis = fixture(false, false);
        result(&mut old_genesis, 0)["declared_tasks"][0]["item_revision"] = json!(2);
        assert!(verify(&old_genesis, 2, 1, 1, 2, 0).is_err());
    }

    #[test]
    fn run_next_uses_its_real_board_scoped_assigned_and_complete_receipts() {
        let mut capture = fixture(false, false);
        let start_board = result(&mut capture, 0)["task_board_update"].clone();
        capture.events.insert(1,JournalEvent {event_type:"turn".into(),raw:json!({"tool_calls":[{
            "tool_call_id":"resume","name":"run_next_work_item","ok":true,"result_full":{
                "status":"assigned","item_id":"alpha","item_revision":1,"attempt_id":"attempt-a",
                "task_board_update":start_board
            }
        }]})});
        let final_board = result(&mut capture, 3)["task_board_update"].clone();
        capture.events.push(JournalEvent {
            event_type: "turn".into(),
            raw: json!({"tool_calls":[{
                "tool_call_id":"complete","name":"run_next_work_item","ok":true,"result_full":{
                    "status":"complete","item_id":null,"task_board_update":final_board
                }
            }]}),
        });
        assert!(verify(&capture, 2, 1, 1, 2, 0).is_ok());
        result(&mut capture, 2)["attempt_id"] = json!("another-attempt");
        assert!(
            verify(&capture, 2, 1, 1, 2, 0)
                .unwrap_err()
                .contains("assigned execution attempt")
        );
        result(&mut capture, 2)["attempt_id"] = json!("attempt-a");
        result(&mut capture, 1)["item_revision"] = json!(2);
        assert!(verify(&capture, 2, 1, 1, 2, 0).is_err());
        result(&mut capture, 1)["item_revision"] = json!(1);
        result(&mut capture, 4)["status"] = json!("invented");
        assert!(verify(&capture, 2, 1, 1, 2, 0).is_err());
        result(&mut capture, 4)["status"] = json!("complete");
        result(&mut capture, 4)["work_id"] = json!("foreign");
        assert!(verify(&capture, 2, 1, 1, 2, 0).is_err());
    }

    #[test]
    fn replay_is_idempotent_but_another_attempt_cannot_deliver_twice() {
        let mut replay = fixture(false, false);
        let mut duplicate = replay.events[1].clone();
        duplicate.raw["tool_calls"][0]["tool_call_id"] = json!("replay");
        replay.events.insert(2, duplicate);
        assert!(verify(&replay, 2, 1, 1, 2, 0).is_ok());
        result(&mut replay, 2)["attempt_id"] = json!("another-attempt");
        assert!(verify(&replay, 2, 1, 1, 2, 0).is_err());
    }

    #[test]
    fn deferred_timing_requires_causal_settlement_not_a_late_snapshot() {
        assert!(verify(&fixture(false, true), 2, 1, 1, 2, 1).is_ok());
        assert!(verify(&fixture(false, false), 2, 1, 1, 2, 1).is_err());
        let mut uncertain = fixture(false, true);
        uncertain.events.insert(1,JournalEvent {event_type:"turn".into(),raw:json!({"tool_calls":[{
            "tool_call_id":"unobserved-change","name":"propose_work_plan","ok":true,"result_full":{"status":"accepted"}
        }]})});
        assert!(verify(&uncertain, 2, 1, 1, 2, 1).is_err());
        result(&mut uncertain, 1)["status"] = json!("rejected");
        assert!(verify(&uncertain, 2, 1, 1, 2, 1).is_ok());
    }

    #[test]
    fn corrupted_journal_and_inconsistent_counts_fail() {
        let mut capture = fixture(false, false);
        capture.dropped_lines = 1;
        assert!(verify(&capture, 2, 1, 1, 2, 0).is_err());
        assert!(verify(&fixture(false, false), 2, 1, 2, 3, 0).is_err());
    }

    #[test]
    fn declarative_criterion_requires_bound_durable_evidence() {
        use crate::criteria::{
            Criterion, evaluate_deterministic_with_session, requires_durable_run_binding,
            validate_criterion,
        };
        let criterion: Criterion = serde_json::from_value(json!({
            "type":"journal_work_replacement_lifecycle", "initial_items":2,
            "cancelled_items":1, "added_items":1, "delivered_items":2
        }))
        .unwrap();
        validate_criterion(&criterion).unwrap();
        let criteria = [criterion];
        assert!(requires_durable_run_binding(&criteria));
        let outcome = crate::runner::RunOutcome::default();
        assert!(!evaluate_deterministic_with_session(&criteria, &outcome, None)[0].passed);
        assert!(
            evaluate_deterministic_with_session(&criteria, &outcome, Some(&fixture(true, false)))
                [0]
            .passed
        );
        let invalid: Criterion = serde_json::from_value(json!({
            "type":"journal_work_replacement_lifecycle", "initial_items":2,
            "cancelled_items":3, "added_items":1, "delivered_items":0
        }))
        .unwrap();
        assert!(validate_criterion(&invalid).is_err());
    }
}
