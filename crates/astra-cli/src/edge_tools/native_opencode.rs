//! OpenCode ACP adapter.
//!
//! OpenCode's ACP transport is a newline-delimited JSON-RPC process. This
//! adapter translates only ACP session/prompt facts; the existing Edge owner,
//! child run association, cancellation, permission receipt and journal remain
//! the authorities for lifecycle and recovery.

use super::{ApprovedNativeRuntime, ToolExecutor, native_codex};
use astra_edge::EdgeInvocationInput;
use astra_sandbox::{FramedProcess, FramedProcessLimits};
use astra_tools::{ProviderInteractionGate, ToolResult};
use astra_turn_types::{ProviderRuntimeRequirements, ProviderStageInputAck};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const TOOL_NAME: &str = "native_opencode";
const PROVIDER_NAME: &str = "opencode";
const PROTOCOL_NAME: &str = "opencode-acp";
const EXECUTABLE_NAMES: &[&str] = if cfg!(windows) {
    &["opencode.exe", "opencode.cmd", "opencode.bat", "opencode"]
} else {
    &["opencode"]
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
            .map_err(|_| "invalid native OpenCode stage arguments")?;
        let valid = |value: &str| {
            !value.is_empty()
                && value.len() <= 512
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
            || stage.effort.as_deref().is_some_and(|effort| {
                !matches!(effort, "none" | "low" | "medium" | "high" | "xhigh" | "max")
            })
        {
            return Err("invalid native OpenCode stage arguments");
        }
        if stage.effort.as_deref() == Some("none") {
            return Err("native OpenCode does not support disabling reasoning; omit effort");
        }
        Ok(stage)
    }
}

pub fn schema() -> Value {
    let mut schema = native_codex::schema();
    schema["function"]["name"] = json!(TOOL_NAME);
    schema["function"]["description"] = json!(
        "Execute one admitted native OpenCode collaborator stage through ACP in the selected CLI workspace. The model is passed through ACP configuration; the acknowledged session and prompt result establish what actually ran. Resume only an exact acknowledged native_session_id. Run, control and deadline authority comes from the invocation, never arguments."
    );
    schema["function"]["parameters"]["properties"]["effort"]["enum"] =
        json!(["low", "medium", "high", "xhigh", "max"]);
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
            astra_turn_core::provider_resolution::NativeCollaboratorProtocol::OpenCodeAcp
                .extension_value()
        ),
    );
    astra_turn_types::ProviderRuntimeRequirements::from_extension_fields(&extension_fields)?;
    let declaration = astra_turn_types::ProviderToolDeclaration {
        native_tool_id: astra_turn_types::NativeToolId::new(TOOL_NAME)?,
        native_tool_name: TOOL_NAME.into(),
        stable_tool_alias: Some(astra_turn_types::StableToolAlias::new(TOOL_NAME)?),
        title: Some("Native OpenCode collaborator".into()),
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
        .ok_or("OpenCode executable is unavailable")?;
    native_codex::runtime_requirements_for_executable(&executable)
}

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
        return Err("OpenCode capability probe deadline expired");
    }
    let (mut command, owner) =
        native_codex::prepare_native_process(executable, &["--version".into()])
            .map_err(|_| "OpenCode process ownership unavailable")?;
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
        .map_err(|_| "OpenCode capability probe unavailable")?;
    let frame = tokio::time::timeout(timeout, process.recv_frame())
        .await
        .ok()
        .flatten();
    let outcome = process
        .cancel_and_wait()
        .await
        .map_err(|_| "OpenCode capability probe settlement unavailable")?;
    if outcome
        .settlement
        .as_ref()
        .is_none_or(|settlement| !settlement.ownership.is_authoritative())
        || frame.as_deref().is_none_or(|frame| frame.is_empty())
    {
        return Err("OpenCode capability probe did not settle");
    }
    Ok(())
}

#[derive(Default)]
struct Evidence {
    session_id: Option<String>,
    output: String,
    terminal: bool,
    stop_reason: Option<String>,
    provider_error: Option<String>,
    provider_error_code: Option<i64>,
    usage: Option<Value>,
    cost_usd: Option<f64>,
    output_capped: bool,
    capability_unavailable: bool,
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

fn opencode_permission_environment(read_only: bool, network_allowed: bool) -> String {
    let web_tools = if network_allowed { "allow" } else { "deny" };
    let edit = if read_only { "deny" } else { "allow" };
    let mut read = serde_json::Map::new();
    read.insert("*".into(), json!("allow"));
    // OpenCode's permission object is the provider-side projection of the
    // canonical sandbox matcher. Keep the catch-all first because OpenCode
    // applies the last matching rule; no second sensitive-path policy is
    // introduced here.
    let rules = astra_sandbox::sensitive_path_rules();
    for substring in rules.path_substrings {
        let pattern = substring.trim_start_matches('/');
        if !pattern.is_empty() {
            read.insert(format!("*{pattern}*"), json!("deny"));
        }
    }
    for name in rules.credential_file_names {
        read.insert(format!("*{name}"), json!("deny"));
    }
    for marker in rules.credential_directories {
        let marker = marker.trim_start_matches('/');
        if !marker.is_empty() {
            read.insert(format!("*{marker}*"), json!("deny"));
        }
    }
    json!({
        "read": read,
        "list": "allow",
        "glob": "allow",
        "grep": "allow",
        "lsp": "allow",
        "edit": edit,
        "bash": "deny",
        "task": "deny",
        "skill": "deny",
        "mcp": "deny",
        "question": "deny",
        "external_directory": "deny",
        "webfetch": web_tools,
        "websearch": web_tools,
        "todoread": "deny",
        "todowrite": "deny",
        "doom_loop": "deny"
    })
    .to_string()
}

fn ingest_update(update: &Value, evidence: &mut Evidence, output_limit: usize) {
    if update.get("sessionUpdate").and_then(Value::as_str) == Some("agent_message_chunk")
        && let Some(text) = update
            .get("content")
            .and_then(|content| content.get("text"))
            .and_then(Value::as_str)
    {
        append_text(
            &mut evidence.output,
            text,
            output_limit,
            &mut evidence.output_capped,
        );
    }
    if update.get("sessionUpdate").and_then(Value::as_str) == Some("usage_update") {
        if let Some(cost) = update.pointer("/cost/amount").and_then(Value::as_f64) {
            evidence.cost_usd = Some(cost);
        }
    }
}

fn ingest_notification(
    envelope: &Value,
    evidence: &mut Evidence,
    expected_session_id: Option<&str>,
    output_limit: usize,
) -> Result<(), &'static str> {
    if envelope.get("id").is_some() {
        return Err("OpenCode requested an unsupported interactive operation");
    }
    if envelope.get("method").and_then(Value::as_str) != Some("session/update") {
        return Ok(());
    }
    let params = envelope
        .get("params")
        .and_then(Value::as_object)
        .ok_or("OpenCode session update params are invalid")?;
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|id| valid_id(id))
        .ok_or("OpenCode session update has no valid sessionId")?;
    if expected_session_id.is_some_and(|expected| expected != session_id) {
        return Err("OpenCode session update changed the selected session");
    }
    evidence.session_id = Some(session_id.to_owned());
    if let Some(update) = params.get("update") {
        ingest_update(update, evidence, output_limit);
    }
    Ok(())
}

fn canonical_usage(value: &Value) -> Option<astra_turn_types::CanonicalTokenUsage> {
    let input = value.get("inputTokens").and_then(Value::as_u64);
    let output = value.get("outputTokens").and_then(Value::as_u64);
    astra_turn_types::CanonicalTokenUsage::new(Some(input?), None, None, Some(output?)).ok()
}

fn acknowledged_session_id(
    response: &Value,
    requested_session_id: Option<&str>,
) -> Result<String, &'static str> {
    let session_id = match response.get("sessionId") {
        Some(value) => value
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or("OpenCode session response has an invalid sessionId")?
            .to_owned(),
        // ACP session/resume acknowledges the requested session through a
        // successful response and returns configuration options, not another
        // sessionId. The caller already supplied the exact identity.
        None => requested_session_id
            .filter(|id| valid_id(id))
            .ok_or("OpenCode session response has no valid sessionId")?
            .to_owned(),
    };
    if requested_session_id.is_some_and(|expected| expected != session_id) {
        return Err("OpenCode resume acknowledged a different session");
    }
    Ok(session_id)
}

struct RpcError {
    message: String,
    code: Option<i64>,
    capability_unavailable: bool,
}

/// Preserve protocol-level error meaning without copying provider text into
/// user-visible output or using that text as control flow. ACP reserves
/// -32000 for authentication-required errors; the JSON-RPC standard codes
/// below distinguish an unsupported method, invalid request and provider
/// service failure. The service name is only included when it is a bounded,
/// structured value supplied by OpenCode.
fn classify_rpc_error(error: &Value) -> RpcError {
    let code = error.get("code").and_then(Value::as_i64);
    let message = match code {
        Some(-32000) => "OpenCode authentication is required".to_owned(),
        Some(-32601) => "OpenCode does not support the requested ACP method".to_owned(),
        Some(-32602) => "OpenCode rejected the ACP request parameters".to_owned(),
        Some(-32603) => error
            .pointer("/data/service")
            .and_then(Value::as_str)
            .filter(|service| {
                !service.is_empty()
                    && service.len() <= 64
                    && service
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            })
            .map(|service| format!("OpenCode service '{service}' is unavailable"))
            .unwrap_or_else(|| "OpenCode service is unavailable".to_owned()),
        _ => "OpenCode ACP request was rejected".to_owned(),
    };
    RpcError {
        message,
        code,
        capability_unavailable: code == Some(-32000),
    }
}

#[allow(clippy::too_many_arguments)]
async fn rpc(
    process: &mut FramedProcess,
    input: &astra_sandbox::FramedProcessInput,
    id: u64,
    method: &str,
    params: Value,
    evidence: &mut Evidence,
    expected_session_id: Option<&str>,
    output_limit: usize,
    cancel: &CancellationToken,
    input_rx: &mut Option<tokio::sync::mpsc::Receiver<EdgeInvocationInput>>,
) -> Result<Value, String> {
    native_codex::send(
        input,
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
    )
    .await
    .map_err(str::to_owned)?;
    loop {
        let frame = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("native OpenCode invocation cancelled".into()),
            frame = process.recv_frame() => frame,
            input = async {
                match input_rx.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => None,
                }
            }, if input_rx.is_some() => {
                match input {
                    Some(input) => {
                        let _ = input.ack.send(ProviderStageInputAck::rejected(
                            &input.input,
                            "OpenCode ACP accepts follow-up text at the next collaborator stage",
                        ));
                    }
                    None => *input_rx = None,
                }
                continue;
            }
        }
        .ok_or_else(|| "OpenCode ACP ended before the request response".to_string())?;
        let envelope = native_codex::decode(&frame).map_err(str::to_owned)?;
        if envelope.get("method").is_some() {
            if let Some(request_id) = envelope.get("id") {
                // Do not leave a provider permission/question request hanging
                // on an adapter that did not advertise support for it. The
                // provider sees a typed rejection, while Astra still owns
                // cancellation and process settlement.
                let _ = native_codex::send(
                    input,
                    json!({
                        "jsonrpc":"2.0",
                        "id":request_id,
                        "error":{"code":-32601,"message":"Astra does not support this OpenCode interaction in the current stage"}
                    }),
                )
                .await;
                evidence.provider_error =
                    Some("OpenCode requested an unsupported interactive operation".into());
            }
            ingest_notification(&envelope, evidence, expected_session_id, output_limit)
                .map_err(str::to_owned)?;
            continue;
        }
        if envelope.get("id") != Some(&json!(id)) {
            return Err("OpenCode ACP response ID does not match the request".into());
        }
        if envelope.get("error").is_some() {
            let error = classify_rpc_error(
                envelope
                    .get("error")
                    .expect("checked OpenCode ACP error envelope"),
            );
            evidence.provider_error_code = error.code;
            evidence.capability_unavailable |= error.capability_unavailable;
            evidence.provider_error = Some(error.message.clone());
            return Err(error.message);
        }
        return envelope
            .get("result")
            .cloned()
            .ok_or_else(|| "OpenCode ACP response has no result".into());
    }
}

async fn drive(
    process: &mut FramedProcess,
    stage: &Stage,
    cwd: &str,
    evidence: &mut Evidence,
    output_limit: usize,
    cancel: &CancellationToken,
    mut input_rx: Option<tokio::sync::mpsc::Receiver<EdgeInvocationInput>>,
) -> Result<(), String> {
    let input = process.input();
    rpc(
        process,
        &input,
        1,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {},
            "clientInfo": {"name":"astra","version":env!("CARGO_PKG_VERSION")}
        }),
        evidence,
        None,
        output_limit,
        cancel,
        &mut input_rx,
    )
    .await?;
    // ACP v1 does not define the LSP-style `initialized` notification. Some
    // ACP clients reject it as an unknown method and never process the
    // following session request, so initialize's response is the handshake
    // boundary and the next request must be session/new or session/resume.
    let session_method = if stage.native_session_id.is_some() {
        "session/resume"
    } else {
        "session/new"
    };
    let mut session_params = json!({"cwd": cwd, "mcpServers": []});
    if let Some(session_id) = &stage.native_session_id {
        session_params["sessionId"] = json!(session_id);
    }
    let session = rpc(
        process,
        &input,
        2,
        session_method,
        session_params,
        evidence,
        stage.native_session_id.as_deref(),
        output_limit,
        cancel,
        &mut input_rx,
    )
    .await?;
    let session_id = acknowledged_session_id(&session, stage.native_session_id.as_deref())?;
    evidence.session_id = Some(session_id.clone());
    if let Some(model) = &stage.model {
        let value = if stage
            .effort
            .as_deref()
            .is_some_and(|effort| !matches!(effort, "none" | "minimal"))
        {
            format!("{}/{}", model, stage.effort.as_deref().unwrap())
        } else {
            model.clone()
        };
        rpc(
            process,
            &input,
            3,
            "session/set_config_option",
            json!({"sessionId":session_id,"configId":"model","value":value}),
            evidence,
            Some(&session_id),
            output_limit,
            cancel,
            &mut input_rx,
        )
        .await?;
    }
    rpc(
        process,
        &input,
        4,
        "session/prompt",
        json!({"sessionId":session_id,"prompt":[{"type":"text","text":stage.task}]}),
        evidence,
        Some(&session_id),
        output_limit,
        cancel,
        &mut input_rx,
    )
    .await
    .map(|result| {
        evidence.terminal = true;
        evidence.stop_reason = result
            .get("stopReason")
            .and_then(Value::as_str)
            .map(str::to_owned);
        evidence.usage = result.get("usage").cloned();
        if evidence.stop_reason.as_deref() != Some("end_turn") {
            evidence.provider_error = Some("OpenCode prompt did not end normally".into());
        }
    })?;
    Ok(())
}

fn failure(reason: &'static str) -> ToolResult {
    ToolResult {
        output: format!("Error: {reason}"),
        is_error: true,
        metadata: Some(
            json!({"native_collaborator":{"provider":PROVIDER_NAME,"protocol":PROTOCOL_NAME,
                "dispatch_state":"not_dispatched","native_session_id":null,"native_turn_id":null,
                "session_acknowledged":false,"turn_acknowledged":false,"target_released":false,
                "usage_snapshot":null,"cost_usd":null},"workspace_effect_settled":true})
            .as_object()
            .expect("native OpenCode failure metadata is an object")
            .clone(),
        ),
        exit_semantics: None,
    }
}

#[allow(clippy::too_many_arguments)]
impl ToolExecutor {
    pub(super) async fn execute_native_opencode(
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
            return failure("native OpenCode execution requires an immutable execution grant");
        };
        if invocation.run_id.filter(|id| !id.is_empty()).is_none()
            || invocation
                .tool_call_id
                .filter(|id| !id.is_empty())
                .is_none()
            || invocation.admission_source.is_none()
        {
            return failure(
                "native OpenCode execution requires canonical run and invocation admission",
            );
        }
        if cancel.is_some_and(CancellationToken::is_cancelled)
            || invocation
                .admission_deadline
                .is_some_and(|deadline| deadline <= std::time::Instant::now())
            || native_codex::native_stage_remaining(invocation).is_err()
        {
            return failure("native OpenCode invocation admission cancelled or expired");
        }
        let mut policy = astra_core::sync_poison::recover_rwlock_read(&self.sandbox_policy).clone();
        if policy
            .as_ref()
            .is_some_and(|policy| policy.isolation == astra_sandbox::IsolationLevel::Strict)
        {
            return failure(
                "native OpenCode protocol cannot prove the selected strict isolation boundary",
            );
        }
        let cwd = match execution_root.canonicalize() {
            Ok(cwd) => cwd,
            Err(_) => return failure("native OpenCode workspace is unavailable"),
        };
        if std::path::Path::new(&ceiling.workspace_root) != cwd {
            return failure(
                "native OpenCode execution grant does not match the selected workspace",
            );
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
                != astra_turn_core::provider_resolution::NativeCollaboratorProtocol::OpenCodeAcp
                || approved.requirements != requirements
                || approved.workspace_root != cwd
            {
                return failure(
                    "native OpenCode runtime approval does not match the selected provider",
                );
            }
            let Some(local_policy) = policy.as_mut() else {
                return failure(
                    "native OpenCode runtime approval requires a selected local sandbox policy",
                );
            };
            for path in &approved.requirements.read_paths {
                if astra_sandbox::is_never_readable_path(std::path::Path::new(path)) {
                    return failure("native OpenCode runtime approval includes a forbidden path");
                }
                local_policy.allowed_paths.push(path.into());
            }
        }
        if native_codex::validate_runtime_grant(
            &ceiling.runtime_read_paths,
            &requirements,
            policy.as_ref(),
        )
        .is_err()
        {
            return failure("native OpenCode runtime grant does not match the installed provider");
        }
        let executable = std::path::Path::new(&requirements.executable);
        let Some(attribution) =
            astra_tools::workspace_observation::WorkspaceAttributionState::capture(&cwd)
        else {
            return failure("native OpenCode workspace attribution is unavailable");
        };
        let (mut command, owner) =
            match native_codex::prepare_native_process(executable, &["acp".into()]) {
                Ok(prepared) => prepared,
                Err(_) => {
                    return failure("native OpenCode structured process ownership is unavailable");
                }
            };
        command.current_dir(&cwd);
        let read_only = !ceiling.workspace_write_allowed
            || self.read_only_execution
            || self.plan_mode_authoring_active().await;
        let network_allowed =
            ceiling.network_allowed && policy.as_ref().is_none_or(|policy| policy.network_allowed);
        if let Some(policy) = &mut policy {
            policy.project_root = cwd.clone();
            if astra_sandbox::sandbox_command(policy, &mut command).is_err() {
                return failure("native OpenCode sandbox preparation failed");
            }
        }
        // `sandbox_command` owns process environment setup. Add the
        // call-scoped provider permissions afterwards so project/user
        // OpenCode settings cannot widen the admitted surface.
        command.env(
            "OPENCODE_PERMISSION",
            opencode_permission_environment(read_only, network_allowed),
        );
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
            Err(_) => return failure("native OpenCode structured process spawn failed"),
        };
        let mut unsettled = native_codex::UnsettledOnDrop(Some(attribution));
        let mut evidence = Evidence::default();
        let cwd_text = cwd.to_string_lossy().into_owned();
        let driven = tokio::select! {
            biased;
            _ = token.cancelled() => Err("native OpenCode invocation cancelled".to_string()),
            result = drive(&mut process, &stage, &cwd_text, &mut evidence, native_codex::OUTPUT_BYTES, &token, input_rx) => result,
        };
        let (driven, settlement) =
            native_codex::settle_native_process(process, driven, &token, &mut unsettled).await;
        let settled = settlement.authoritative;
        let cleanup_cancelled = settlement.cancelled_after_terminal;
        let transport_ok = settlement.transport_settled(true);
        let target_released = settlement.target_released;
        let is_error = driven.is_err()
            || evidence.session_id.is_none()
            || !evidence.terminal
            || evidence.provider_error.is_some()
            || evidence.stop_reason.as_deref() != Some("end_turn")
            || !settled
            || !transport_ok;
        let mut output = evidence.output;
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
                    .or(evidence.provider_error.as_deref())
                    .or(evidence.stop_reason.as_deref())
                    .unwrap_or("native OpenCode stage did not complete with settled transport"),
            );
        }
        let mut metadata = json!({
            "native_collaborator": {
                "provider": PROVIDER_NAME,
                "protocol": PROTOCOL_NAME,
                "native_session_id": evidence.session_id,
                "native_turn_id": null,
                "requested_model": stage.model,
                "model_resolution": if stage.model.is_some() { "provider_argument" } else { "provider_default" },
                "session_acknowledged": evidence.session_id.is_some(),
                "turn_acknowledged": evidence.terminal,
                "dispatch_state": if evidence.session_id.is_some() { "acknowledged" } else { "not_dispatched" },
                "native_terminal": evidence.terminal,
                "usage_snapshot": evidence.usage,
                "cost_usd": evidence.cost_usd,
                "output_capped": evidence.output_capped,
                "target_released": target_released,
                "settlement_authoritative": settled,
                "transport_settled_after_terminal": transport_ok,
                "cleanup_cancelled": cleanup_cancelled,
                "provider_error": evidence.provider_error,
                "provider_error_code": evidence.provider_error_code
            },
            "workspace_effect_settled": settled
        })
        .as_object_mut()
        .expect("native OpenCode metadata is an object")
        .clone();
        if let Some(session_id) = evidence.session_id {
            metadata.insert(
                astra_services::runs::COLLABORATOR_NATIVE_SESSION_METADATA_KEY.into(),
                json!(astra_services::runs::CollaboratorNativeSession {
                    anchor_run_id: stage.anchor_run_id,
                    provider: astra_services::runs::CollaboratorProvider::OpenCode,
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
    use astra_tools::ProviderInteractionDecision;

    #[test]
    fn schema_reuses_the_canonical_stage_shape() {
        let schema = schema();
        assert_eq!(schema["function"]["name"], TOOL_NAME);
        assert_eq!(schema["function"]["parameters"]["required"][0], "task");
        assert_eq!(
            schema["function"]["parameters"]["properties"]["effort"]["enum"],
            json!(["low", "medium", "high", "xhigh", "max"])
        );
        assert!(
            Stage::parse(&json!({"task":"review","anchor_run_id":"anchor","effort":"none"}))
                .is_err()
        );
    }

    #[test]
    fn session_update_preserves_exact_session_identity() {
        let mut evidence = Evidence::default();
        ingest_notification(
            &json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ok"}}}}),
            &mut evidence,
            Some("ses_1"),
            native_codex::OUTPUT_BYTES,
        )
        .unwrap();
        assert_eq!(evidence.session_id.as_deref(), Some("ses_1"));
        assert_eq!(evidence.output, "ok");
        assert!(ingest_notification(
            &json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_2","update":{}}}),
            &mut evidence,
            Some("ses_1"),
            native_codex::OUTPUT_BYTES,
        )
        .is_err());
    }

    #[test]
    fn provider_requests_are_not_silently_treated_as_notifications() {
        let mut evidence = Evidence::default();
        let error = ingest_notification(
            &json!({
                "jsonrpc":"2.0",
                "id":7,
                "method":"elicitation/create",
                "params":{"message":"choose"}
            }),
            &mut evidence,
            Some("ses_1"),
            native_codex::OUTPUT_BYTES,
        )
        .expect_err("unsupported provider requests must fail closed");
        assert_eq!(
            error,
            "OpenCode requested an unsupported interactive operation"
        );
    }

    #[test]
    fn usage_requires_both_input_and_output_counters() {
        assert!(canonical_usage(&json!({"inputTokens":2,"outputTokens":1})).is_some());
        assert!(canonical_usage(&json!({"inputTokens":2})).is_none());
    }

    #[test]
    fn permission_environment_projects_the_execution_ceiling() {
        let read_only: Value =
            serde_json::from_str(&opencode_permission_environment(true, false)).unwrap();
        assert_eq!(read_only["read"]["*"], "allow");
        assert_eq!(read_only["read"]["*.env*"], "deny");
        assert_eq!(read_only["edit"], "deny");
        assert_eq!(read_only["bash"], "deny");
        assert_eq!(read_only["webfetch"], "deny");
        assert_eq!(read_only["external_directory"], "deny");

        let writable: Value =
            serde_json::from_str(&opencode_permission_environment(false, true)).unwrap();
        assert_eq!(writable["edit"], "allow");
        assert_eq!(writable["websearch"], "allow");
        assert_eq!(writable["bash"], "deny");
        assert_eq!(writable["task"], "deny");
    }

    #[test]
    fn resume_acknowledges_the_requested_session_without_repeating_its_id() {
        assert_eq!(
            acknowledged_session_id(&json!({"configOptions": []}), Some("ses_existing")),
            Ok("ses_existing".to_owned())
        );
        assert_eq!(
            acknowledged_session_id(&json!({"sessionId": "ses_new"}), Some("ses_existing")),
            Err("OpenCode resume acknowledged a different session")
        );
        assert_eq!(
            acknowledged_session_id(&json!({"configOptions": []}), None),
            Err("OpenCode session response has no valid sessionId")
        );
    }

    #[test]
    fn rpc_errors_keep_structured_meaning_without_provider_text() {
        let auth = classify_rpc_error(&json!({
            "code": -32000,
            "message": "private token details"
        }));
        assert_eq!(auth.code, Some(-32000));
        assert!(auth.capability_unavailable);
        assert_eq!(auth.message, "OpenCode authentication is required");

        let service = classify_rpc_error(&json!({
            "code": -32603,
            "message": "private stack trace",
            "data": {"service": "directory"}
        }));
        assert!(!service.capability_unavailable);
        assert_eq!(
            service.message,
            "OpenCode service 'directory' is unavailable"
        );

        let untrusted = classify_rpc_error(&json!({
            "code": -32603,
            "data": {"service": "directory; secret"}
        }));
        assert_eq!(untrusted.message, "OpenCode service is unavailable");
    }

    /// Paid live evidence for the complete Astra adapter, not just a direct
    /// ACP probe. The executable is selected from PATH so this test exercises
    /// the same discovery, runtime grant, process owner, session identity and
    /// settlement path as production. Keep it opt-in because provider auth,
    /// network and model quota are deployment state.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "live OpenCode task; requires explicit opt-in, auth, PATH-selected executable and fresh supervisor binary"]
    async fn live_native_opencode_two_stages_same_session() {
        assert_eq!(
            std::env::var("ASTRA_NATIVE_OPENCODE_HARNESS").as_deref(),
            Ok("1"),
            "explicit paid-task opt-in is required"
        );
        let helper = std::path::PathBuf::from(
            std::env::var_os("ASTRA_NATIVE_HARNESS_SUPERVISOR_BIN")
                .expect("set the absolute path of a freshly built Astra supervisor binary"),
        );
        assert!(
            helper.is_absolute() && helper.is_file(),
            "supervisor binary unavailable"
        );
        let base = std::path::PathBuf::from("target/astra-native-opencode-harness");
        std::fs::create_dir_all(&base).expect("create disk-backed harness parent");
        let workspace = tempfile::Builder::new()
            .prefix("two-stage-")
            .tempdir_in(base)
            .expect("create isolated disk workspace");
        let root = workspace
            .path()
            .canonicalize()
            .expect("canonical workspace");
        let mut executor = ToolExecutor::new(&root);
        let mut policy = astra_sandbox::SandboxPolicy::permissive(&root);
        policy.network_allowed = true;
        *astra_core::sync_poison::recover_rwlock_write(&executor.sandbox_policy) = Some(policy);
        executor.set_read_only_execution();

        struct RejectUnexpectedInteraction;
        #[async_trait::async_trait]
        impl ProviderInteractionGate for RejectUnexpectedInteraction {
            async fn request_interaction(
                &self,
                request: &astra_turn_types::ProviderInteractionRequest,
            ) -> astra_tools::ProviderInteractionDecision {
                request
                    .validate()
                    .expect("canonical native question envelope");
                ProviderInteractionDecision::Cancelled
            }
        }

        let gate = RejectUnexpectedInteraction;
        let cancel = CancellationToken::new();
        let session = format!("opencode-live-session-{}", uuid::Uuid::now_v7());
        let anchor_run_id = format!("opencode-live-run-{}", uuid::Uuid::now_v7());
        let tasks = [
            "Return exactly OPENCODE_ADAPTER_STAGE_ONE and nothing else.",
            "Return exactly OPENCODE_ADAPTER_STAGE_TWO and nothing else.",
        ];
        let mut native_session: Option<String> = None;
        for (index, task) in tasks.into_iter().enumerate() {
            let run_id = format!("opencode-live-run-{}", uuid::Uuid::now_v7());
            let identity = astra_turn_types::ToolInvocationIdentity::new(
                "native-opencode-live-harness",
                &session,
                &run_id,
                &run_id,
                "native-stage",
            )
            .expect("canonical fixture invocation identity");
            let mut args = json!({"task": task, "anchor_run_id": anchor_run_id});
            if let Some(native_session) = &native_session {
                args["native_session_id"] = json!(native_session);
            }
            let invocation = astra_tools::tool_engine::ToolInvocationMetadata {
                admission_deadline: Some(std::time::Instant::now() + Duration::from_secs(180)),
                run_id: Some(&identity.run_id),
                turn_chain_id: Some(&identity.turn_chain_id),
                tool_call_id: Some(&identity.invocation_id),
                admission_source: Some(
                    astra_tools::tool_engine::ToolInvocationAdmissionSource::ParentApproval,
                ),
                ..astra_tools::tool_engine::ToolInvocationMetadata::default()
            };
            let mut ceiling = astra_server_types::edge_ws_protocol::EdgeExecutionCeiling {
                workspace_root: root.to_string_lossy().into_owned(),
                workspace_id: None,
                materialization_id: None,
                execution_binding_generation: 1,
                runtime_read_paths: Vec::new(),
                workspace_write_allowed: false,
                network_allowed: true,
            };
            ceiling.runtime_read_paths = installed_runtime_requirements()
                .expect("installed OpenCode requirements")
                .read_paths;
            let result = executor
                .execute_native_opencode(
                    &args,
                    invocation,
                    Some(&cancel),
                    &root,
                    Some(&gate),
                    Some(&ceiling),
                    None,
                    None,
                )
                .await;
            if result.is_error {
                let native = result
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("native_collaborator"));
                eprintln!(
                    "OpenCode live stage {} failed: provider_error={:?} code={:?} session_acknowledged={:?} terminal={:?} transport_settled={:?} output_bytes={}",
                    index + 1,
                    native.and_then(|value| value.get("provider_error")),
                    native.and_then(|value| value.get("provider_error_code")),
                    native.and_then(|value| value.get("session_acknowledged")),
                    native.and_then(|value| value.get("native_terminal")),
                    native.and_then(|value| value.get("transport_settled_after_terminal")),
                    result.output.len(),
                );
            }
            assert!(
                !result.is_error,
                "OpenCode adapter stage {} failed",
                index + 1
            );
            let metadata = result.metadata.as_ref().expect("native metadata");
            let native = metadata
                .get("native_collaborator")
                .and_then(Value::as_object)
                .expect("native collaborator metadata");
            assert_eq!(native.get("session_acknowledged"), Some(&Value::Bool(true)));
            assert_eq!(native.get("turn_acknowledged"), Some(&Value::Bool(true)));
            assert_eq!(native.get("native_terminal"), Some(&Value::Bool(true)));
            assert_eq!(
                native.get("transport_settled_after_terminal"),
                Some(&Value::Bool(true))
            );
            assert_eq!(
                result.output.trim(),
                if index == 0 {
                    "OPENCODE_ADAPTER_STAGE_ONE"
                } else {
                    "OPENCODE_ADAPTER_STAGE_TWO"
                }
            );
            let acknowledged = native
                .get("native_session_id")
                .and_then(Value::as_str)
                .expect("acknowledged OpenCode session");
            if let Some(previous) = &native_session {
                assert_eq!(
                    previous, acknowledged,
                    "resume changed the session identity"
                );
            }
            native_session = Some(acknowledged.to_owned());
        }
        workspace.close().expect("workspace cleanup");
    }
}
