use crate::cli::cli_config::cli_args::{
    WorkContinueArgs, WorkShowArgs, WorkStartArgs, WorkSubcommand,
};
use crate::cli::exit_code::ExitCode;
use crate::cli::sse_utils::stream_sse_markdown;
use astra_thin_client::work::{
    WorkTaskCheckFreshnessV2, WorkTaskCheckOutcomeV2, WorkTaskDeclarationStateV2,
};
use astra_thin_client::{
    ThinClient, WorkBranchAttachRequestV1, WorkBranchControlCommandV1,
    WorkBranchControlOperationRequestV1, WorkCreateCriterionV1, WorkCreateRequestV1,
    WorkItemDeliveryStatusV2, WorkItemExecutionStatusV2, WorkItemVerificationStatusV2,
    WorkTaskGraphPageV2, WorkTurnRequestV1,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const WORK_STREAM_ERROR_EVENTS: [&str; 2] = ["error", "run_error"];
const WORK_TASK_GRAPH_MAX_PAGES: usize = 64;

pub(crate) async fn execute_work_command(
    command: WorkSubcommand,
    token: &str,
    api: &ThinClient,
) -> Result<ExitCode, String> {
    match command {
        WorkSubcommand::Start(args) => start_work(api, token, args).await?,
        WorkSubcommand::Show(args) => show_work(api, token, args).await?,
        WorkSubcommand::Continue(args) => continue_work(api, token, args).await?,
    }
    Ok(ExitCode::Success)
}

async fn start_work(api: &ThinClient, token: &str, args: WorkStartArgs) -> Result<(), String> {
    let goal = args.goal.join(" ");
    let criteria = args
        .done_when
        .into_iter()
        .enumerate()
        .map(|(index, statement)| WorkCreateCriterionV1::HumanReview {
            criterion_id: format!("done-when-{:02}", index + 1),
            statement,
        })
        .collect();
    let observation = api
        .post_work(
            token,
            &WorkCreateRequestV1 {
                request_id: request_id("start"),
                goal: goal.clone(),
                criteria,
            },
        )
        .await
        .map_err(|error| format!("Start Work failed: {error}"))?;
    let (work_id, branch_id) = work_identity(&observation, None)?;
    eprintln!("Work {work_id} · branch {branch_id}");
    run_work_turn(api, token, work_id, branch_id, &goal).await?;
    print_current_work(api, token, work_id, branch_id, false).await
}

async fn show_work(api: &ThinClient, token: &str, args: WorkShowArgs) -> Result<(), String> {
    let observation = load_work(api, token, &args.work_id).await?;
    let (_, branch_id) = work_identity(&observation, args.branch.as_deref())?;
    let graph = load_task_graph(api, token, &args.work_id, branch_id).await?;
    if args.json {
        stdout_println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "work": observation,
                "task_graph": graph,
            }))
            .map_err(|error| format!("Unable to encode Work output: {error}"))?
        );
    } else {
        stdout_println!("{}", render_work_snapshot(&observation, &graph)?);
    }
    Ok(())
}

async fn continue_work(
    api: &ThinClient,
    token: &str,
    args: WorkContinueArgs,
) -> Result<(), String> {
    let observation = load_work(api, token, &args.work_id).await?;
    let (_, branch_id) = work_identity(&observation, args.branch.as_deref())?;
    run_work_turn(
        api,
        token,
        &args.work_id,
        branch_id,
        &args.message.join(" "),
    )
    .await?;
    print_current_work(api, token, &args.work_id, branch_id, false).await
}

async fn print_current_work(
    api: &ThinClient,
    token: &str,
    work_id: &str,
    branch_id: &str,
    json_output: bool,
) -> Result<(), String> {
    show_work(
        api,
        token,
        WorkShowArgs {
            work_id: work_id.to_string(),
            branch: Some(branch_id.to_string()),
            json: json_output,
        },
    )
    .await
}

async fn run_work_turn(
    api: &ThinClient,
    token: &str,
    work_id: &str,
    branch_id: &str,
    message: &str,
) -> Result<(), String> {
    let attachment = api
        .post_work_branch_attachment(
            token,
            work_id,
            branch_id,
            &WorkBranchAttachRequestV1 {
                request_id: request_id("attach"),
            },
        )
        .await
        .map_err(|error| format!("Unable to attach to Work {work_id}: {error}"))?;
    let attachment_id = required_str(&attachment, "/attachment_id", "attachment_id")?;
    let response = api
        .post_work_branch_turn(
            token,
            work_id,
            branch_id,
            &WorkTurnRequestV1 {
                request_id: request_id("turn"),
                attachment_id: attachment_id.to_string(),
                message: message.to_string(),
            },
        )
        .await
        .map_err(|error| format!("Unable to continue Work {work_id}: {error}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let _ = api
            .delete_work_branch_attachment(token, work_id, branch_id, attachment_id)
            .await;
        return Err(format!("Work turn rejected ({status}): {body}"));
    }
    let result = stream_sse_markdown(response).await;
    let release = release_work_controller(api, token, work_id, branch_id, attachment_id).await;
    if let Err(release_error) = release {
        if let Some(turn_error) = result.completion_error() {
            return Err(format!(
                "{turn_error}; controller release also failed: {release_error}"
            ));
        }
        return Err(release_error);
    }
    if let Some(error) = result.completion_error() {
        return Err(error);
    }
    if result
        .event_types
        .iter()
        .any(|event| WORK_STREAM_ERROR_EVENTS.contains(&event.as_str()))
    {
        return Err("Work turn ended with a server-reported error".to_string());
    }
    Ok(())
}

async fn release_work_controller(
    api: &ThinClient,
    token: &str,
    work_id: &str,
    branch_id: &str,
    controller_attachment_id: &str,
) -> Result<(), String> {
    let observer = api
        .post_work_branch_attachment(
            token,
            work_id,
            branch_id,
            &WorkBranchAttachRequestV1 {
                request_id: request_id("release-basis"),
            },
        )
        .await
        .map_err(|error| format!("Unable to refresh Work controller basis: {error}"))?;
    let observer_attachment_id =
        required_str(&observer, "/attachment_id", "release observer attachment")?.to_string();
    let release_result = async {
        let branch_revision = required_i64(&observer, "/branch_revision", "branch revision")?;
        let writer_epoch = required_u64(
            &observer,
            "/control_basis/writer_epoch",
            "controller writer epoch",
        )?;
        let canonical_root_hash = optional_str(
            &observer,
            "/control_basis/canonical_root_hash",
            "controller root hash",
        )?;
        let operation = api
            .post_work_branch_control_operation(
                token,
                work_id,
                branch_id,
                &WorkBranchControlOperationRequestV1 {
                    request_id: request_id("release"),
                    expected_branch_revision: branch_revision,
                    expected_writer_epoch: writer_epoch,
                    expected_canonical_root_hash: canonical_root_hash.map(str::to_string),
                    command: WorkBranchControlCommandV1::ReleaseBranchControl {
                        attachment_id: controller_attachment_id.to_string(),
                    },
                },
            )
            .await
            .map_err(|error| format!("Unable to release Work controller: {error}"))?;
        let state = required_str(&operation, "/state", "control operation state")?;
        let outcome = required_str(&operation, "/outcome", "control operation outcome")?;
        if state != "succeeded" || !matches!(outcome, "released" | "already_released") {
            return Err(format!(
                "Work controller release did not succeed: {state}/{outcome}"
            ));
        }
        api.delete_work_branch_attachment(token, work_id, branch_id, controller_attachment_id)
            .await
            .map_err(|error| format!("Unable to detach released Work controller: {error}"))
    }
    .await;
    let observer_cleanup = api
        .delete_work_branch_attachment(token, work_id, branch_id, &observer_attachment_id)
        .await
        .map_err(|error| format!("Unable to detach Work release observer: {error}"));
    match (release_result, observer_cleanup) {
        (Err(release), Err(cleanup)) => Err(format!("{release}; {cleanup}")),
        (Err(release), Ok(())) => Err(release),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn load_work(api: &ThinClient, token: &str, work_id: &str) -> Result<Value, String> {
    api.get_work(token, work_id)
        .await
        .map_err(|error| format!("Unable to read Work {work_id}: {error}"))
}

async fn load_task_graph(
    api: &ThinClient,
    token: &str,
    work_id: &str,
    branch_id: &str,
) -> Result<WorkTaskGraphPageV2, String> {
    let mut snapshot = api
        .get_work_branch_task_graph_page(token, work_id, branch_id, None, 0, 0)
        .await
        .map_err(|error| format!("Unable to read Task Graph for Work {work_id}: {error}"))?;
    let graph_revision = snapshot.basis.graph_revision;
    let item_total = snapshot.items.total;
    let dependency_total = snapshot.dependencies.total;
    let mut items = std::mem::take(&mut snapshot.items.entries);
    let mut dependencies = std::mem::take(&mut snapshot.dependencies.entries);
    let mut next = snapshot.next_cursor.take();
    let mut seen = BTreeSet::new();
    for _ in 0..WORK_TASK_GRAPH_MAX_PAGES {
        let Some(cursor) = next else {
            break;
        };
        if cursor.graph_revision != graph_revision || !seen.insert(cursor) {
            return Err("Task Graph pagination returned a stale or repeated cursor".to_string());
        }
        let page = api
            .get_work_branch_task_graph_page(
                token,
                work_id,
                branch_id,
                Some(cursor.graph_revision),
                cursor.item_offset,
                cursor.dependency_offset,
            )
            .await
            .map_err(|error| format!("Unable to continue Task Graph read: {error}"))?;
        if page.basis.graph_revision != graph_revision
            || page.cursor != cursor
            || page.items.total != item_total
            || page.dependencies.total != dependency_total
            || page.basis.graph_manifest_hash != snapshot.basis.graph_manifest_hash
        {
            return Err("Task Graph continuation did not match its pinned cursor".to_string());
        }
        items.extend(page.items.entries);
        dependencies.extend(page.dependencies.entries);
        next = page.next_cursor;
    }
    if next.is_some() {
        return Err(format!(
            "Task Graph exceeded the bounded {WORK_TASK_GRAPH_MAX_PAGES}-page client budget"
        ));
    }
    if items.len() != usize::from(item_total) || dependencies.len() != usize::from(dependency_total)
    {
        return Err("Task Graph pagination ended before its declared totals".to_string());
    }
    snapshot.items.entries = items;
    snapshot.dependencies.entries = dependencies;
    snapshot.next_cursor = None;
    Ok(snapshot)
}

fn work_identity<'a>(
    observation: &'a Value,
    branch_override: Option<&'a str>,
) -> Result<(&'a str, &'a str), String> {
    let work_id = required_str(observation, "/overview/work_id", "Work identity")?;
    let branch_id = match branch_override {
        Some(branch_id) if !branch_id.is_empty() => branch_id,
        Some(_) => return Err("Work branch identity cannot be empty".to_string()),
        None => required_str(
            observation,
            "/overview/delivery_branch/branch_id",
            "delivery branch identity",
        )?,
    };
    Ok((work_id, branch_id))
}

fn render_work_snapshot(
    observation: &Value,
    graph: &WorkTaskGraphPageV2,
) -> Result<String, String> {
    let (work_id, branch_id) = work_identity(observation, None)?;
    let goal = required_str(observation, "/overview/goal/goal", "Work goal")?;
    let delivery = required_str(
        observation,
        "/overview/delivery/status",
        "Work delivery status",
    )?;
    let graph_revision = graph.basis.graph_revision;
    let items = &graph.items.entries;
    let dependencies = &graph.dependencies.entries;

    let mut lines = vec![
        format!("Work {work_id}"),
        format!("Goal: {goal}"),
        format!("Status: {delivery} · branch {branch_id} · graph r{graph_revision}"),
        String::new(),
        "Plan".to_string(),
    ];
    for item in items {
        let (marker, state) = work_item_presentation(item);
        lines.push(format!(
            "  {marker} {}  [{} · {state}]",
            item.objective, item.item_id
        ));
        if matches!(
            item.delivery.status,
            WorkItemDeliveryStatusV2::Blocked | WorkItemDeliveryStatusV2::Failed
        ) {
            if let Some(summary) = item.delivery.summary.as_deref() {
                lines.push(format!("      {summary}"));
            }
            if !item.delivery.unavailable_capabilities.is_empty() {
                lines.push(format!(
                    "      Unavailable: {}",
                    item.delivery.unavailable_capabilities.join(", ")
                ));
            }
        }
    }
    if !dependencies.is_empty() {
        lines.push(String::new());
        lines.push("Dependencies".to_string());
        for dependency in dependencies {
            lines.push(format!(
                "  {} → {}",
                dependency.predecessor_item_id, dependency.successor_item_id
            ));
        }
    }
    Ok(lines.join("\n"))
}

fn work_item_presentation(
    item: &astra_thin_client::WorkTaskGraphItemV2,
) -> (&'static str, &'static str) {
    match item.declaration_state {
        WorkTaskDeclarationStateV2::Cancelled => return ("×", "Cancelled"),
        WorkTaskDeclarationStateV2::Superseded => return ("↺", "Replaced"),
        WorkTaskDeclarationStateV2::Active => {}
    }
    match item.delivery.status {
        WorkItemDeliveryStatusV2::Blocked => return ("!", "Blocked"),
        WorkItemDeliveryStatusV2::Failed => return ("!", "Failed"),
        WorkItemDeliveryStatusV2::Unreported | WorkItemDeliveryStatusV2::Delivered => {}
    }
    match item.execution.status {
        WorkItemExecutionStatusV2::Running | WorkItemExecutionStatusV2::Delegated => {
            ("●", "Running")
        }
        WorkItemExecutionStatusV2::Waiting => ("!", "Waiting"),
        WorkItemExecutionStatusV2::Paused => ("!", "Paused"),
        WorkItemExecutionStatusV2::Failed => ("!", "Failed"),
        WorkItemExecutionStatusV2::Cancelled => ("!", "Cancelled"),
        WorkItemExecutionStatusV2::NotStarted => ("○", "Planned"),
        WorkItemExecutionStatusV2::Completed
            if item.delivery.status == WorkItemDeliveryStatusV2::Unreported =>
        {
            ("!", "Result not reported")
        }
        WorkItemExecutionStatusV2::Completed => {
            let current_passed = item.verification.status
                == WorkItemVerificationStatusV2::EvidenceAvailable
                && item
                    .verification
                    .latest_check
                    .as_ref()
                    .is_some_and(|check| {
                        check.freshness == WorkTaskCheckFreshnessV2::Current
                            && check.outcome == WorkTaskCheckOutcomeV2::Passed
                    });
            if current_passed {
                ("✓", "Checked")
            } else if item.verification.status == WorkItemVerificationStatusV2::StaleEvidence {
                ("!", "Needs recheck")
            } else {
                // A delivered Work item is complete from the execution
                // contract's perspective. Verification is an independent
                // evidence projection; when no criterion/check exists it
                // must not turn a finished task into an attention item.
                ("✓", "Completed")
            }
        }
    }
}

fn required_str<'a>(value: &'a Value, pointer: &str, label: &str) -> Result<&'a str, String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("Work response is missing {label}"))
}

fn required_i64(value: &Value, pointer: &str, label: &str) -> Result<i64, String> {
    value
        .pointer(pointer)
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("Work response is missing {label}"))
}

fn required_u64(value: &Value, pointer: &str, label: &str) -> Result<u64, String> {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("Work response is missing {label}"))
}

fn optional_str<'a>(
    value: &'a Value,
    pointer: &str,
    label: &str,
) -> Result<Option<&'a str>, String> {
    match value.pointer(pointer) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value)),
        _ => Err(format!("Work response has an invalid {label}")),
    }
}

fn request_id(kind: &str) -> String {
    format!("cli-{kind}-{}", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{
        body_partial_json, body_string_contains, header, method, path, query_param,
    };
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn observation() -> Value {
        json!({
            "overview": {
                "work_id": "work-1",
                "goal": {"goal": "Ship the feature"},
                "delivery_branch": {"branch_id": "branch-1"},
                "delivery": {"status": "needs_verification"}
            }
        })
    }

    fn graph() -> WorkTaskGraphPageV2 {
        let page: WorkTaskGraphPageV2 = serde_json::from_str(include_str!(
            "../../../../fixtures/contracts/work_task_graph_v2.json"
        ))
        .expect("shared Task Graph fixture");
        page.validate().expect("valid shared Task Graph fixture");
        page
    }

    fn graph_continuation(graph_revision: i64) -> WorkTaskGraphPageV2 {
        let mut value = serde_json::to_value(graph()).expect("encode fixture");
        value["basis"]["graph_revision"] = json!(graph_revision);
        value["basis"]["graph_manifest_hash"] = json!(format!(
            "sha256:{}",
            if graph_revision == 1 {
                "b".repeat(64)
            } else {
                "d".repeat(64)
            }
        ));
        value["cursor"] = json!({
            "graph_revision": graph_revision,
            "item_offset": 1,
            "dependency_offset": 1
        });
        value["next_cursor"] = Value::Null;
        value["items"]["offset"] = json!(1);
        value["items"]["entries"] = json!([{
            "item_id": "task-b",
            "revision": 1,
            "kind": "task",
            "objective": "Implement task-b",
            "expected_result": "Verify task-b",
            "declaration_state": "active",
            "execution": {"status": "not_started", "terminal": false, "run": null},
            "delivery": {"status": "unreported", "summary": null, "blocker_kind": null, "unavailable_capabilities": []},
            "verification": {"status": "unknown", "latest_check": null}
        }]);
        value["dependencies"]["offset"] = json!(1);
        value["dependencies"]["entries"] = json!([]);
        let page: WorkTaskGraphPageV2 = serde_json::from_value(value).expect("continuation");
        page.validate().expect("valid Task Graph continuation");
        page
    }

    fn complete_graph() -> WorkTaskGraphPageV2 {
        let mut page = graph();
        let continuation = graph_continuation(1);
        page.items.limit = 8;
        page.items.entries.extend(continuation.items.entries);
        page.next_cursor = None;
        page.validate().expect("valid complete Task Graph");
        page
    }

    #[test]
    fn snapshot_renders_delivery_and_verification_without_false_completion() {
        let observation = observation();
        let mut graph = complete_graph();

        let rendered = render_work_snapshot(&observation, &graph).expect("snapshot");
        assert!(rendered.contains("✓ Implement task-a  [task-a · Checked]"));
        assert!(rendered.contains("task-a → task-b"));

        // Delivery is the terminal execution fact for a Work item when no
        // durable verification evidence exists. The CLI must agree with the
        // TUI and avoid inventing a review obligation from `unknown` alone.
        let completed_without_check = graph.items.entries[0].clone();
        graph.items.entries[1].execution = completed_without_check.execution;
        graph.items.entries[1].delivery = completed_without_check.delivery;
        graph.items.entries[1].verification = astra_thin_client::work::WorkTaskVerificationV2 {
            status: WorkItemVerificationStatusV2::Unknown,
            latest_check: None,
        };
        graph
            .validate()
            .expect("coherent delivered task without check");
        let rendered = render_work_snapshot(&observation, &graph).expect("snapshot");
        assert!(rendered.contains("✓ Implement task-b  [task-b · Completed]"));
        assert!(!rendered.contains("Delivered · review"));

        let mut blocked = graph;
        blocked.items.entries[0].delivery.status = WorkItemDeliveryStatusV2::Blocked;
        blocked.items.entries[0].delivery.summary = Some("Network access is unavailable".into());
        blocked.items.entries[0].delivery.blocker_kind =
            Some(astra_thin_client::WorkItemDeliveryBlockerKindV2::CapabilityUnavailable);
        blocked.items.entries[0].delivery.unavailable_capabilities = vec!["web_fetch".into()];
        blocked.items.entries[0].verification.status = WorkItemVerificationStatusV2::Unknown;
        blocked.items.entries[0].verification.latest_check = None;
        blocked.validate().expect("coherent blocked Task");
        let rendered = render_work_snapshot(&observation, &blocked).expect("blocked snapshot");
        assert!(rendered.contains("! Implement task-a  [task-a · Blocked]"));
        assert!(rendered.contains("Network access is unavailable"));
        assert!(rendered.contains("Unavailable: web_fetch"));
    }

    #[test]
    fn snapshot_fails_closed_on_incomplete_projection() {
        let error = render_work_snapshot(&json!({}), &complete_graph()).unwrap_err();
        assert!(error.contains("Work identity"));
    }

    #[tokio::test]
    async fn start_work_uses_one_server_owned_loop_then_refreshes_the_task_graph() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/works"))
            .and(header("authorization", "Bearer token"))
            .and(body_partial_json(json!({
                "goal": "Ship the feature",
                "criteria": [{
                    "kind": "human_review",
                    "criterion_id": "done-when-01",
                    "statement": "The user accepts the result"
                }]
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(observation()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/works/work-1/branches/branch-1/attachments"))
            .and(body_string_contains("\"request_id\":\"cli-attach-"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "attachment_id": "attachment-1"
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/works/work-1/branches/branch-1/turns"))
            .and(body_partial_json(json!({
                "attachment_id": "attachment-1",
                "message": "Ship the feature"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string(concat!(
                "data: {\"type\":\"text_delta\",\"content\":\"Working.\"}\n\n",
                "data: {\"type\":\"run_finished\",\"status\":\"completed\"}\n\n"
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/works/work-1/branches/branch-1/attachments"))
            .and(body_string_contains("\"request_id\":\"cli-release-basis-"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "attachment_id": "release-observer-1",
                "branch_revision": 2,
                "control_basis": {
                    "writer_epoch": 7,
                    "canonical_root_hash": "sha256:root"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(
                "/v1/works/work-1/branches/branch-1/control-operations",
            ))
            .and(body_partial_json(json!({
                "expected_branch_revision": 2,
                "expected_writer_epoch": 7,
                "expected_canonical_root_hash": "sha256:root",
                "command": {
                    "kind": "release_branch_control",
                    "attachment_id": "attachment-1"
                }
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "state": "succeeded",
                "outcome": "released"
            })))
            .expect(1)
            .mount(&server)
            .await;
        for attachment_id in ["attachment-1", "release-observer-1"] {
            Mock::given(method("DELETE"))
                .and(path(format!(
                    "/v1/works/work-1/branches/branch-1/attachments/{attachment_id}"
                )))
                .respond_with(ResponseTemplate::new(204))
                .expect(1)
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/v1/works/work-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(observation()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/works/work-1/branches/branch-1/task-graph"))
            .respond_with(ResponseTemplate::new(200).set_body_json(complete_graph()))
            .expect(1)
            .mount(&server)
            .await;

        let api = ThinClient::new(&server.uri(), None).expect("client");
        start_work(
            &api,
            "token",
            WorkStartArgs {
                done_when: vec!["The user accepts the result".to_string()],
                goal: vec!["Ship".into(), "the".into(), "feature".into()],
            },
        )
        .await
        .expect("Start Work journey");
    }

    #[tokio::test]
    async fn missing_attachment_identity_fails_before_a_turn_is_submitted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/works/work-1/branches/branch-1/attachments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "attachment_epoch": 1
            })))
            .expect(1)
            .mount(&server)
            .await;

        let api = ThinClient::new(&server.uri(), None).expect("client");
        let error = run_work_turn(
            &api,
            "token",
            "work-1",
            "branch-1",
            "Do not submit this turn",
        )
        .await
        .expect_err("missing attachment identity must fail closed");
        assert!(error.contains("attachment_id"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn task_graph_pages_are_aggregated_at_one_pinned_revision() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/works/work-1/branches/branch-1/task-graph"))
            .respond_with(ResponseTemplate::new(200).set_body_json(graph()))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/works/work-1/branches/branch-1/task-graph"))
            .and(query_param("graph_revision", "1"))
            .and(query_param("item_offset", "1"))
            .and(query_param("dependency_offset", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(graph_continuation(1)))
            .expect(1)
            .mount(&server)
            .await;

        let api = ThinClient::new(&server.uri(), None).expect("client");
        let graph = load_task_graph(&api, "token", "work-1", "branch-1")
            .await
            .expect("pinned graph");
        assert_eq!(graph.items.entries.len(), 2);
        assert!(graph.next_cursor.is_none());
    }

    #[tokio::test]
    async fn task_graph_pagination_rejects_a_replan_between_pages() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/works/work-1/branches/branch-1/task-graph"))
            .respond_with(ResponseTemplate::new(200).set_body_json(graph()))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/works/work-1/branches/branch-1/task-graph"))
            .and(query_param("graph_revision", "1"))
            .and(query_param("item_offset", "1"))
            .and(query_param("dependency_offset", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(graph_continuation(2)))
            .mount(&server)
            .await;

        let api = ThinClient::new(&server.uri(), None).expect("client");
        let error = load_task_graph(&api, "token", "work-1", "branch-1")
            .await
            .expect_err("mixed graph revisions must fail closed");
        assert!(error.contains("pinned cursor"), "{error}");
    }
}
