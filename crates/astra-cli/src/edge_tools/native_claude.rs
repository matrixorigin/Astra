//! Claude Code's structured CLI adapter.
//!
//! The canonical child-run, Edge delivery, permission receipt, process owner,
//! cancellation and result journal remain in their existing owners. This
//! module only translates Claude's newline-delimited `stream-json` events to
//! the native collaborator result contract.

use super::{ApprovedNativeRuntime, ToolExecutor, native_codex};
use astra_edge::EdgeInvocationInput;
use astra_sandbox::{FramedProcess, FramedProcessEnd, FramedProcessLimits};
use astra_tools::{ProviderInteractionGate, ToolResult};
use astra_turn_types::{ProviderRuntimeRequirements, ProviderStageInputAck};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const TOOL_NAME: &str = "native_claude";
const PROVIDER_NAME: &str = "claude-code";
const PROTOCOL_NAME: &str = "claude-stream-json";
const EXECUTABLE_NAMES: &[&str] = if cfg!(windows) {
    &[
        "claude.exe",
        "claude.cmd",
        "claude.bat",
        "claude",
        "claude-code.exe",
        "claude-code",
    ]
} else {
    &["claude", "claude-code"]
};

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Stage {
    task: String,
    anchor_run_id: String,
    native_session_id: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

impl Stage {
    fn parse(args: &Value) -> Result<Self, &'static str> {
        let stage: Self = serde_json::from_value(args.clone())
            .map_err(|_| "invalid native Claude stage arguments")?;
        let valid = |value: &str| {
            !value.is_empty()
                && value.len() <= 256
                && value.trim() == value
                && !value.chars().any(char::is_control)
        };
        if stage.task.trim().is_empty()
            || stage.task.len() > native_codex::OUTPUT_BYTES
            || !valid(&stage.anchor_run_id)
            || stage
                .native_session_id
                .as_deref()
                .is_some_and(|id| !valid(id))
            || stage.model.as_deref().is_some_and(|model| !valid(model))
            || stage
                .effort
                .as_deref()
                .is_some_and(|effort| !matches!(effort, "none" | "low" | "medium" | "high" | "max"))
        {
            return Err("invalid native Claude stage arguments");
        }
        if stage.effort.as_deref() == Some("none") {
            return Err("native Claude does not support disabling reasoning; omit effort");
        }
        Ok(stage)
    }
}

pub fn schema() -> Value {
    let mut schema = native_codex::schema();
    schema["function"]["name"] = json!(TOOL_NAME);
    schema["function"]["description"] = json!(
        "Execute one admitted native Claude Code collaborator stage in the selected CLI workspace. The model is passed to Claude Code as an explicit provider argument; the provider's acknowledged session and result establish what actually ran. Resume only an exact acknowledged native_session_id. Run, control and deadline authority comes from the invocation, never arguments."
    );
    schema["function"]["parameters"]["properties"]["effort"]["enum"] =
        json!(["low", "medium", "high", "max"]);
    schema
}

pub(crate) fn provider_declaration(
    requirements: ProviderRuntimeRequirements,
) -> Result<astra_turn_types::ProviderToolDeclaration, astra_turn_types::ProviderContractError> {
    let schema = schema();
    let mut extension_fields = serde_json::Map::new();
    extension_fields.insert(
        astra_turn_types::PROVIDER_RUNTIME_REQUIREMENTS_KEY.into(),
        json!(requirements),
    );
    extension_fields.insert(
        astra_turn_types::PROVIDER_COLLABORATOR_STAGE_KEY.into(),
        json!(true),
    );
    extension_fields.insert(
        astra_turn_core::provider_resolution::NativeCollaboratorProtocol::EXTENSION_KEY.into(),
        json!(
            astra_turn_core::provider_resolution::NativeCollaboratorProtocol::ClaudeStreamJson
                .extension_value()
        ),
    );
    astra_turn_types::ProviderRuntimeRequirements::from_extension_fields(&extension_fields)?;
    let declaration = astra_turn_types::ProviderToolDeclaration {
        native_tool_id: astra_turn_types::NativeToolId::new(TOOL_NAME)?,
        native_tool_name: TOOL_NAME.into(),
        stable_tool_alias: Some(astra_turn_types::StableToolAlias::new(TOOL_NAME)?),
        title: Some("Native Claude Code collaborator".into()),
        description: schema["function"]["description"]
            .as_str()
            .map(str::to_owned),
        input_schema: schema["function"]["parameters"].clone(),
        output_schema: None,
        claims: Default::default(),
        task_support: astra_turn_types::ProviderTaskSupport::Required,
        extension_fields,
    };
    declaration.validate()?;
    Ok(declaration)
}

pub(crate) fn executable_candidates() -> Vec<std::path::PathBuf> {
    native_codex::native_executable_candidates_for_names(EXECUTABLE_NAMES)
}

pub(crate) fn executable_snapshot() -> Vec<native_codex::NativeExecutableIdentity> {
    native_codex::native_executable_snapshot_for_names(EXECUTABLE_NAMES)
}

pub(crate) fn installed_runtime_requirements() -> Result<ProviderRuntimeRequirements, &'static str>
{
    let executable = executable_candidates()
        .into_iter()
        .next()
        .ok_or("Claude Code executable is unavailable")?;
    native_codex::runtime_requirements_for_executable(&executable)
}

/// Discovery is deliberately non-invasive. `--version` proves that the
/// selected executable can be started; authentication and model readiness are
/// established only by the real admitted stage, where their result can be
/// associated with the canonical child run.
pub(crate) async fn verify_installed_protocol(
    executable: &std::path::Path,
    cwd: &std::path::Path,
    cancel: &CancellationToken,
    deadline: std::time::Instant,
) -> Result<(), &'static str> {
    let timeout = deadline
        .saturating_duration_since(std::time::Instant::now())
        .min(Duration::from_secs(2));
    if timeout.is_zero() {
        return Err("Claude Code capability probe deadline expired");
    }
    let (mut command, owner) =
        native_codex::prepare_native_process(executable, &["--version".into()])
            .map_err(|_| "Claude Code process ownership unavailable")?;
    command.current_dir(cwd);
    let mut process = owner
        .spawn_framed(
            command,
            FramedProcessLimits {
                max_frame_bytes: native_codex::FRAME_BYTES,
                max_queued_frames: 2,
                max_stderr_bytes: 4096,
                timeout,
            },
            cancel.child_token(),
        )
        .map_err(|_| "Claude Code capability probe unavailable")?;
    let frame = tokio::time::timeout(timeout, process.recv_frame())
        .await
        .ok()
        .flatten();
    let outcome = process
        .cancel_and_wait()
        .await
        .map_err(|_| "Claude Code capability probe settlement unavailable")?;
    let settled = outcome
        .settlement
        .as_ref()
        .is_some_and(|settlement| settlement.ownership.is_authoritative());
    if !settled
        || !matches!(
            outcome.end,
            FramedProcessEnd::Exited | FramedProcessEnd::Cancelled
        )
        || frame.as_deref().is_none_or(|frame| frame.is_empty())
    {
        return Err("Claude Code capability probe did not settle");
    }
    Ok(())
}

#[derive(Default)]
struct Evidence {
    session_id: Option<String>,
    model: Option<String>,
    output: String,
    final_output: Option<String>,
    terminal: bool,
    provider_error: Option<&'static str>,
    provider_error_class: Option<&'static str>,
    usage: Option<Value>,
    cost_usd: Option<f64>,
    output_capped: bool,
    capability_unavailable: bool,
}

/// Claude's event protocol supplies an error enum in this field. Only exact
/// enum values may change capability state; free-form provider text remains a
/// generic provider error and can never be mistaken for an auth/model signal.
fn classify_provider_error(value: &str) -> &'static str {
    match value {
        "authentication_failed" | "auth_failed" => "authentication",
        "model_not_found" | "model_unavailable" => "model",
        "billing_error" | "quota_exceeded" => "billing",
        "permission_denied" => "permission",
        _ => "provider",
    }
}

fn provider_error_message(class: &'static str) -> &'static str {
    match class {
        "authentication" => "Claude Code authentication is required",
        "model" => "Claude Code model is unavailable",
        "billing" => "Claude Code billing or quota is unavailable",
        "permission" => "Claude Code rejected the requested operation",
        _ => "Claude Code reported a provider error",
    }
}

fn record_provider_error(evidence: &mut Evidence, value: &str) {
    let class = classify_provider_error(value);
    evidence.provider_error_class = Some(class);
    evidence.provider_error = Some(provider_error_message(class));
    evidence.capability_unavailable |= matches!(class, "authentication" | "model" | "billing");
}

fn claude_cli_args(read_only: bool) -> Vec<String> {
    let tools = if read_only {
        "Read,Glob,Grep"
    } else {
        // Claude's native file tools remain useful for implementation, while
        // command, MCP and network authority stays with Astra's owners.
        "Read,Edit,Write,Glob,Grep"
    };
    vec![
        "--print".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--include-partial-messages".into(),
        // `allowedTools` only pre-approves tools; it does not prevent a
        // project/user settings file from making more tools available.
        // Restricted mode ignores those settings and `--tools` is the actual
        // availability boundary for this invocation.
        "--restricted".into(),
        "--strict-mcp-config".into(),
        "--tools".into(),
        tools.into(),
        "--permission-mode".into(),
        if read_only {
            "dontAsk".into()
        } else {
            "acceptEdits".into()
        },
    ]
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn append_text(target: &mut String, text: &str, limit: usize, capped: &mut bool) {
    let remaining = limit.saturating_sub(target.len());
    let mut keep = text.len().min(remaining);
    while keep > 0 && !text.is_char_boundary(keep) {
        keep -= 1;
    }
    target.push_str(&text[..keep]);
    *capped |= keep < text.len();
}

fn message_text(message: &Value, target: &mut String, limit: usize, capped: &mut bool) {
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = block.get("text").and_then(Value::as_str)
        {
            append_text(target, text, limit, capped);
        }
    }
}

fn ingest(frame: &[u8], evidence: &mut Evidence, output_limit: usize) -> Result<(), &'static str> {
    let event: Value =
        serde_json::from_slice(frame).map_err(|_| "invalid Claude stream-json event")?;
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .ok_or("Claude stream-json event type is missing")?;
    match event_type {
        "system" => {
            if event.get("subtype").and_then(Value::as_str) == Some("init") {
                let session_id = event
                    .get("session_id")
                    .and_then(Value::as_str)
                    .filter(|id| valid_id(id))
                    .ok_or("Claude init event has no valid session_id")?;
                if evidence
                    .session_id
                    .as_deref()
                    .is_some_and(|known| known != session_id)
                {
                    return Err("Claude session identity changed during a stage");
                }
                evidence.session_id = Some(session_id.to_owned());
                if let Some(model) = event.get("model").and_then(Value::as_str) {
                    if !valid_id(model) {
                        return Err("Claude init event has an invalid model");
                    }
                    evidence.model = Some(model.to_owned());
                }
            }
            if let Some(error) = event.get("error").and_then(Value::as_str) {
                record_provider_error(evidence, error);
            }
        }
        "assistant" => {
            message_text(
                &event["message"],
                &mut evidence.output,
                output_limit,
                &mut evidence.output_capped,
            );
            if let Some(error) = event.get("error").and_then(Value::as_str) {
                record_provider_error(evidence, error);
            }
        }
        "stream_event" => {
            if event.pointer("/event/delta/type").and_then(Value::as_str) == Some("text_delta")
                && let Some(text) = event.pointer("/event/delta/text").and_then(Value::as_str)
            {
                append_text(
                    &mut evidence.output,
                    text,
                    output_limit,
                    &mut evidence.output_capped,
                );
            }
        }
        "result" => {
            evidence.terminal = true;
            if let Some(session_id) = event.get("session_id").and_then(Value::as_str) {
                if !valid_id(session_id)
                    || evidence
                        .session_id
                        .as_deref()
                        .is_some_and(|known| known != session_id)
                {
                    return Err("Claude result session identity does not match init");
                }
                evidence.session_id = Some(session_id.to_owned());
            }
            if let Some(result) = event.get("result").and_then(Value::as_str) {
                let mut final_output = String::new();
                append_text(
                    &mut final_output,
                    result,
                    output_limit,
                    &mut evidence.output_capped,
                );
                evidence.final_output = Some(final_output);
            }
            if event.get("is_error").and_then(Value::as_bool) == Some(true) {
                let value = event
                    .get("error")
                    .and_then(Value::as_str)
                    .or_else(|| event.get("subtype").and_then(Value::as_str))
                    .unwrap_or("provider");
                record_provider_error(evidence, value);
            }
            if let Some(usage) = event.get("usage") {
                evidence.usage = Some(usage.clone());
            }
            evidence.cost_usd = event.get("total_cost_usd").and_then(Value::as_f64);
        }
        _ => {}
    }
    Ok(())
}

fn canonical_usage(value: &Value) -> Option<astra_turn_types::CanonicalTokenUsage> {
    let input = value.get("input_tokens").and_then(Value::as_u64);
    let cached = value.get("cache_read_input_tokens").and_then(Value::as_u64);
    let creation = value
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);
    let output = value.get("output_tokens").and_then(Value::as_u64);
    astra_turn_types::CanonicalTokenUsage::new(input, cached, creation, output).ok()
}

async fn drive(
    process: &mut FramedProcess,
    evidence: &mut Evidence,
    output_limit: usize,
    cancel: &CancellationToken,
    mut input_rx: Option<tokio::sync::mpsc::Receiver<EdgeInvocationInput>>,
) -> Result<(), String> {
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("native Claude invocation cancelled".into()),
            frame = process.recv_frame() => frame.map(|frame| Ok::<Vec<u8>, String>(frame)),
            input = async {
                match input_rx.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => None,
                }
            }, if input_rx.is_some() => {
                if let Some(input) = input {
                    let _ = input.ack.send(ProviderStageInputAck::rejected(
                        &input.input,
                        "Claude Code accepts follow-up text at the next collaborator stage, not during this CLI turn",
                    ));
                } else {
                    input_rx = None;
                }
                continue;
            }
        };
        let Some(frame) = next else {
            return Err("Claude stream ended without a result event".into());
        };
        ingest(&frame?, evidence, output_limit).map_err(str::to_owned)?;
        if evidence.terminal {
            return Ok(());
        }
    }
}

fn failure(reason: &'static str) -> ToolResult {
    ToolResult {
        output: format!("Error: {reason}"),
        is_error: true,
        metadata: Some(
            json!({
                "native_collaborator": {
                    "provider": PROVIDER_NAME,
                    "protocol": PROTOCOL_NAME,
                    "dispatch_state": "not_dispatched",
                    "native_session_id": null,
                    "native_turn_id": null,
                    "session_acknowledged": false,
                    "turn_acknowledged": false,
                    "target_released": false,
                    "usage_snapshot": null,
                    "cost_usd": null
                },
                "workspace_effect_settled": true
            })
            .as_object()
            .expect("native failure metadata is an object")
            .clone(),
        ),
        exit_semantics: None,
    }
}

#[allow(clippy::too_many_arguments)]
impl ToolExecutor {
    pub(super) async fn execute_native_claude(
        &self,
        args: &Value,
        invocation: astra_tools::tool_engine::ToolInvocationMetadata<'_>,
        cancel: Option<&CancellationToken>,
        execution_root: &std::path::Path,
        gate: Option<&dyn ProviderInteractionGate>,
        execution_ceiling: Option<&astra_server_types::edge_ws_protocol::EdgeExecutionCeiling>,
        runtime_approval: Option<&ApprovedNativeRuntime>,
        input_rx: Option<tokio::sync::mpsc::Receiver<EdgeInvocationInput>>,
    ) -> ToolResult {
        let stage = match Stage::parse(args) {
            Ok(stage) => stage,
            Err(reason) => return failure(reason),
        };
        if gate.is_none() {
            return failure("canonical native interaction route is not connected");
        }
        let Some(ceiling) = execution_ceiling else {
            return failure("native Claude execution requires an immutable execution grant");
        };
        if invocation.run_id.filter(|id| !id.is_empty()).is_none()
            || invocation
                .tool_call_id
                .filter(|id| !id.is_empty())
                .is_none()
            || invocation.admission_source.is_none()
        {
            return failure(
                "native Claude execution requires canonical run and invocation admission",
            );
        }
        if cancel.is_some_and(CancellationToken::is_cancelled)
            || invocation
                .admission_deadline
                .is_some_and(|deadline| deadline <= std::time::Instant::now())
        {
            return failure("native Claude invocation admission cancelled or expired");
        }
        if native_codex::native_stage_remaining(invocation).is_err() {
            return failure("native Claude admitted stage budget expired before dispatch");
        }
        let mut policy = astra_core::sync_poison::recover_rwlock_read(&self.sandbox_policy).clone();
        if policy
            .as_ref()
            .is_some_and(|policy| policy.isolation == astra_sandbox::IsolationLevel::Strict)
        {
            return failure(
                "native Claude protocol cannot prove the selected strict isolation boundary",
            );
        }
        let cwd = match execution_root.canonicalize() {
            Ok(cwd) => cwd,
            Err(_) => return failure("native Claude workspace is unavailable"),
        };
        if std::path::Path::new(&ceiling.workspace_root) != cwd {
            return failure("native Claude execution grant does not match the selected workspace");
        }
        let requirements = match runtime_approval {
            Some(approved) => match native_codex::runtime_requirements_for_executable(
                std::path::Path::new(&approved.requirements.executable),
            ) {
                Ok(requirements) => requirements,
                Err(reason) => return failure(reason),
            },
            None => match installed_runtime_requirements() {
                Ok(requirements) => requirements,
                Err(reason) => return failure(reason),
            },
        };
        if let Some(approved) = runtime_approval {
            if approved.protocol
                != astra_turn_core::provider_resolution::NativeCollaboratorProtocol::ClaudeStreamJson
                || approved.requirements != requirements
                || approved.workspace_root != cwd
            {
                return failure("native Claude runtime approval does not match the selected provider");
            }
            let Some(local_policy) = policy.as_mut() else {
                return failure(
                    "native Claude runtime approval requires a selected local sandbox policy",
                );
            };
            local_policy.allowed_paths.extend(
                approved
                    .requirements
                    .read_paths
                    .iter()
                    .map(std::path::PathBuf::from),
            );
        }
        if native_codex::validate_runtime_grant(
            &ceiling.runtime_read_paths,
            &requirements,
            policy.as_ref(),
        )
        .is_err()
        {
            return failure("native Claude runtime grant does not match the installed provider");
        }
        let executable = std::path::Path::new(&requirements.executable);
        let read_only = !ceiling.workspace_write_allowed
            || self.read_only_execution
            || self.plan_mode_authoring_active().await;
        let mut args = claude_cli_args(read_only);
        if let Some(model) = &stage.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(effort) = &stage.effort
            && !matches!(effort.as_str(), "none" | "minimal")
        {
            args.extend(["--effort".into(), effort.clone()]);
        }
        if let Some(session_id) = &stage.native_session_id {
            args.extend(["--resume".into(), session_id.clone()]);
        }
        args.push(stage.task.clone());
        let Some(attribution) =
            astra_tools::workspace_observation::WorkspaceAttributionState::capture(&cwd)
        else {
            return failure("native Claude workspace attribution is unavailable");
        };
        let (mut command, owner) = match native_codex::prepare_native_process(executable, &args) {
            Ok(prepared) => prepared,
            Err(_) => return failure("native Claude structured process ownership is unavailable"),
        };
        command.current_dir(&cwd);
        if let Some(policy) = &mut policy {
            policy.project_root = cwd.clone();
            if astra_sandbox::sandbox_command(policy, &mut command).is_err() {
                return failure("native Claude sandbox preparation failed");
            }
        }
        let token = cancel.map_or_else(CancellationToken::new, CancellationToken::child_token);
        let timeout = match native_codex::native_stage_remaining(invocation) {
            Ok(timeout) => timeout,
            Err(reason) => return failure(reason),
        };
        let mut process = match owner.spawn_framed(
            command,
            FramedProcessLimits {
                max_frame_bytes: native_codex::FRAME_BYTES,
                max_queued_frames: 4,
                max_stderr_bytes: 4096,
                timeout,
            },
            token.clone(),
        ) {
            Ok(process) => process,
            Err(_) => return failure("native Claude structured process spawn failed"),
        };
        let mut unsettled = native_codex::UnsettledOnDrop(Some(attribution));
        let mut evidence = Evidence::default();
        let driven = tokio::select! {
            biased;
            _ = token.cancelled() => Err("native Claude invocation cancelled".to_string()),
            result = drive(&mut process, &mut evidence, native_codex::OUTPUT_BYTES, &token, input_rx) => result,
        };
        let (driven, settlement) =
            native_codex::settle_native_process(process, driven, &token, &mut unsettled).await;
        let settled = settlement.authoritative;
        let transport_ok = settlement.transport_settled(false);
        let target_released = settlement.target_released;
        let is_error = driven.is_err()
            || evidence.session_id.is_none()
            || !evidence.terminal
            || evidence.provider_error.is_some()
            || !settled
            || !transport_ok;
        let mut output = evidence.final_output.take().unwrap_or(evidence.output);
        if is_error {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("Error: ");
            output.push_str(
                driven
                    .as_ref()
                    .err()
                    .map(String::as_str)
                    .or(evidence.provider_error)
                    .unwrap_or("native Claude stage did not complete with settled transport"),
            );
        }
        let mut metadata = json!({
            "native_collaborator": {
                "provider": PROVIDER_NAME,
                "protocol": PROTOCOL_NAME,
                "native_session_id": evidence.session_id,
                "native_turn_id": null,
                "requested_model": stage.model,
                "acknowledged_model": evidence.model,
                "model_resolution": if stage.model.is_some() { "provider_argument" } else { "provider_default" },
                "session_acknowledged": evidence.session_id.is_some(),
                "turn_acknowledged": evidence.terminal,
                "dispatch_state": if evidence.session_id.is_some() { "acknowledged" } else { "not_dispatched" },
                "native_terminal": evidence.terminal,
                "usage_snapshot": evidence.usage,
                "cost_usd": evidence.cost_usd,
                "provider_error_class": evidence.provider_error_class,
                "output_capped": evidence.output_capped,
                "target_released": target_released,
                "settlement_authoritative": settled,
                "transport_settled_after_terminal": transport_ok
            },
            "workspace_effect_settled": settled
        })
        .as_object_mut()
        .expect("native Claude metadata is an object")
        .clone();
        if let Some(session_id) = evidence.session_id {
            metadata.insert(
                astra_services::runs::COLLABORATOR_NATIVE_SESSION_METADATA_KEY.into(),
                json!(astra_services::runs::CollaboratorNativeSession {
                    anchor_run_id: stage.anchor_run_id,
                    provider: astra_services::runs::CollaboratorProvider::Claude,
                    native_session_id: session_id,
                }),
            );
        }
        if let Some(usage) = evidence.usage.as_ref().and_then(canonical_usage) {
            metadata.insert("collaborator_usage".into(), json!(usage));
        }
        if evidence.capability_unavailable {
            metadata.insert("native_capability_unavailable".into(), Value::Bool(true));
        }
        ToolResult {
            output,
            is_error,
            metadata: Some(metadata),
            exit_semantics: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_reuses_the_canonical_stage_shape() {
        let schema = schema();
        assert_eq!(schema["function"]["name"], TOOL_NAME);
        assert_eq!(schema["function"]["parameters"]["required"][0], "task");
        assert_eq!(
            schema["function"]["parameters"]["required"][1],
            "anchor_run_id"
        );
        assert_eq!(
            schema["function"]["parameters"]["properties"]["effort"]["enum"],
            json!(["low", "medium", "high", "max"])
        );
        assert!(
            Stage::parse(&json!({"task":"review","anchor_run_id":"anchor","effort":"none"}))
                .is_err()
        );
    }

    #[test]
    fn auth_failure_is_typed_without_using_free_text_as_control_flow() {
        let mut evidence = Evidence::default();
        ingest(
            br#"{"type":"assistant","error":"authentication_failed","message":{"content":[{"type":"text","text":"Not logged in"}]}}"#,
            &mut evidence,
            native_codex::OUTPUT_BYTES,
        )
        .unwrap();
        assert!(evidence.capability_unavailable);
        assert_eq!(
            evidence.provider_error,
            Some("Claude Code authentication is required")
        );
        assert_eq!(evidence.provider_error_class, Some("authentication"));
        assert_eq!(evidence.output, "Not logged in");
    }

    #[test]
    fn unknown_provider_error_text_is_not_control_flow() {
        let mut evidence = Evidence::default();
        ingest(
            br#"{"type":"assistant","error":"authentication_failed: secret-token-value","message":{"content":[]}}"#,
            &mut evidence,
            native_codex::OUTPUT_BYTES,
        )
        .unwrap();
        assert_eq!(evidence.provider_error_class, Some("provider"));
        assert_eq!(
            evidence.provider_error,
            Some("Claude Code reported a provider error")
        );
        assert!(
            !evidence
                .provider_error
                .is_some_and(|message| message.contains("secret-token-value"))
        );
    }

    #[test]
    fn claude_cli_tools_are_an_availability_boundary() {
        let read_only = claude_cli_args(true);
        assert!(read_only.iter().any(|arg| arg == "--restricted"));
        assert!(read_only.iter().any(|arg| arg == "--strict-mcp-config"));
        assert_eq!(
            read_only
                .iter()
                .position(|arg| arg == "--tools")
                .map(|index| &read_only[index + 1]),
            Some(&"Read,Glob,Grep".to_owned())
        );
        assert!(!read_only.iter().any(|arg| arg == "--allowedTools"));

        let writable = claude_cli_args(false);
        let tools = writable
            .iter()
            .position(|arg| arg == "--tools")
            .map(|index| writable[index + 1].as_str());
        assert_eq!(tools, Some("Read,Edit,Write,Glob,Grep"));
        assert!(!writable.iter().any(|arg| arg == "Bash"));
    }

    #[test]
    fn result_preserves_partial_output_and_cost() {
        let mut evidence = Evidence::default();
        ingest(
            br#"{"type":"system","subtype":"init","session_id":"s1","model":"sonnet"}"#,
            &mut evidence,
            native_codex::OUTPUT_BYTES,
        )
        .unwrap();
        ingest(
            br#"{"type":"result","session_id":"s1","result":"done","is_error":false,"total_cost_usd":0.25,"usage":{"input_tokens":10,"output_tokens":3,"cache_read_input_tokens":2,"cache_creation_input_tokens":1}}"#,
            &mut evidence,
            native_codex::OUTPUT_BYTES,
        )
        .unwrap();
        assert!(evidence.terminal);
        assert_eq!(evidence.final_output.as_deref(), Some("done"));
        assert_eq!(evidence.cost_usd, Some(0.25));
        assert!(canonical_usage(evidence.usage.as_ref().unwrap()).is_some());
    }

    #[test]
    fn session_change_is_rejected() {
        let mut evidence = Evidence::default();
        ingest(
            br#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            &mut evidence,
            native_codex::OUTPUT_BYTES,
        )
        .unwrap();
        assert!(
            ingest(
                br#"{"type":"result","session_id":"s2","result":"wrong","is_error":false}"#,
                &mut evidence,
                native_codex::OUTPUT_BYTES,
            )
            .is_err()
        );
    }
}
