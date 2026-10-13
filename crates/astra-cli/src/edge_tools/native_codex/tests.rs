use super::*;
use astra_tools::tool_engine::{ToolInvocationAdmissionSource, ToolInvocationMetadata};

/// Feed a fixture input through the same active dispatcher used by production.
#[allow(clippy::too_many_arguments)]
async fn submit_stage_input(
    process: &mut FramedProcess,
    input: &FramedProcessInput,
    evidence: &mut Evidence,
    gate: Option<&dyn ProviderInteractionGate>,
    cancel: &CancellationToken,
    stage_input: ProviderStageInput,
    output_limit: usize,
    ack_sender: tokio::sync::oneshot::Sender<ProviderStageInputAck>,
    input_rx: &mut Option<tokio::sync::mpsc::Receiver<EdgeInvocationInput>>,
) -> Result<(), &'static str> {
    let pending = start_stage_input(
        input,
        evidence,
        EdgeInvocationInput {
            input: stage_input,
            ack: ack_sender,
        },
    )
    .await?;
    drive_active_turn(
        process,
        input,
        evidence,
        gate,
        cancel,
        output_limit,
        input_rx.take(),
        pending,
    )
    .await
}

// Fixture-owned admission, not proof of the production server grant producer.
fn fixture_execution_ceiling(
    executor: &super::super::ToolExecutor,
) -> astra_server_types::edge_ws_protocol::EdgeExecutionCeiling {
    astra_server_types::edge_ws_protocol::EdgeExecutionCeiling {
        workspace_root: executor
            .effective_project_root()
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned(),
        workspace_id: None,
        materialization_id: None,
        execution_binding_generation: 1,
        runtime_read_paths: Vec::new(),
        workspace_write_allowed: false,
        network_allowed: false,
    }
}

fn stage() -> Stage {
    Stage::parse(&json!({"task": "Review stage", "anchor_run_id": "anchor"})).unwrap()
}

fn test_profile() -> Value {
    permission_profile("/workspace", false, false, &test_requirements()).unwrap()
}

fn test_requirements() -> astra_turn_types::ProviderRuntimeRequirements {
    astra_turn_types::ProviderRuntimeRequirements {
        executable: "/workspace/codex".into(),
        read_paths: vec!["/workspace/codex".into()],
    }
}

#[test]
fn provider_text_input_maps_to_one_fenced_codex_steer_request() {
    let evidence = Evidence {
        thread: Some("thread-1".into()),
        turn: Some("turn-7".into()),
        ..Evidence::default()
    };
    let input = ProviderStageInput::Text {
        input_id: "message-1".into(),
        content: "continue with the requested change".into(),
        correlation_id: None,
        expected_turn_id: Some("turn-7".into()),
    };
    let request = turn_steer_request(&evidence, &input).unwrap();
    assert_eq!(request["method"], "turn/steer");
    assert_eq!(request["params"]["threadId"], "thread-1");
    assert_eq!(request["params"]["expectedTurnId"], "turn-7");
    assert_eq!(request["params"]["clientUserMessageId"], "message-1");
    assert_eq!(
        request["params"]["input"][0]["text"],
        "continue with the requested change"
    );

    let mismatched = ProviderStageInput::Text {
        input_id: "message-1".into(),
        content: "continue with the requested change".into(),
        correlation_id: None,
        expected_turn_id: Some("turn-old".into()),
    };
    assert!(turn_steer_request(&evidence, &mismatched).is_err());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn malformed_steer_ack_is_transport_unknown_not_provider_rejection() {
    let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
request=recv()
assert request['method']=='turn/steer'
emit({'id':request['id'],'result':{}})
for line in sys.stdin: pass
"#;
    let token = CancellationToken::new();
    let mut process = transport::process(script, token.clone()).await;
    let mut evidence = Evidence {
        thread: Some("thread".into()),
        turn: Some("turn".into()),
        ..Evidence::default()
    };
    let input = process.input();
    let mut input_rx = None;
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    let result = submit_stage_input(
        &mut process,
        &input,
        &mut evidence,
        None,
        &token,
        ProviderStageInput::Text {
            input_id: "malformed-steer-ack".into(),
            content: "continue".into(),
            correlation_id: None,
            expected_turn_id: Some("turn".into()),
        },
        OUTPUT_BYTES,
        ack_tx,
        &mut input_rx,
    )
    .await;
    assert!(result.is_err());
    assert!(ack_rx.await.is_err());
    assert!(
        process
            .cancel_and_wait()
            .await
            .unwrap()
            .settlement
            .unwrap()
            .ownership
            .is_authoritative()
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn active_native_turn_accepts_a_fenced_mailbox_text_input() {
    let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
request=recv()
assert request['method']=='initialize'
emit({'id':1,'result':{'userAgent':'codex-cli test','codexHome':'/tmp/codex','platformFamily':'unix','platformOs':'linux'}})
assert recv()['method']=='initialized'
request=recv()
assert request['id']==7 and request['method']=='account/read'
emit({'id':7,'result':{'account':None,'requiresOpenaiAuth':False}})
request=recv()
assert request['method']=='thread/start'
emit({'id':2,'result':{'thread':{'id':'thread','status':{'type':'idle'}},'cwd':'/workspace','approvalPolicy':'never','approvalsReviewer':'user','activePermissionProfile':{'id':request['params']['permissions']},'sandbox':{'type':'readOnly','networkAccess':False}}})
request=recv()
assert request['method']=='turn/start'
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
request=recv()
assert request['method']=='turn/steer'
assert request['params']['threadId']=='thread'
assert request['params']['expectedTurnId']=='turn'
assert request['params']['clientUserMessageId']=='message-1'
assert request['params']['input'][0]['text']=='continue'
emit({'id':request['id'],'result':{'turnId':'turn'}})
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
for line in sys.stdin: pass
"#;
    let token = CancellationToken::new();
    let mut process = transport::process(script, token.clone()).await;
    let (input_tx, input_rx) = tokio::sync::mpsc::channel(1);
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    input_tx
        .send(EdgeInvocationInput {
            input: ProviderStageInput::Text {
                input_id: "message-1".into(),
                content: "continue".into(),
                correlation_id: None,
                expected_turn_id: Some("turn".into()),
            },
            ack: ack_tx,
        })
        .await
        .unwrap();
    drop(input_tx);
    let mut evidence = Evidence::default();
    drive_with_input(
        &mut process,
        &stage(),
        "/workspace",
        &test_profile(),
        &mut evidence,
        OUTPUT_BYTES,
        None,
        &token,
        Some(input_rx),
    )
    .await
    .unwrap();
    let ack = ack_rx.await.unwrap();
    assert!(ack.accepted);
    assert_eq!(ack.provider_turn_id.as_deref(), Some("turn"));
    assert_eq!(evidence.terminal.as_deref(), Some("completed"));
    assert!(
        process
            .wait()
            .await
            .unwrap()
            .settlement
            .unwrap()
            .ownership
            .is_authoritative()
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn uncertain_guidance_never_becomes_a_negative_receipt_or_replay() {
    for question in [false, true] {
        for (response, protocol_error) in [
            (
                json!({"error":{"code":-32603,"message":"internal failure"}}),
                false,
            ),
            (
                json!({"error":{"code":1234,"message":"provider failure"}}),
                false,
            ),
            (json!({"error":null}), true),
            (
                json!({"error":{"code":"rejected","message":"not accepted"}}),
                true,
            ),
            (json!({"error":{"code":-32600}}), true),
            (json!({"result":{"turnId":"different-turn"}}), true),
            (json!({"result":{}}), true),
            (
                json!({"id":"wrong-request","result":{"turnId":"turn"}}),
                true,
            ),
        ] {
            let script = format!(
                r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
request=recv()
assert request['method']=='turn/steer'
if {question}:
    emit({{'id':'native-question','method':'item/tool/requestUserInput','params':{{'threadId':'thread','turnId':'turn','itemId':'item','questions':[{{'id':'q','header':'Choice','question':'Proceed?'}}]}}}})
response=json.loads({response:?})
response.setdefault('id',request['id'])
emit(response)
if {question}:
    answer=recv()
    assert answer['id']=='native-question'
emit({{'method':'turn/completed','params':{{'threadId':'thread','turn':{{'id':'turn','status':'completed'}}}}}})
for line in sys.stdin:
    assert json.loads(line).get('method') != 'turn/steer'
"#,
                question = if question { "True" } else { "False" },
                response = response.to_string()
            );
            let token = CancellationToken::new();
            let mut process = transport::process(&script, token.clone()).await;
            let input = process.input();
            let mut evidence = Evidence {
                thread: Some("thread".into()),
                turn: Some("turn".into()),
                ..Evidence::default()
            };
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                submit_stage_input(
                    &mut process,
                    &input,
                    &mut evidence,
                    Some(&transport::AnswerGate),
                    &token,
                    ProviderStageInput::Text {
                        input_id: "uncertain".into(),
                        content: "extra guidance".into(),
                        correlation_id: None,
                        expected_turn_id: Some("turn".into()),
                    },
                    OUTPUT_BYTES,
                    ack_tx,
                    &mut None,
                ),
            )
            .await
            .expect("unknown steering must not hang healthy execution");
            assert_eq!(
                result.is_err(),
                protocol_error,
                "response={response}, question={question}"
            );
            assert!(
                ack_rx.await.is_err(),
                "uncertainty must not become a negative ACK"
            );
            assert!(
                process
                    .cancel_and_wait()
                    .await
                    .unwrap()
                    .settlement
                    .unwrap()
                    .ownership
                    .is_authoritative()
            );
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn active_native_turn_does_not_forge_acceptance_after_terminal_evidence() {
    let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
request=recv()
assert request['method']=='initialize'
emit({'id':1,'result':{'userAgent':'codex-cli test','codexHome':'/tmp/codex','platformFamily':'unix','platformOs':'linux'}})
assert recv()['method']=='initialized'
request=recv()
assert request['id']==7 and request['method']=='account/read'
emit({'id':7,'result':{'account':None,'requiresOpenaiAuth':False}})
request=recv()
assert request['method']=='thread/start'
emit({'id':2,'result':{'thread':{'id':'thread','status':{'type':'idle'}},'cwd':'/workspace','approvalPolicy':'never','approvalsReviewer':'user','activePermissionProfile':{'id':request['params']['permissions']},'sandbox':{'type':'readOnly','networkAccess':False}}})
request=recv()
assert request['method']=='turn/start'
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
request=recv()
assert request['method']=='turn/steer'
for line in sys.stdin: pass
"#;
    let token = CancellationToken::new();
    let mut process = transport::process(script, token.clone()).await;
    let (input_tx, input_rx) = tokio::sync::mpsc::channel(1);
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    input_tx
        .send(EdgeInvocationInput {
            input: ProviderStageInput::Text {
                input_id: "message-after-terminal".into(),
                content: "too late".into(),
                correlation_id: None,
                expected_turn_id: Some("turn".into()),
            },
            ack: ack_tx,
        })
        .await
        .unwrap();
    drop(input_tx);
    let mut evidence = Evidence::default();
    drive_with_input(
        &mut process,
        &stage(),
        "/workspace",
        &test_profile(),
        &mut evidence,
        OUTPUT_BYTES,
        None,
        &token,
        Some(input_rx),
    )
    .await
    .unwrap();
    assert!(ack_rx.await.is_err());
    assert_eq!(evidence.terminal.as_deref(), Some("completed"));
    assert!(
        process
            .wait()
            .await
            .unwrap()
            .settlement
            .unwrap()
            .ownership
            .is_authoritative()
    );
}

#[test]
fn provider_declaration_carries_bounded_runtime_requirements_in_the_existing_extension() {
    let requirements = test_requirements();
    let declaration = provider_declaration(requirements.clone(), None).unwrap();
    let roundtrip = astra_turn_types::ProviderRuntimeRequirements::from_extension_fields(
        &declaration.extension_fields,
    )
    .unwrap()
    .unwrap();
    assert_eq!(roundtrip, requirements);
    assert_eq!(declaration.native_tool_name, TOOL_NAME);
    assert!(declaration.is_collaborator_stage());
    assert!(declaration.claims.read_only.is_none());
    assert!(
        !declaration
            .extension_fields
            .contains_key("codex.protocolVersion"),
        "provider availability must be established by the protocol contract, not a CLI version"
    );
    assert!(
        declaration.input_schema["properties"]
            .get("runtime_read_paths")
            .is_none()
    );
    assert!(
        declaration.input_schema["properties"]
            .get("permissions")
            .is_none()
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn discovery_handshake_uses_the_same_bounded_initialize_contract_as_execution() {
    let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(value): print(json.dumps(value),flush=True)
request=recv()
assert request['id']==1 and request['method']=='initialize'
assert request['params']['capabilities']['explicitGatewayOauth'] is True
emit({'id':1,'result':{'userAgent':'codex-cli fixture','codexHome':'/tmp/codex','platformFamily':'unix','platformOs':'linux'}})
assert recv()['method']=='initialized'
request=recv()
assert request['id']==7 and request['method']=='account/read'
emit({'id':7,'result':{'account':None,'requiresOpenaiAuth':False}})
for line in sys.stdin:
    assert json.loads(line).get('method') != 'model/list', 'provider default queried a catalog'
"#;
    let token = CancellationToken::new();
    let mut process = transport::process(script, token.clone()).await;
    let input = process.input();
    let mut evidence = Evidence::default();
    initialize_protocol(
        &mut process,
        &input,
        &mut evidence,
        OUTPUT_BYTES,
        None,
        &token,
    )
    .await
    .expect("a valid initialize response establishes the protocol contract");
    verify_provider_authentication(&mut process, &input, &mut evidence, &token, OUTPUT_BYTES)
        .await
        .expect("current provider authentication establishes the capability boundary");
    assert_eq!(
        resolve_requested_model(
            &mut process,
            &input,
            &stage(),
            &mut evidence,
            OUTPUT_BYTES,
            &token,
            None,
        )
        .await
        .unwrap(),
        None,
        "authenticated provider-default execution must not require a model catalog"
    );
    let outcome = process.cancel_and_wait().await.unwrap();
    assert!(
        outcome
            .settlement
            .is_some_and(|settlement| settlement.ownership.is_authoritative())
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires a freshly built production invocation supervisor"]
async fn discovery_preserves_the_whole_budget_across_protocol_phases() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("provider");
    std::fs::write(
        &executable,
        r#"#!/usr/bin/env python3
import json,sys,time
def recv(): return json.loads(sys.stdin.readline())
def emit(value): print(json.dumps(value),flush=True)
assert recv()['method']=='initialize'
time.sleep(0.2)
emit({'id':1,'result':{'userAgent':'fixture','codexHome':'/fixture','platformFamily':'unix','platformOs':'linux'}})
assert recv()['method']=='initialized'
assert recv()['method']=='account/read'
emit({'id':7,'result':{'account':None,'requiresOpenaiAuth':False}})
assert recv()['method']=='model/list'
time.sleep(2.2)
emit({'id':4,'result':{'data':[{'id':'provider-model','model':'provider-model','displayName':'Provider Model'}],'nextCursor':None}})
for line in sys.stdin: pass
"#,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let catalog = verify_installed_protocol(
        &executable,
        root.path(),
        &CancellationToken::new(),
        std::time::Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap()
    .expect("authenticated discovery returns its model catalog");
    assert!(catalog.is_complete());
    assert_eq!(catalog.models[0].selector, "provider-model");
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "real authenticated Codex protocol/catalog probe; explicit native harness opt-in"]
async fn live_native_discovery_reports_current_catalog_without_model_execution() {
    assert_eq!(
        std::env::var("ASTRA_NATIVE_CODEX_HARNESS").as_deref(),
        Ok("1")
    );
    let directory = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();
    let catalog = verify_installed_protocol(
        &native_executable().unwrap(),
        directory.path(),
        &CancellationToken::new(),
        started + Duration::from_secs(5),
    )
    .await
    .expect("installed protocol must authenticate and settle")
    .expect("authenticated protocol returns catalog status");
    println!(
        "discovery elapsed_ms={} catalog_complete={} models={}",
        started.elapsed().as_millis(),
        catalog.is_complete(),
        catalog.models.len()
    );
    assert!(
        catalog.is_complete(),
        "current native catalog must be available; no model work was dispatched"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn discovery_readiness_rejects_a_cached_catalog_without_current_authentication() {
    let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(value): print(json.dumps(value),flush=True)
request=recv()
assert request['id']==1 and request['method']=='initialize'
emit({'id':1,'result':{'userAgent':'codex-cli fixture','codexHome':'/tmp/codex','platformFamily':'unix','platformOs':'linux'}})
assert recv()['method']=='initialized'
request=recv()
assert request['id']==7 and request['method']=='account/read'
emit({'id':7,'result':{'account':None,'requiresOpenaiAuth':True}})
for line in sys.stdin:
    request=json.loads(line)
    assert request['method']!='model/list', 'readiness must not trust the cached catalog after logout'
"#;
    let token = CancellationToken::new();
    let mut process = transport::process(script, token.clone()).await;
    let input = process.input();
    let mut evidence = Evidence::default();
    initialize_protocol(
        &mut process,
        &input,
        &mut evidence,
        OUTPUT_BYTES,
        None,
        &token,
    )
    .await
    .unwrap();
    let error =
        verify_provider_authentication(&mut process, &input, &mut evidence, &token, OUTPUT_BYTES)
            .await
            .unwrap_err();
    assert_eq!(error, "native provider authentication is unavailable");
    let outcome = process.cancel_and_wait().await.unwrap();
    assert!(
        outcome
            .settlement
            .is_some_and(|settlement| settlement.ownership.is_authoritative())
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn discovery_rejects_a_malformed_account_without_treating_it_as_authenticated() {
    let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(value): print(json.dumps(value),flush=True)
request=recv()
assert request['id']==1 and request['method']=='initialize'
emit({'id':1,'result':{'userAgent':'codex-cli fixture','codexHome':'/tmp/codex','platformFamily':'unix','platformOs':'linux'}})
assert recv()['method']=='initialized'
request=recv()
assert request['id']==7 and request['method']=='account/read'
emit({'id':7,'result':{'account':False,'requiresOpenaiAuth':True}})
for line in sys.stdin: pass
"#;
    let token = CancellationToken::new();
    let mut process = transport::process(script, token.clone()).await;
    let input = process.input();
    let mut evidence = Evidence::default();
    initialize_protocol(
        &mut process,
        &input,
        &mut evidence,
        OUTPUT_BYTES,
        None,
        &token,
    )
    .await
    .unwrap();
    let error =
        verify_provider_authentication(&mut process, &input, &mut evidence, &token, OUTPUT_BYTES)
            .await
            .unwrap_err();
    assert_eq!(error, "native account readiness response is invalid");
    assert!(evidence.capability_unavailable);
    let outcome = process.cancel_and_wait().await.unwrap();
    assert!(
        outcome
            .settlement
            .is_some_and(|settlement| settlement.ownership.is_authoritative())
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn discovery_handshake_rejects_a_malformed_initialize_object() {
    let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(value): print(json.dumps(value),flush=True)
request=recv()
assert request['id']==1 and request['method']=='initialize'
emit({'id':1,'result':{}})
for line in sys.stdin: pass
"#;
    let token = CancellationToken::new();
    let mut process = transport::process(script, token.clone()).await;
    let input = process.input();
    let mut evidence = Evidence::default();
    let error = initialize_protocol(
        &mut process,
        &input,
        &mut evidence,
        OUTPUT_BYTES,
        None,
        &token,
    )
    .await
    .unwrap_err();
    assert_eq!(error, "native initialize returned an invalid result");
    let outcome = process.cancel_and_wait().await.unwrap();
    assert!(
        outcome
            .settlement
            .is_some_and(|settlement| settlement.ownership.is_authoritative())
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn discovery_handshake_rejects_a_non_object_initialize_result() {
    let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(value): print(json.dumps(value),flush=True)
request=recv()
assert request['id']==1 and request['method']=='initialize'
emit({'id':1,'result':[]})
for line in sys.stdin: pass
"#;
    let token = CancellationToken::new();
    let mut process = transport::process(script, token.clone()).await;
    let input = process.input();
    let mut evidence = Evidence::default();
    let error = initialize_protocol(
        &mut process,
        &input,
        &mut evidence,
        OUTPUT_BYTES,
        None,
        &token,
    )
    .await
    .unwrap_err();
    assert_eq!(error, "native initialize returned an invalid result");
    let outcome = process.cancel_and_wait().await.unwrap();
    assert!(
        outcome
            .settlement
            .is_some_and(|settlement| settlement.ownership.is_authoritative())
    );
}

#[test]
fn runtime_grant_requires_exact_installation_local_authority_and_portable_bounds() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let executable = root.join("codex");
    std::fs::write(&executable, b"fake installed executable").unwrap();
    let text = executable.to_str().unwrap().to_owned();
    let expected = astra_turn_types::ProviderRuntimeRequirements {
        executable: text.clone(),
        read_paths: vec![text.clone()],
    };
    let policy = astra_sandbox::SandboxPolicy::for_project(&root);
    validate_runtime_grant(&expected.read_paths, &expected, Some(&policy)).unwrap();
    assert!(validate_runtime_grant(&[], &expected, Some(&policy)).is_err());
    assert!(validate_runtime_grant(&expected.read_paths, &expected, None).is_err());
    let mut extra = expected.read_paths.clone();
    extra.push("/usr".into());
    assert!(validate_runtime_grant(&extra, &expected, Some(&policy)).is_err());
    let mut too_many = expected.read_paths.clone();
    too_many.resize(33, text.clone());
    assert!(validate_runtime_grant(&too_many, &expected, Some(&policy)).is_err());
    for bad in [
        "/",
        "/tmp/*",
        "~/bin/codex",
        "/tmp/$HOME/codex",
        "/tmp/../bin/codex",
        "relative/codex",
    ] {
        let malformed = astra_turn_types::ProviderRuntimeRequirements {
            executable: bad.into(),
            read_paths: vec![bad.into()],
        };
        assert!(
            validate_runtime_grant(&malformed.read_paths, &malformed, Some(&policy)).is_err(),
            "{bad}"
        );
    }
    #[cfg(unix)]
    {
        let link = root.join("codex-link");
        std::os::unix::fs::symlink(&executable, &link).unwrap();
        let text = link.to_str().unwrap().to_owned();
        let alias = astra_turn_types::ProviderRuntimeRequirements {
            executable: text.clone(),
            read_paths: vec![text],
        };
        assert!(validate_runtime_grant(&alias.read_paths, &alias, Some(&policy)).is_err());
    }
    let other = tempfile::tempdir().unwrap();
    let mut outside_policy = astra_sandbox::SandboxPolicy::for_project(other.path());
    // Temporary directories are normally authorized by the project policy.
    // Exercise an actually disjoint authority, independent of TMPDIR.
    outside_policy.allowed_paths.clear();
    assert!(
        validate_runtime_grant(&expected.read_paths, &expected, Some(&outside_policy)).is_err()
    );
}

#[cfg(unix)]
#[test]
fn native_path_discovery_skips_a_non_runnable_shadow() {
    use std::os::unix::fs::PermissionsExt;

    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let shadow = first.path().join("codex");
    let usable = second.path().join("codex");
    std::fs::write(&shadow, b"not runnable").unwrap();
    std::fs::write(&usable, b"runnable").unwrap();
    let mut permissions = std::fs::metadata(&shadow).unwrap().permissions();
    permissions.set_mode(0o644);
    std::fs::set_permissions(&shadow, permissions).unwrap();
    let mut permissions = std::fs::metadata(&usable).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&usable, permissions).unwrap();

    let path = std::env::join_paths([first.path(), second.path(), second.path()]).unwrap();
    let candidates = native_executable_candidates_for_path(&path);
    assert_eq!(candidates, vec![usable.canonicalize().unwrap()]);
}

#[test]
fn native_profile_is_rootless_and_tracks_admitted_workspace_authority() {
    let read_only = test_profile();
    let config = requested_profile_config(&read_only).unwrap();
    assert!(config.get("extends").is_none());
    let filesystem = config["filesystem"].as_object().unwrap();
    assert!(!filesystem.contains_key(":minimal"));
    assert!(!filesystem.contains_key("/etc"));
    for path in [
        "/workspace/.kube/config",
        "/workspace/nested/.kube/config",
        "/workspace/nested/.env/private",
        "/workspace/.AWS/credentials",
        "/workspace/nested/.AWS/credentials",
    ] {
        assert!(profile_denies(filesystem, path), "{path}");
    }
    assert!(!filesystem.contains_key("/workspace/*config*"));
    assert_eq!(
        expected_profile_sandbox(&read_only, "/workspace").unwrap(),
        ("readOnly", false)
    );

    let writable = permission_profile("/workspace", true, true, &test_requirements()).unwrap();
    assert_eq!(
        expected_profile_sandbox(&writable, "/workspace").unwrap(),
        ("workspaceWrite", true)
    );
    #[cfg(target_os = "linux")]
    {
        let mut approved = test_requirements();
        for root in PLATFORM_RUNTIME_ROOTS {
            if let Ok(target) = std::path::Path::new(root).canonicalize() {
                approved.read_paths.push(target.to_str().unwrap().into());
            }
        }
        let projected = permission_profile("/workspace", false, false, &approved).unwrap();
        let projected = requested_profile_config(&projected).unwrap();
        assert_eq!(projected["filesystem"][":minimal"], "read");
        assert_eq!(projected["filesystem"]["/etc"], "deny");
        assert_eq!(projected["filesystem"]["/proc"], "deny");
        assert!(profile_denies(
            projected["filesystem"].as_object().unwrap(),
            "/lib64/nested/.env"
        ));
        assert!(projected["filesystem"].as_object().unwrap().len() <= 100);
        assert!(projected["filesystem"].get("/").is_none());
        let nested =
            permission_profile("/usr/src/astra-profile-fixture", true, false, &approved).unwrap();
        let nested = requested_profile_config(&nested).unwrap();
        assert_eq!(
            nested["filesystem"]["/usr/src/astra-profile-fixture/id_rsa"],
            "deny"
        );
    }
}

fn deny_matcher(pattern: &str) -> globset::GlobMatcher {
    // The native client's POSIX permission matcher uses these exact options.
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .allow_unclosed_class(true)
        .backslash_escape(true)
        .build()
        .unwrap()
        .compile_matcher()
}

fn profile_denies(filesystem: &serde_json::Map<String, Value>, path: &str) -> bool {
    filesystem
        .iter()
        .filter(|(_, permission)| *permission == "deny")
        .any(|(pattern, _)| deny_matcher(pattern).is_match(path))
}

#[test]
fn compact_native_masks_preserve_canonical_rule_matches() {
    let rules = astra_sandbox::sensitive_path_rules();
    let mut old = Vec::new();
    let mut samples = vec!["ordinary".to_owned(), "safe/configuration".to_owned()];
    for substring in rules.path_substrings {
        let substring = substring.trim_start_matches('/');
        if substring.is_empty() {
            continue;
        }
        old.extend([
            format!("/workspace/*{substring}*"),
            format!("/workspace/**/*{substring}*"),
            format!("/workspace/*{substring}*/**"),
            format!("/workspace/**/*{substring}*/**"),
        ]);
        samples.extend([
            substring.to_owned(),
            format!("prefix{substring}suffix"),
            substring.to_uppercase(),
        ]);
    }
    for name in rules.credential_file_names {
        old.extend([
            format!("/workspace/{name}"),
            format!("/workspace/**/{name}"),
        ]);
        samples.extend([(*name).to_owned(), format!("prefix{name}suffix")]);
    }
    for marker in rules.credential_directories {
        let marker = marker.trim_start_matches('/');
        let pattern = case_insensitive_glob_literal(marker);
        old.extend([
            format!("/workspace/{pattern}"),
            format!("/workspace/{pattern}/**"),
            format!("/workspace/**/{pattern}"),
            format!("/workspace/**/{pattern}/**"),
        ]);
        samples.extend([
            marker.to_owned(),
            marker.to_uppercase(),
            format!("prefix{marker}suffix"),
        ]);
    }
    let old = old
        .iter()
        .map(|pattern| deny_matcher(pattern))
        .collect::<Vec<_>>();
    let profile = test_profile();
    let config = requested_profile_config(&profile).unwrap();
    let filesystem = config["filesystem"].as_object().unwrap();
    let compact = filesystem
        .iter()
        .filter(|(_, permission)| *permission == "deny")
        .map(|(pattern, _)| deny_matcher(pattern))
        .collect::<Vec<_>>();
    for sample in samples {
        for prefix in ["", "nested/", "nested/deeper/"] {
            for suffix in ["", "/child", "/child/deeper"] {
                let path = format!("/workspace/{prefix}{sample}{suffix}");
                assert_eq!(
                    old.iter().any(|matcher| matcher.is_match(&path)),
                    compact.iter().any(|matcher| matcher.is_match(&path)),
                    "{path}"
                );
            }
        }
    }
    for root in ["/workspace/[alias]", "/workspace/*", "/workspace/{alias}"] {
        assert!(permission_profile(root, false, false, &test_requirements()).is_err());
        let mut requirements = test_requirements();
        requirements.read_paths.push(root.into());
        assert!(permission_profile("/workspace", false, false, &requirements).is_err());
    }
    for root in [
        "/workspace/.env-secrets",
        "/workspace/.env-secrets/project",
        "/workspace/credentials.json/project",
    ] {
        assert!(astra_sandbox::is_never_readable_path(std::path::Path::new(
            root
        )));
        for write in [false, true] {
            assert!(permission_profile(root, write, false, &test_requirements()).is_err());
        }
    }
    #[cfg(unix)]
    {
        let directory = tempfile::tempdir().unwrap();
        let sensitive = directory.path().join(".env-secrets/project");
        std::fs::create_dir_all(&sensitive).unwrap();
        let alias = directory.path().join("innocent");
        std::os::unix::fs::symlink(&sensitive, &alias).unwrap();
        let canonical = alias.canonicalize().unwrap();
        assert!(astra_sandbox::is_never_readable_path(&canonical));
        assert!(
            permission_profile(
                canonical.to_str().unwrap(),
                false,
                false,
                &test_requirements()
            )
            .is_err()
        );
    }
}

#[test]
fn native_stage_lifetime_is_optional_and_independent_of_command_cap() {
    let metadata = ToolInvocationMetadata {
        command_timeout_cap_ms: Some(86_400_000),
        ..Default::default()
    };
    assert_eq!(native_stage_remaining(metadata), Ok(None));
    let deadline = std::time::Instant::now() + Duration::from_secs(43_200);
    for command_timeout_cap_ms in [None, Some(1), Some(86_400_000)] {
        let remaining = native_stage_remaining(ToolInvocationMetadata {
            admission_deadline: Some(deadline),
            command_timeout_cap_ms,
            ..Default::default()
        })
        .unwrap()
        .unwrap();
        assert!(remaining > Duration::from_secs(43_199));
        assert!(remaining <= Duration::from_secs(43_200));
    }
    assert!(
        native_stage_remaining(ToolInvocationMetadata {
            admission_deadline: Some(std::time::Instant::now()),
            ..metadata
        })
        .is_err()
    );
}

fn questionnaire() -> ProviderInteractionRequest {
    ProviderInteractionRequest {
        request_id: "rpc-question".into(),
        timeout_ms: Some(10_000),
        payload: json!({"provider":"codex", "method":"item/tool/requestUserInput", "params":{"questions":[
            {"id":"native-first", "header":"First", "question":"Choose first?", "isOther":true,
             "options":[{"label":"A", "description":"First option"},{"label":"B", "description":"Second option"}]},
            {"id":"native-second", "header":"Second", "question":"Choose second?", "isOther":false,
             "options":[{"label":"C", "description":"Third option"},{"label":"D", "description":"Fourth option"}]}
        ]}}),
    }
}

#[test]
fn native_question_projection_preserves_ids_options_and_normalized_order() {
    let request = questionnaire();
    let mut prompt = question_prompt(&request).unwrap();
    prompt.context = Some("Codex · originating session".into());
    assert_eq!(prompt.questions[0].header, "First");
    assert_eq!(
        prompt.questions[0].options[0].description.as_deref(),
        Some("First option")
    );
    assert!(prompt.questions[0].allow_freeform);
    assert!(!prompt.questions[1].allow_freeform);
    let answers = astra_tools::AskUserAnswers {
        answers: vec![
            astra_tools::AskUserQuestionAnswer {
                id: Some("native-second".into()),
                question: "Choose second?".into(),
                answers: vec![" D ".into()],
                multi_select: false,
                annotation: None,
            },
            astra_tools::AskUserQuestionAnswer {
                id: Some("native-first".into()),
                question: "Choose first?".into(),
                answers: vec!["custom answer".into()],
                multi_select: false,
                annotation: None,
            },
        ],
    };
    assert_eq!(
        question_response(&request, &prompt, &answers).unwrap(),
        json!({"answers":{
            "native-first":{"answers":["custom answer"]}, "native-second":{"answers":["D"]}
        }})
    );
    let mut wrong = prompt.clone();
    wrong.questions.swap(0, 1);
    assert!(question_response(&request, &wrong, &answers).is_err());
}

#[test]
fn native_question_ids_disambiguate_identical_presentations() {
    let mut request = questionnaire();
    for question in request.payload["params"]["questions"]
        .as_array_mut()
        .unwrap()
    {
        question["header"] = json!("Choice");
        question["question"] = json!("Continue?");
    }
    let prompt = question_prompt(&request).unwrap();
    let answers = astra_tools::AskUserAnswers {
        answers: vec![
            astra_tools::AskUserQuestionAnswer {
                id: Some("native-second".into()),
                question: "Continue?".into(),
                answers: vec!["D".into()],
                multi_select: false,
                annotation: None,
            },
            astra_tools::AskUserQuestionAnswer {
                id: Some("native-first".into()),
                question: "Continue?".into(),
                answers: vec!["A".into()],
                multi_select: false,
                annotation: None,
            },
        ],
    };
    let normalized = astra_tools::normalize_ask_user_answers(&prompt, &answers).unwrap();
    assert_eq!(
        normalized.to_tool_result_value()["answers"],
        json!({"native-first":"A", "native-second":"D"})
    );
    assert_eq!(
        question_response(&request, &prompt, &answers).unwrap(),
        json!({"answers":{"native-first":{"answers":["A"]},"native-second":{"answers":["D"]}}})
    );
    for id in [None, Some("native-first"), Some("unexpected")] {
        let mut invalid = answers.clone();
        invalid.answers[0].id = id.map(str::to_owned);
        assert!(question_response(&request, &prompt, &invalid).is_err());
    }
    let mut mixed = serde_json::to_value(&prompt).unwrap();
    mixed["questions"][0].as_object_mut().unwrap().remove("id");
    assert!(astra_tools::parse_ask_user_prompt(&mixed).is_err());
}

#[test]
fn native_question_projection_rejects_secret_duplicates_and_nonquestions() {
    let mut request = questionnaire();
    request.payload["params"]["questions"][0]["isSecret"] = json!(true);
    request.payload["params"]["questions"][0]["question"] =
        json!("secret-content-not-for-rendering");
    let error = question_prompt(&request).unwrap_err();
    assert!(error.contains("secret"));
    assert!(!error.contains("secret-content-not-for-rendering"));
    let mut request = questionnaire();
    request.payload["params"]["questions"][1]["id"] = json!("native-first");
    assert!(question_prompt(&request).unwrap_err().contains("unique"));
    let mut request = questionnaire();
    request.payload["params"]["questions"][1]["question"] = json!("Choose first?");
    assert!(question_prompt(&request).is_ok());
    request.payload["method"] = json!("item/permissions/requestApproval");
    assert!(
        question_prompt(&request)
            .unwrap_err()
            .contains("not supported")
    );
}

fn active() -> Evidence {
    Evidence {
        thread: Some("thread".into()),
        turn: Some("turn".into()),
        ..Evidence::default()
    }
}

fn terminal(thread: &str, turn: &str) -> Value {
    json!({"method": "turn/completed", "params": {"threadId": thread, "turn": {"id": turn, "status": "completed"}}})
}

#[test]
fn stage_arguments_cannot_carry_execution_controls() {
    for field in [
        "run_id",
        "expected_control_epoch",
        "timeout",
        "command",
        "cwd",
        "approvalPolicy",
    ] {
        let mut args = json!({"task": "Review", "anchor_run_id": "anchor"});
        args[field] = json!("injected");
        assert!(Stage::parse(&args).is_err(), "{field}");
    }
    assert!(Stage::parse(&json!({"task": "Review", "anchor_run_id": " anchor"})).is_err());
}

#[test]
fn exact_thread_resume_and_native_turn_input() {
    let mut stage = stage();
    stage.native_session_id = Some("native-thread".into());
    stage.model = Some("chosen-model".into());
    stage.effort = Some("xhigh".into());
    let profile = test_profile();
    let resume = thread_request(&stage, "/workspace", &profile, stage.model.as_deref());
    assert_eq!(resume["method"], "thread/resume");
    assert_eq!(resume["params"]["threadId"], "native-thread");
    assert_eq!(resume["params"]["excludeTurns"], true);
    assert_eq!(resume["params"]["config"]["mcp_servers"], json!({}));
    assert_eq!(resume["params"]["config"]["features.apps"], false);
    assert_eq!(resume["params"]["config"]["features.plugins"], false);
    assert_eq!(resume["params"]["config"]["features.hooks"], false);
    assert_eq!(resume["params"]["config"]["notify"], json!([]));
    assert!(resume["params"].get("path").is_none());
    let turn = turn_request(
        &stage,
        "native-thread",
        "/workspace",
        &profile,
        stage.model.as_deref(),
    );
    assert_eq!(
        turn["params"]["input"][0],
        json!({"type": "text", "text": "Review stage", "text_elements": []})
    );
    assert_eq!(turn["params"]["permissions"], profile["profileId"]);
    assert!(turn["params"].get("sandboxPolicy").is_none());
    assert!(resume["params"].get("sandbox").is_none());
    let mut expected_config = profile["config"].clone();
    expected_config["mcp_servers"] = json!({});
    expected_config["features.apps"] = json!(false);
    expected_config["features.plugins"] = json!(false);
    expected_config["features.hooks"] = json!(false);
    expected_config["notify"] = json!([]);
    assert_eq!(resume["params"]["config"], expected_config);
    assert_eq!(turn["params"]["effort"], "xhigh");
}

fn model_item(id: &str, model: &str, display_name: &str) -> NativeModelListItem {
    NativeModelListItem {
        id: id.into(),
        model: model.into(),
        display_name: display_name.into(),
        aliases: vec![id.into()],
        hidden: false,
        supported_reasoning_efforts: Vec::new(),
    }
}

#[test]
fn native_model_selector_uses_provider_names_without_guessing() {
    let mut models = vec![
        model_item("luna-56", "gpt-5.6-luna", "GPT-5.6-Luna"),
        model_item("luna-6", "gpt-6-luna", "GPT-6-Luna"),
        model_item("sol-6", "gpt-6-sol", "GPT-6-Sol"),
    ];
    assert_eq!(
        resolve_model_selector_diagnostic("gpt-5.6-luna", &models).unwrap(),
        "gpt-5.6-luna"
    );
    assert_eq!(
        resolve_model_selector_diagnostic("luna-56", &models).unwrap(),
        "gpt-5.6-luna"
    );
    assert_eq!(
        resolve_model_selector_diagnostic("gPt-5.6-lUnA", &models).unwrap(),
        "gpt-5.6-luna"
    );
    let unavailable_component = resolve_model_selector_diagnostic("luna", &models)
        .unwrap_err()
        .to_string();
    assert!(unavailable_component.contains("not available"));
    let unavailable_selector = resolve_model_selector_diagnostic("sol", &models)
        .unwrap_err()
        .to_string();
    assert!(unavailable_selector.contains("not available"));
    assert!(resolve_model_selector_diagnostic("GPT-6-Luna", &models).is_ok());
    let unavailable = resolve_model_selector_diagnostic("missing", &models)
        .unwrap_err()
        .to_string();
    assert!(unavailable.contains("not available"));
    assert!(unavailable.contains("GPT-5.6-Luna"));
    assert!(!unavailable.contains("gpt-5.6-luna"));

    let duplicate_display = vec![
        model_item("luna-a", "provider-luna-a", "Luna"),
        model_item("luna-b", "provider-luna-b", "Luna"),
    ];
    let duplicate = resolve_model_selector_diagnostic("luna", &duplicate_display)
        .unwrap_err()
        .to_string();
    assert!(duplicate.contains("Luna (provider-luna-a)"));
    assert!(duplicate.contains("Luna (provider-luna-b)"));

    models[0].supported_reasoning_efforts = vec![NativeReasoningEffort {
        reasoning_effort: "high".into(),
    }];
    assert!(validate_requested_effort(Some("high"), &models[0]).is_ok());
    let unsupported = validate_requested_effort(Some("xhigh"), &models[0]).unwrap_err();
    assert!(unsupported.contains("GPT-5.6-Luna"));
    assert!(unsupported.contains("high"));

    models[2].aliases = vec!["sol".into()];
    assert_eq!(
        resolve_model_selector_diagnostic("sol", &models).unwrap(),
        "gpt-6-sol"
    );

    let case_distinct = vec![
        model_item("first", "CaseModel", "First Model"),
        model_item("second", "casemodel", "Second Model"),
    ];
    assert_eq!(
        resolve_model_selector_diagnostic("CaseModel", &case_distinct).unwrap(),
        "CaseModel"
    );
    assert_eq!(
        resolve_model_selector_diagnostic("casemodel", &case_distinct).unwrap(),
        "casemodel"
    );
    assert!(matches!(
        resolve_model_selector_diagnostic("CASEMODEL", &case_distinct),
        Err(ModelSelectorError::Ambiguous { .. })
    ));
}

#[test]
fn acknowledgements_never_guess_thread_or_join_old_work() {
    let mut stage = stage();
    stage.native_session_id = Some("expected".into());
    let mut evidence = Evidence::default();
    assert!(
        acknowledge_thread(
            &stage,
            &json!({"thread": {"id": "other", "status": {"type": "idle"}}}),
            &mut evidence
        )
        .is_err()
    );
    assert!(evidence.thread.is_none());
    assert!(
        acknowledge_thread(
            &stage,
            &json!({"thread": {"id": "expected", "status": {"type": "active"}}}),
            &mut evidence
        )
        .is_err()
    );
    assert_eq!(evidence.thread.as_deref(), Some("expected"));
    assert!(evidence.turn.is_none());
}

#[test]
fn terminal_requires_both_exact_identities_and_typed_terminal_status() {
    for (thread, turn) in [("other", "turn"), ("thread", "other")] {
        let mut evidence = active();
        assert!(
            evidence
                .notification(
                    "turn/completed",
                    &terminal(thread, turn)["params"],
                    OUTPUT_BYTES
                )
                .is_err()
        );
        assert!(evidence.terminal.is_none());
    }
    let mut evidence = active();
    assert!(
        evidence
            .notification(
                "turn/completed",
                &json!({"threadId": "thread", "turn": {"id": "turn", "status": "inProgress"}}),
                OUTPUT_BYTES
            )
            .is_err()
    );
    evidence
        .notification(
            "turn/completed",
            &terminal("thread", "turn")["params"],
            OUTPUT_BYTES,
        )
        .unwrap();
    assert_eq!(evidence.terminal.as_deref(), Some("completed"));
}

#[test]
fn terminal_failure_uses_only_correlated_bounded_machine_causes() {
    for (info, expected) in [
        (json!("usageLimitExceeded"), "usage_limit_exceeded"),
        (json!("unauthorized"), "unauthorized"),
        (json!("contextWindowExceeded"), "context_window_exceeded"),
        (
            json!({"httpConnectionFailed":{"httpStatusCode":401}}),
            "http_connection_failed",
        ),
        (
            json!({"responseStreamDisconnected":{}}),
            "response_stream_disconnected",
        ),
        (json!("x".repeat(100_000)), "native_turn_failed"),
        (Value::Null, "native_turn_failed"),
        (json!({"unauthorized":{}}), "native_turn_failed"),
        (json!("httpConnectionFailed"), "native_turn_failed"),
        (
            json!({"httpConnectionFailed":"synthetic-private-detail"}),
            "native_turn_failed",
        ),
        (
            json!({"httpConnectionFailed":{"httpStatusCode":"secret"}}),
            "native_turn_failed",
        ),
    ] {
        let mut evidence = active();
        let mut failed = terminal("thread", "turn")["params"].clone();
        failed["turn"]["status"] = json!("failed");
        failed["turn"]["error"] = json!({"codexErrorInfo":info,
            "message":"synthetic-private-detail", "additionalDetails":"synthetic-private-detail"});
        let mut foreign = failed.clone();
        foreign["threadId"] = json!("foreign");
        assert!(
            evidence
                .notification("turn/completed", &foreign, OUTPUT_BYTES)
                .is_err()
        );
        assert!(evidence.provider_error_class.is_none());
        evidence
            .notification("turn/completed", &failed, OUTPUT_BYTES)
            .unwrap();
        assert_eq!(evidence.terminal.as_deref(), Some("failed"));
        assert_eq!(evidence.provider_error_class, Some(expected));
        assert!(evidence.provider_error_code.is_none());
        assert!(evidence.provider_error_service.is_none());
    }
}

#[test]
fn native_sandbox_ack_cannot_broaden_selected_authority() {
    let requested = test_profile();
    let mut response = json!({"cwd": "/workspace", "approvalPolicy": "never", "approvalsReviewer": "user", "activePermissionProfile": {"id":requested["profileId"]}, "sandbox": {"type":"readOnly", "networkAccess":false}});
    verify_sandbox(&response, &requested, "/workspace").unwrap();
    response["activePermissionProfile"]["id"] = json!(":workspace");
    assert!(verify_sandbox(&response, &requested, "/workspace").is_err());
    response["activePermissionProfile"]["id"] = requested["profileId"].clone();
    response["cwd"] = json!("/other");
    assert!(verify_sandbox(&response, &requested, "/workspace").is_err());
    response["cwd"] = json!("/workspace");
    response["approvalPolicy"] = json!("on-request");
    assert!(verify_sandbox(&response, &requested, "/workspace").is_err());
    response["approvalPolicy"] = json!("never");
    response["activePermissionProfile"]["extends"] = json!("broad_profile");
    assert!(verify_sandbox(&response, &requested, "/workspace").is_err());
    response
        .as_object_mut()
        .unwrap()
        .remove("activePermissionProfile");
    assert!(verify_sandbox(&response, &requested, "/workspace").is_err());
}

#[test]
fn usage_is_last_snapshot_not_sum_and_absence_is_unknown() {
    let mut evidence = active();
    assert!(evidence.usage.is_none());
    assert!(evidence.stage_usage().unwrap().is_none());
    let counters = json!({"inputTokens": 30, "cachedInputTokens": 10, "outputTokens": 5, "reasoningOutputTokens": 2, "totalTokens": 35});
    let event = json!({"threadId": "thread", "turnId": "turn", "tokenUsage": {"total": counters, "last": counters, "modelContextWindow": null}});
    for _ in 0..2 {
        evidence
            .notification("thread/tokenUsage/updated", &event, OUTPUT_BYTES)
            .unwrap();
    }
    assert_eq!(evidence.usage.as_ref().unwrap()["total"]["totalTokens"], 35);
    assert_eq!(
        evidence.stage_usage_with_inclusive().unwrap().unwrap().0,
        Some(30)
    );
    // Missing cache-write evidence cannot be filled with an invented zero.
    assert_eq!(
        evidence.stage_usage().unwrap().unwrap().to_json(),
        json!({"cached_input_tokens": 10, "output_tokens": 5})
    );
}

fn usage_event(turn: &str, input: u64, cached: u64, creation: u64, output: u64) -> Value {
    let counters = json!({"inputTokens": input, "cachedInputTokens": cached, "cacheWriteInputTokens": creation, "outputTokens": output, "reasoningOutputTokens": 2, "totalTokens": input + output});
    json!({"threadId": "thread", "turnId": turn, "tokenUsage": {"total": counters, "last": counters}})
}

#[test]
fn usage_receipt_is_disjoint_stage_delta_not_thread_total_or_last_response() {
    let mut evidence = active();
    evidence.resumed = true;
    evidence
        .notification(
            "thread/tokenUsage/updated",
            &usage_event("old-turn", 100, 40, 20, 10),
            OUTPUT_BYTES,
        )
        .unwrap();
    let mut event = usage_event("turn", 150, 60, 30, 20);
    event["tokenUsage"]["last"] = usage_event("turn", 10, 4, 2, 3)["tokenUsage"]["last"].clone();
    for _ in 0..2 {
        evidence
            .notification("thread/tokenUsage/updated", &event, OUTPUT_BYTES)
            .unwrap();
    }
    assert_eq!(
        evidence.stage_usage().unwrap().unwrap().to_json(),
        json!({"input_tokens": 20, "cached_input_tokens": 20, "cache_creation_tokens": 10, "output_tokens": 10, "total_tokens": 60})
    );
    assert_eq!(
        evidence.stage_usage_with_inclusive().unwrap().unwrap().0,
        Some(50)
    );
    evidence
        .notification(
            "thread/tokenUsage/updated",
            &usage_event("old-turn", 100, 40, 20, 10),
            OUTPUT_BYTES,
        )
        .unwrap();
    assert!(
        evidence.stage_usage().unwrap().is_none(),
        "ambiguous replay must not recreate a charge"
    );
}

#[test]
fn resumed_usage_without_baseline_is_unknown_and_invalid_counters_do_not_become_receipts() {
    let mut evidence = active();
    evidence.resumed = true;
    evidence
        .notification(
            "thread/tokenUsage/updated",
            &usage_event("turn", 150, 60, 30, 20),
            OUTPUT_BYTES,
        )
        .unwrap();
    assert!(evidence.stage_usage().unwrap().is_none());
    evidence
        .notification(
            "thread/tokenUsage/updated",
            &usage_event("turn", 149, 60, 30, 20),
            OUTPUT_BYTES,
        )
        .unwrap();
    assert!(evidence.stage_usage().unwrap().is_none());
    let mut evidence = active();
    evidence
        .notification(
            "thread/tokenUsage/updated",
            &usage_event("turn", 30, 20, 11, 10),
            OUTPUT_BYTES,
        )
        .unwrap();
    assert!(evidence.usage.is_none());
    assert!(
        evidence
            .notification(
                "thread/tokenUsage/updated",
                &usage_event("old-turn", 100, 40, 20, 10),
                OUTPUT_BYTES
            )
            .is_err()
    );
}

#[test]
fn output_is_utf8_bounded_and_not_terminal_evidence() {
    let mut evidence = active();
    evidence
        .notification(
            "item/agentMessage/delta",
            &json!({"threadId": "thread", "turnId": "turn", "itemId": "answer", "delta": "你好completed"}),
            4,
        )
        .unwrap();
    assert_eq!(evidence.output, "你");
    assert!(evidence.output_capped);
    assert!(evidence.terminal.is_none());
}

#[test]
fn completed_agent_message_is_authoritative_over_truncated_progress() {
    let mut evidence = active();
    evidence
        .notification(
            "item/agentMessage/delta",
            &json!({"threadId": "thread", "turnId": "turn", "itemId": "answer", "delta": "x".repeat(OUTPUT_BYTES + 1)}),
            OUTPUT_BYTES,
        )
        .unwrap();
    evidence
        .notification(
            "item/completed",
            &json!({
                "threadId": "thread",
                "turnId": "turn",
                "item": {"type": "agentMessage", "id": "answer", "text": "final answer"}
            }),
            OUTPUT_BYTES,
        )
        .unwrap();
    assert_eq!(evidence.output, "final answer");
    assert!(evidence.output_capped);
    for delta in ["新", "答案"] {
        evidence
            .notification(
                "item/agentMessage/delta",
                &json!({"threadId": "thread", "turnId": "turn", "itemId": "next", "delta": delta}),
                OUTPUT_BYTES,
            )
            .unwrap();
    }
    assert_eq!(evidence.output, "新答案");
    assert!(evidence.terminal.is_none());
    assert!(
        evidence
            .notification(
                "item/agentMessage/delta",
                &json!({"threadId": "thread", "turnId": "turn", "delta": "unbound text"}),
                OUTPUT_BYTES,
            )
            .is_err()
    );
    assert_eq!(
        evidence.output, "新答案",
        "invalid identity must not replace observed text"
    );
}

#[test]
fn original_native_questions_and_rpc_id_type_are_preserved() {
    let envelope = json!({"id": "native-question", "method": "item/tool/requestUserInput", "params": {"threadId": "thread", "turnId": "turn", "itemId": "item", "questions": [{"id": "q", "question": "Which?"}], "isBlocking": true}});
    let request = interaction_request(&envelope, &active()).unwrap();
    assert_eq!(request.payload["native_request_id"], envelope["id"]);
    assert_eq!(request.payload["params"], envelope["params"]);
    assert_eq!(request.request_id, "\"native-question\"");
    let mut wrong = envelope;
    wrong["params"]["turnId"] = json!("other");
    assert!(interaction_request(&wrong, &active()).is_err());
}

#[tokio::test]
async fn selected_cli_entrypoint_requires_binding_policy_and_respects_explicit_budget() {
    struct NoTaskGate;
    #[async_trait::async_trait]
    impl ProviderInteractionGate for NoTaskGate {
        async fn request_interaction(
            &self,
            _: &ProviderInteractionRequest,
        ) -> ProviderInteractionDecision {
            panic!("preflight must not execute native work");
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(directory.path());
    let args = json!({"task": "Review", "anchor_run_id": "anchor"});
    let denied = executor
        .execute_with_invocation_metadata_cancelable(
            TOOL_NAME,
            &args,
            ToolInvocationMetadata::default(),
            None,
        )
        .await;
    assert!(denied.is_error);
    executor.set_cli_local_provider_schemas(vec![schema()]);
    executor.set_current_visible_tool_schemas(&[schema()]);
    let invocation = ToolInvocationMetadata {
        admission_deadline: Some(std::time::Instant::now() + Duration::from_secs(86_400)),
        run_id: Some("run"),
        tool_call_id: Some("call"),
        admission_source: Some(ToolInvocationAdmissionSource::Policy),
        command_timeout_cap_ms: Some(86_400_000),
        ..ToolInvocationMetadata::default()
    };
    let denied = executor
        .execute_native_provider_invocation(
            astra_turn_core::provider_resolution::NativeCollaboratorProtocol::CodexAppServer,
            TOOL_NAME,
            &args,
            invocation,
            None,
            &NoTaskGate,
            &fixture_execution_ceiling(&executor),
            None,
            tokio::sync::mpsc::channel(1).1,
            &|| Ok(()),
        )
        .await;
    assert!(denied.is_error);
    assert_eq!(
        denied.tool_result_fields.unwrap()["native_collaborator"]["dispatch_state"],
        "not_dispatched"
    );
    // Omitting a task cutoff does not permit a local sandbox bypass.
    // This reaches the selected entrypoint, not just the argument parser.
    *astra_core::sync_poison::recover_rwlock_write(&executor.sandbox_policy) = None;
    let denied = executor
        .execute_native_provider_invocation(
            astra_turn_core::provider_resolution::NativeCollaboratorProtocol::CodexAppServer,
            TOOL_NAME,
            &args,
            ToolInvocationMetadata {
                admission_deadline: None,
                ..invocation
            },
            None,
            &NoTaskGate,
            &fixture_execution_ceiling(&executor),
            None,
            tokio::sync::mpsc::channel(1).1,
            &|| Ok(()),
        )
        .await;
    assert!(denied.is_error);
    let fields = denied.tool_result_fields.unwrap();
    assert_eq!(fields["native_collaborator"]["target_released"], false);
    assert!(fields.get("collaborator_usage").is_none());

    let lease = astra_tools::workspace_observation::acquire_workspace_mutation_lease_with_options(
        directory.path(),
        None,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    let uncancelled = CancellationToken::new();
    // Only writable native work owns the exclusive lease. A read-only ceiling
    // deliberately permits observation while a parent holds that lease.
    let mut write_ceiling = fixture_execution_ceiling(&executor);
    write_ceiling.workspace_write_allowed = true;
    let denied = tokio::time::timeout(
        Duration::from_secs(2),
        executor.execute_native_provider_invocation(
            astra_turn_core::provider_resolution::NativeCollaboratorProtocol::CodexAppServer,
            TOOL_NAME,
            &args,
            ToolInvocationMetadata {
                admission_deadline: Some(std::time::Instant::now() + Duration::from_millis(100)),
                ..invocation
            },
            Some(&uncancelled),
            &NoTaskGate,
            &write_ceiling,
            None,
            tokio::sync::mpsc::channel(1).1,
            &|| panic!("expired lock wait must not reach provider dispatch"),
        ),
    )
    .await
    .expect("lease wait must use the admitted deadline without transport cancellation");
    assert!(denied.is_error);
    assert!(!uncancelled.is_cancelled());
    assert_eq!(
        denied.tool_result_fields.unwrap()["execution_started"],
        false
    );
    drop(lease);
}

/// Real producer effects with a local synthetic response, never a paid model.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "installed Codex, synthetic provider, fresh production supervisor; explicit opt-in"]
async fn live_native_configuration_suppresses_trusted_hooks_and_notifications() {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    assert_eq!(
        std::env::var("ASTRA_NATIVE_CODEX_HARNESS").as_deref(),
        Ok("1")
    );
    assert!(std::env::var_os("ASTRA_NATIVE_HARNESS_SUPERVISOR_BIN").is_some());
    let server = MockServer::start().await;
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    let config_home = directory.path().join("client");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&config_home).unwrap();
    let hook_marker = directory.path().join("hook");
    let notify_marker = directory.path().join("notify");
    let config_path = config_home.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
model = "synthetic-no-inference"
model_provider = "synthetic"
web_search = "live"
notify = ["/bin/sh", "-c", {}]
[model_providers.synthetic]
name = "Synthetic"
base_url = {}
wire_api = "responses"
requires_openai_auth = false
[features]
hooks = true
[hooks]
SessionStart = [{{ hooks = [{{ type = "command", command = {}, timeout = 2 }}] }}]
"#,
            json!(format!("touch {}", json!(notify_marker.to_str().unwrap()))),
            json!(format!("{}/v1", server.uri())),
            json!(format!("touch {}", json!(hook_marker.to_str().unwrap())))
        ),
    )
    .unwrap();
    let executable = native_executable().unwrap();
    let requirements = runtime_requirements_for_executable(&executable).unwrap();
    let mut retained_thread = None;
    for (index, (disable, resume, diagnostic)) in [
        (false, false, false),
        (false, true, false),
        (false, true, true),
        (true, false, true),
        (true, true, true),
        (true, false, true),
    ]
    .into_iter()
    .enumerate()
    {
        // The positive legacy-notify control passes the answer as one argv
        // entry; Linux's per-argument limit is not a protocol frame limit.
        // Exercise large frames on the actual admitted configuration (which
        // disables notify), including both start and retained-thread resume.
        let answer = if disable {
            "界".repeat(128_000)
        } else {
            "Synthetic boundary result.".to_owned()
        };
        let body = [
            json!({"type":"response.created","response":{"id":"synthetic"}}),
            json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant","id":"message","content":[{"type":"output_text","text":answer}]}}),
            json!({"type":"response.completed","response":{"id":"synthetic","usage":{"input_tokens":0,"input_tokens_details":null,"output_tokens":0,"output_tokens_details":null,"total_tokens":0}}}),
        ].iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
        let _response = Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        // The first stage authorizes configured live search. Every later
        // stage, including resume of that history, denies hosted networking.
        let profile = permission_profile(
            workspace.to_str().unwrap(),
            false,
            matches!(index, 0 | 5),
            &requirements,
        )
        .unwrap();
        for marker in [&hook_marker, &notify_marker] {
            if marker.exists() {
                std::fs::remove_file(marker).unwrap();
            }
        }
        let cancel = CancellationToken::new();
        let (mut command, owner) =
            prepare_native_process(&executable, &["app-server".into()]).unwrap();
        command
            .current_dir(&workspace)
            .env("CODEX_HOME", &config_home)
            .env("RUST_LOG", "codex_core::hook_runtime=warn")
            .env("NO_COLOR", "1");
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env_remove(key);
        }
        let mut process = owner
            .spawn_framed(
                command,
                FramedProcessLimits {
                    timeout: Some(Duration::from_secs(15)),
                    max_frame_bytes: FRAME_BYTES,
                    max_queued_frames: 1,
                    max_stderr_bytes: 4096,
                },
                cancel.clone(),
            )
            .unwrap();
        let input = process.input();
        let mut evidence = Evidence::default();
        let proof = async {
            initialize_protocol(
                &mut process,
                &input,
                &mut evidence,
                OUTPUT_BYTES,
                None,
                &cancel,
            )
            .await?;
            let listing = rpc(
                &mut process,
                &input,
                json!({"id":60,"method":"hooks/list","params":{"cwds":[workspace]}}),
                &mut evidence,
                OUTPUT_BYTES,
                None,
                Some(&cancel),
            )
            .await?;
            let hook = &listing["data"][0]["hooks"][0];
            let key = hook["key"].as_str().ok_or("missing synthetic hook")?;
            let hash = hook["currentHash"]
                .as_str()
                .ok_or("missing hook trust hash")?;
            rpc(
                &mut process,
                &input,
                json!({"id":61,"method":"config/value/write","params":{
                "keyPath":format!("hooks.state.{}.trusted_hash", json!(key)), "value":hash,
                "mergeStrategy":"replace", "filePath":config_path}}),
                &mut evidence,
                OUTPUT_BYTES,
                None,
                Some(&cancel),
            )
            .await?;
            let listing = rpc(
                &mut process,
                &input,
                json!({"id":62,"method":"hooks/list","params":{"cwds":[workspace]}}),
                &mut evidence,
                OUTPUT_BYTES,
                None,
                Some(&cancel),
            )
            .await?;
            assert_eq!(listing["data"][0]["hooks"][0]["trustStatus"], "trusted");
            if index == 3 {
                rpc(&mut process, &input, json!({"id":64,"method":"config/value/write","params":{
                    "keyPath":"web_search", "value":"cached", "mergeStrategy":"replace", "filePath":config_path}}),
                    &mut evidence, OUTPUT_BYTES, None, Some(&cancel)).await?;
            }
            if diagnostic {
                // Failed spawn is synchronous: cleanup cannot hide attempted
                // notification behind killing its child before marker creation.
                rpc(&mut process, &input, json!({"id":63,"method":"config/value/write","params":{
                    "keyPath":"notify", "value":[config_home], "mergeStrategy":"replace", "filePath":config_path}}),
                    &mut evidence, OUTPUT_BYTES, None, Some(&cancel)).await?;
            }
            let mut stage = stage();
            stage.native_session_id =
                resume.then(|| retained_thread.clone().expect("positive thread"));
            let mut request = thread_request(&stage, workspace.to_str().unwrap(), &profile, None);
            if !disable {
                let config = request["params"]["config"].as_object_mut().unwrap();
                if !diagnostic {
                    config.remove("features.hooks");
                }
                config.remove("notify");
            }
            let response = rpc(
                &mut process,
                &input,
                request,
                &mut evidence,
                OUTPUT_BYTES,
                None,
                Some(&cancel),
            )
            .await?;
            acknowledge_thread(&stage, &response, &mut evidence)?;
            verify_sandbox(&response, &profile, workspace.to_str().unwrap())?;
            if retained_thread.is_none() {
                retained_thread = evidence.thread.clone();
            }
            let response = rpc(
                &mut process,
                &input,
                turn_request(
                    &stage,
                    evidence.thread.as_deref().unwrap(),
                    workspace.to_str().unwrap(),
                    &profile,
                    None,
                ),
                &mut evidence,
                OUTPUT_BYTES,
                None,
                Some(&cancel),
            )
            .await?;
            acknowledge_turn(&response, &mut evidence)?;
            drive_active_turn(
                &mut process,
                &input,
                &mut evidence,
                None,
                &cancel,
                OUTPUT_BYTES,
                None,
                None,
            )
            .await?;
            if !disable && !diagnostic {
                while !hook_marker.exists() || !notify_marker.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            Ok::<(), &'static str>(())
        };
        let proof = tokio::time::timeout(Duration::from_secs(10), proof).await;
        drop(input);
        let outcome = if matches!(proof, Ok(Ok(()))) {
            process.wait().await
        } else {
            process.cancel_and_wait().await
        }
        .unwrap();
        let settlement = outcome.settlement.unwrap();
        assert!(settlement.ownership.is_authoritative());
        proof
            .unwrap_or_else(|error| {
                panic!(
                    "native control {index}: {error:?}; terminal={:?}, bytes={}, hook={}, notify={}",
                    evidence.terminal,
                    evidence.output.len(),
                    hook_marker.exists(),
                    notify_marker.exists()
                )
            })
            .unwrap();
        assert!(matches!(outcome.end, FramedProcessEnd::Exited));
        assert!(outcome.status.unwrap().success());
        assert!(
            !outcome.stderr_capped,
            "diagnostic absence requires complete capture"
        );
        let stderr = String::from_utf8(outcome.stderr).expect("valid producer diagnostics");
        let notification_attempt = stderr.lines().any(|line| {
            line.contains("codex_core::hook_runtime")
                && line.contains("hook_name=legacy_notify")
                && line.contains("WARN")
        });
        assert_eq!(
            notification_attempt,
            diagnostic && !disable,
            "diagnostic control disable={disable} resume={resume}: {stderr}"
        );
        assert_eq!(evidence.terminal.as_deref(), Some("completed"));
        let expected = if disable {
            "界".repeat(OUTPUT_BYTES / 3)
        } else {
            answer
        };
        assert_eq!(evidence.output, expected);
        assert_eq!(evidence.output_capped, disable);
        assert_eq!(
            hook_marker.exists(),
            !disable && !diagnostic,
            "hook disable={disable} resume={resume}"
        );
        assert_eq!(
            notify_marker.exists(),
            !disable && !diagnostic,
            "notify disable={disable} resume={resume}"
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), index + 1);
        let request: Value = serde_json::from_slice(&requests[index].body).unwrap();
        let search = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["type"] == "web_search");
        if matches!(index, 0 | 5) {
            assert_eq!(
                search.unwrap()["external_web_access"],
                index == 0,
                "allowed networking must preserve configured live/cached search"
            );
        } else {
            assert!(
                search.is_none(),
                "denied start/resume must not expose hosted search"
            );
        }
    }
}

/// Probe actual OS enforcement without model calls or credential inspection.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "installed Codex MCP enforcement; explicit opt-in, synthetic configuration, no model calls"]
async fn live_native_configuration_disables_configured_mcp_with_a_positive_control() {
    assert!(std::env::var_os("ASTRA_NATIVE_HARNESS_SUPERVISOR_BIN").is_some());
    assert_eq!(
        std::env::var("ASTRA_NATIVE_CODEX_HARNESS").as_deref(),
        Ok("1")
    );
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let marker = directory.path().join("mcp-started");
    let server = directory.path().join("mcp.py");
    std::fs::write(&server, format!(r#"
import json,sys,pathlib
pathlib.Path({}).write_text('started')
for line in sys.stdin:
    request=json.loads(line)
    if 'id' not in request: continue
    result={{'protocolVersion':request['params']['protocolVersion'],'capabilities':{{'tools':{{}}}},'serverInfo':{{'name':'synthetic','version':'1'}}}} if request['method']=='initialize' else {{'tools':[]}}
    print(json.dumps({{'jsonrpc':'2.0','id':request['id'],'result':result}}),flush=True)
"#, json!(marker.to_str().unwrap()))).unwrap();
    let executable = native_executable().unwrap();
    let requirements = installed_runtime_requirements().unwrap();
    let profile =
        permission_profile(workspace.to_str().unwrap(), false, false, &requirements).unwrap();
    let mut retained_thread = None;
    // Each phase uses a fresh process: positive startup, cleared startup,
    // then exact resume of the positively configured retained thread.
    for (disable, resume) in [(false, false), (true, false), (true, true)] {
        if marker.exists() {
            std::fs::remove_file(&marker).unwrap();
        }
        let args = vec![
            "app-server".into(),
            "--config".into(),
            "mcp_servers.astra-isolation-probe.command=\"/usr/bin/python3\"".into(),
            "--config".into(),
            format!(
                "mcp_servers.astra-isolation-probe.args={}",
                json!(["-u", server.to_str().unwrap()])
            ),
            "--config".into(),
            "mcp_servers.astra-isolation-probe.enabled=true".into(),
            "--config".into(),
            "features.apps=true".into(),
            "--config".into(),
            "features.plugins=true".into(),
        ];
        let (mut command, owner) = prepare_native_process(&executable, &args).unwrap();
        command.current_dir(&workspace);
        let cancel = CancellationToken::new();
        let mut process = owner
            .spawn_framed(
                command,
                FramedProcessLimits {
                    max_frame_bytes: FRAME_BYTES,
                    max_queued_frames: 1,
                    max_stderr_bytes: 4096,
                    timeout: Some(Duration::from_secs(15)),
                },
                cancel.child_token(),
            )
            .unwrap();
        let input = process.input();
        let mut evidence = Evidence::default();
        let proof = async {
            initialize_protocol(
                &mut process,
                &input,
                &mut evidence,
                OUTPUT_BYTES,
                None,
                &cancel,
            )
            .await?;
            let mut stage = stage();
            stage.native_session_id =
                resume.then(|| retained_thread.clone().expect("positive thread"));
            let mut request = thread_request(&stage, workspace.to_str().unwrap(), &profile, None);
            if !disable {
                request["params"]["config"]
                    .as_object_mut()
                    .unwrap()
                    .remove("mcp_servers");
            }
            let thread = rpc(
                &mut process,
                &input,
                request,
                &mut evidence,
                OUTPUT_BYTES,
                None,
                Some(&cancel),
            )
            .await?;
            verify_sandbox(&thread, &profile, workspace.to_str().unwrap())?;
            let thread_id = thread
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .ok_or("missing thread")?;
            if !disable {
                retained_thread = Some(thread_id.to_owned());
            }
            if resume {
                assert_eq!(Some(thread_id), stage.native_session_id.as_deref());
            }
            loop {
                let status = rpc(
                    &mut process,
                    &input,
                    json!({"id":60,"method":"mcpServerStatus/list",
                    "params":{"threadId":thread_id,"limit":100}}),
                    &mut evidence,
                    OUTPUT_BYTES,
                    None,
                    Some(&cancel),
                )
                .await?;
                let servers = status["data"].as_array().ok_or("missing MCP status")?;
                let state = servers
                    .iter()
                    .find(|item| item["name"] == "astra-isolation-probe")
                    .and_then(|item| item["runtimeStatus"].as_str());
                if (disable && servers.is_empty()) || (!disable && state == Some("connected")) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if !disable {
                // Empty threads have no resumable rollout. This reference-client
                // UI operation records one benign shell result, with no model turn
                // or inference. It is fixture setup, never an Astra execution path.
                let command = json!({"id":61,"method":"thread/shellCommand","params":{
                    "threadId":thread_id,"command":"printf synthetic-history","timeoutMs":1000}});
                input
                    .send_frame(&serde_json::to_vec(&command).unwrap())
                    .await
                    .map_err(|_| "fixture history request failed")?;
                let (mut acknowledged, mut completed) = (false, false);
                while !acknowledged || !completed {
                    let frame = process.recv_frame().await.ok_or("fixture history EOF")?;
                    let event: Value = serde_json::from_slice(&frame)
                        .map_err(|_| "invalid fixture history frame")?;
                    if event["id"] == 61 {
                        if event.get("error").is_some() {
                            return Err("fixture history request rejected");
                        }
                        acknowledged = true;
                    }
                    if event["method"] == "turn/completed"
                        && event["params"]["threadId"] == thread_id
                    {
                        if event["params"]["turn"]["status"] != "completed" {
                            return Err("fixture history turn failed");
                        }
                        completed = true;
                    }
                }
            }
            Ok::<_, &'static str>(())
        };
        let proof = tokio::time::timeout(Duration::from_secs(10), proof).await;
        drop(input);
        let settled = if matches!(&proof, Ok(Ok(()))) {
            process.wait().await.unwrap()
        } else {
            process.cancel_and_wait().await.unwrap()
        };
        assert!(settled.settlement.unwrap().ownership.is_authoritative());
        proof
            .expect("bounded real MCP status proof")
            .unwrap_or_else(|error| {
                panic!(
                    "native MCP phase disable={disable} resume={resume}: {error}; code={:?}",
                    evidence.provider_error_code
                )
            });
        assert_eq!(
            marker.exists(),
            !disable,
            "positive control must start MCP; disabled stage must not"
        );
    }
}

/// Probe actual OS enforcement without model calls or credential inspection.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "real Codex permission enforcement; requires explicit opt-in and fresh supervisor, no model calls"]
async fn live_native_permission_masks_enforce_read_write_and_network_boundaries() {
    assert_eq!(
        std::env::var("ASTRA_NATIVE_CODEX_HARNESS").as_deref(),
        Ok("1")
    );
    assert!(std::env::var_os("ASTRA_NATIVE_HARNESS_SUPERVISOR_BIN").is_some());
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir_all(root.join("nested/.AWS")).unwrap();
    std::fs::write(root.join("control.txt"), "readable-control").unwrap();
    for path in [
        root.join("id_rsa"),
        root.join("nested/.env"),
        root.join("nested/.AWS/credentials"),
        directory.path().join("outside.txt"),
    ] {
        std::fs::write(path, "synthetic-denied-control").unwrap();
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let requirements = installed_runtime_requirements().unwrap();
    let executable = native_executable().unwrap();
    let mut args = vec!["app-server".to_owned()];
    let mut profiles = Vec::new();
    for network in [false, true] {
        let profile =
            permission_profile(root.to_str().unwrap(), false, network, &requirements).unwrap();
        let id = profile["profileId"].as_str().unwrap().to_owned();
        let config = requested_profile_config(&profile).unwrap();
        // CLI dotted keys do not parse quoted path segments. Pass the map
        // as one TOML value, preserving every actual projected rule.
        let filesystem = config["filesystem"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(path, permission)| format!("{}={permission}", json!(path)))
            .collect::<Vec<_>>()
            .join(",");
        args.extend([
            "--config".into(),
            format!("permissions.{id}.filesystem={{{filesystem}}}"),
        ]);
        args.extend([
            "--config".into(),
            format!("permissions.{id}.network.enabled={network}"),
        ]);
        println!(
            "{}",
            json!({"event":"native_permission_projection", "network":network,
            "filesystem_entries":config["filesystem"].as_object().unwrap().len(),
            "filesystem_json_bytes":serde_json::to_vec(&config["filesystem"]).unwrap().len()})
        );
        profiles.push(id);
    }
    args.extend([
        "--config".into(),
        format!("default_permissions={}", json!(profiles[0])),
    ]);
    let (mut command, owner) = prepare_native_process(&executable, &args).unwrap();
    command.current_dir(&root);
    let cancel = CancellationToken::new();
    let mut process = owner
        .spawn_framed(
            command,
            FramedProcessLimits {
                max_frame_bytes: FRAME_BYTES,
                max_queued_frames: 1,
                max_stderr_bytes: 4096,
                timeout: Some(Duration::from_secs(40)),
            },
            cancel.child_token(),
        )
        .unwrap();
    let input = process.input();
    let mut evidence = Evidence::default();
    let probes = async {
        initialize_protocol(&mut process, &input, &mut evidence, OUTPUT_BYTES, None, &cancel).await?;
        let mut results = Vec::new();
        let mut commands = vec![(vec!["/bin/cat".to_owned(), root.join("control.txt").display().to_string()], true, 0)];
        for path in [root.join("id_rsa"), root.join("nested/.env"), root.join("nested/.AWS/credentials"), directory.path().join("outside.txt")] {
            commands.push((vec!["/bin/cat".into(), path.display().to_string()], false, 0));
        }
        commands.push((vec!["/bin/sh".into(), "-c".into(), "printf changed > \"$1\"".into(), "probe".into(), root.join("new-file").display().to_string()], false, 0));
        for profile in 0..2 {
            commands.push((vec!["/usr/bin/python3".into(), "-c".into(), "import socket,sys; socket.create_connection(('127.0.0.1',int(sys.argv[1])),timeout=1).close()".into(), port.clone()], profile == 1, profile));
        }
        for (index, (command, expected_success, profile)) in commands.into_iter().enumerate() {
            let response = rpc(&mut process, &input,
                json!({"id":index+50, "method":"command/exec", "params":{
                    "command":command, "cwd":root, "permissionProfile":profiles[profile],
                    "timeoutMs":3000, "outputBytesCap":2048,
                }}), &mut evidence, OUTPUT_BYTES, None, Some(&cancel)).await?;
            let code = response["exitCode"].as_i64().ok_or("command probe lacks exit evidence")?;
            results.push((index, code == 0, expected_success));
            if index == 0 && response["stdout"].as_str() != Some("readable-control") {
                return Err("positive read control did not return the actual file");
            }
        }
        Ok::<_, &'static str>(results)
    }.await;
    let cleanup = process.cancel_and_wait().await.unwrap();
    if probes.is_err() {
        let (diagnostic, _) =
            astra_text_utils::credential_redaction::redact_credentials_for_display(
                &String::from_utf8_lossy(&cleanup.stderr),
            );
        eprintln!("native permission probe failed after settled cleanup: {diagnostic}");
    }
    assert!(
        cleanup
            .settlement
            .as_ref()
            .is_some_and(|settlement| settlement.ownership.is_authoritative())
    );
    for (index, success, expected) in
        probes.expect("real permission probes failed after settled cleanup")
    {
        assert_eq!(success, expected, "native permission probe {index}");
    }
    assert!(!root.join("new-file").exists());
}

/// Public CLI -> real ServerSubRunExecutor -> durable projection -> Introspect.
/// No fixture-installed sender, invented provider principal or direct adapter
/// invocation can make this pass. Credentials stay with the ordinary CLI.
#[tokio::test]
#[ignore = "paid providers, fresh CLI/server and signed-in account; explicit native parent harness opt-in"]
async fn live_native_parent_retains_observation_through_durable_custody() {
    assert_eq!(
        std::env::var("ASTRA_NATIVE_CODEX_HARNESS").as_deref(),
        Ok("1")
    );
    let binary = std::env::var("ASTRA_NATIVE_HARNESS_SUPERVISOR_BIN").unwrap();
    let api = std::env::var("ASTRA_NATIVE_PARENT_HARNESS_API_URL").unwrap();
    let parent = std::env::var("ASTRA_NATIVE_PARENT_HARNESS_MODEL").unwrap();
    let native = std::env::var("ASTRA_NATIVE_CODEX_HARNESS_MODEL").unwrap();
    let model_reference = std::env::var("ASTRA_NATIVE_PARENT_HARNESS_MODEL_REFERENCE").unwrap();
    let directory = tempfile::tempdir().unwrap();
    let marker = uuid::Uuid::new_v4().to_string();
    let path = directory.path().join("control.txt");
    std::fs::write(&path, &marker).unwrap();

    async fn cli_json(binary: &str, api: &str, cwd: &std::path::Path, args: &[&str]) -> Value {
        let mut command = tokio::process::Command::new(binary);
        command
            .args(["--api-url", api])
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(180), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "public CLI did not complete successfully"
        );
        assert!(output.stdout.len() <= 16 * 1024 * 1024);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    let prompt = format!(
        "使用 Codex 的 {model_reference} 模型、xhigh，读取 {}。只返回文件内容。",
        path.display()
    );
    let result = cli_json(
        &binary,
        &api,
        directory.path(),
        &[
            "chat",
            "--no-resume",
            "--model",
            &parent,
            "--auto-approve",
            "--json",
            "--message",
            &prompt,
        ],
    )
    .await;
    assert_eq!(result["success"], true);
    assert_eq!(result["text"].as_str().unwrap().trim(), marker);
    let session = result["session_id"].as_str().unwrap();
    let root = result["run_id"].as_str().unwrap();
    let capture = cli_json(
        &binary,
        &api,
        directory.path(),
        &[
            "session",
            "show",
            session,
            "--execution",
            "--transcript",
            "--run-events",
        ],
    )
    .await;
    assert_eq!(capture["session_id"], session);
    assert_eq!(capture["run_tree"]["truncated"], false);
    let children = capture["run_tree"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|run| run["parent_run_id"] == root)
        .collect::<Vec<_>>();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["status"], "completed");
    let child = children[0]["run_id"].as_str().unwrap();
    let projection = capture["run_projections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|projection| projection["run_id"] == child)
        .unwrap();
    let terminals = projection["recent_events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "tool_call_end")
        .collect::<Vec<_>>();
    assert_eq!(
        terminals.len(),
        1,
        "native terminal must have one durable custodian"
    );
    assert_eq!(terminals[0]["run_id"], child);
    let observation = &terminals[0]["native_stage_observation"];
    assert_eq!(observation["dispatch_state"], "acknowledged");
    assert_eq!(observation["native_terminal"], "completed");
    assert_eq!(observation["settlement_authoritative"], true);
    assert_eq!(observation["acknowledged_model"], native);
    let parent_calls = capture["transcript"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["run_id"] == root)
        .flat_map(|item| item["tool_calls"].as_array().into_iter().flatten())
        .collect::<Vec<_>>();
    assert!(
        parent_calls.iter().any(|call| call["name"] == "agent"),
        "parent must actually delegate"
    );
    for item in capture["transcript"]["items"].as_array().unwrap() {
        if item["run_id"] == root && item["tool_result"].is_object() {
            assert_eq!(
                item["tool_result"]["status"], "completed",
                "a successful final answer must not hide failed delegation attempts"
            );
        }
    }
    for call in parent_calls {
        assert!(
            call["name"] == "agent",
            "delegation must not require parent discovery, bogus artifacts, or substitute execution"
        );
    }

    // A normal follow-up must continue the previous collaborator. The model
    // may omit selection or repeat its matching selector; deterministic
    // prepare-owner tests cover restoration without relying on that choice.
    let continued = cli_json(
        &binary, &api, directory.path(),
        &["chat", "--session-id", session, "--model", &parent,
            "--auto-approve", "--json", "--message",
            "让刚才那个 Codex 继续：不要再读文件，把它刚才返回的内容原样重复两次，中间用一个冒号分隔。只返回结果。"],
    ).await;
    assert_eq!(continued["success"], true);
    assert_eq!(
        continued["text"].as_str().unwrap().trim(),
        format!("{marker}:{marker}")
    );
    let continued_root = continued["run_id"].as_str().unwrap();
    assert_ne!(continued_root, root);
    let continued_capture = cli_json(
        &binary,
        &api,
        directory.path(),
        &[
            "session",
            "show",
            session,
            "--execution",
            "--transcript",
            "--run-events",
        ],
    )
    .await;
    assert_eq!(continued_capture["run_tree"]["truncated"], false);
    let continued_children = continued_capture["run_tree"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|run| run["parent_run_id"] == continued_root)
        .collect::<Vec<_>>();
    assert_eq!(continued_children.len(), 1);
    let continued_child = continued_children[0]["run_id"].as_str().unwrap();
    assert_ne!(continued_child, child);
    assert_eq!(continued_children[0]["status"], "completed");
    let continued_projection = continued_capture["run_projections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|projection| projection["run_id"] == continued_child)
        .unwrap();
    let continued_terminals = continued_projection["recent_events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "tool_call_end")
        .collect::<Vec<_>>();
    assert_eq!(continued_terminals.len(), 1);
    let continued_observation = &continued_terminals[0]["native_stage_observation"];
    assert_eq!(continued_observation["settlement_authoritative"], true);
    assert_eq!(continued_observation["native_terminal"], "completed");
    assert_eq!(continued_observation["acknowledged_model"], native);
    assert!(
        observation["native_session_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(
        continued_observation["native_session_id"],
        observation["native_session_id"]
    );
    assert_ne!(
        continued_observation["native_turn_id"],
        observation["native_turn_id"]
    );
    let continued_calls = continued_capture["transcript"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["run_id"] == continued_root)
        .flat_map(|item| item["tool_calls"].as_array().into_iter().flatten())
        .collect::<Vec<_>>();
    assert!(!continued_calls.is_empty());
    assert!(continued_calls.iter().all(|call| call["name"] == "agent"));
    let continued_items = continued_capture["transcript"]["items"].as_array().unwrap();
    for item in continued_items
        .iter()
        .filter(|item| item["run_id"] == continued_root)
    {
        if item["tool_result"].is_object() {
            assert_eq!(
                item["tool_result"]["status"], "completed",
                "continuation must not hide failed attempts"
            );
        }
    }
    assert!(
        continued_items
            .iter()
            .any(|item| item["run_id"] == continued_child
                && item["role"] == "assistant"
                && item["content"]
                    .as_str()
                    .is_some_and(|text| text.trim() == format!("{marker}:{marker}"))),
        "the child, not only the parent, must deliver the requested bytes"
    );
    let continuation_call = continued_calls
        .iter()
        .find(|call| {
            continued_items.iter().any(|item| {
                item["run_id"] == continued_root
                    && item["tool_result"]["tool_use_id"] == call["tool_use_id"]
                    && item["tool_result"]["status"] == "completed"
                    && item["content"]
                        .as_str()
                        .and_then(|text| serde_json::from_str::<Value>(text).ok())
                        .is_some_and(|receipt| receipt["run_id"] == continued_child)
            })
        })
        .unwrap();
    let continuation: Value =
        serde_json::from_str(continuation_call["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(continuation["action"], "spawn");
    assert_eq!(continuation["collaborator_id"], child);
    let repeated_execution = &continuation["execution"];
    if !repeated_execution.is_null() {
        let first_call = capture["transcript"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["run_id"] == root)
            .flat_map(|item| item["tool_calls"].as_array().into_iter().flatten())
            .find_map(|call| {
                let arguments: Value = serde_json::from_str(call["arguments"].as_str()?).ok()?;
                (arguments["action"] == "spawn").then_some(arguments)
            })
            .unwrap();
        assert_eq!(repeated_execution["tool"], first_call["execution"]["tool"]);
        assert_eq!(repeated_execution["model"], native);
    }

    let prompt = format!(
        "调用 introspect 获取运行 {child} 的观测，只原样返回其中 native_stage_observations 的 JSON 数组，不要代码围栏。不要重新执行任务。"
    );
    let followup = cli_json(
        &binary,
        &api,
        directory.path(),
        &[
            "chat",
            "--session-id",
            session,
            "--model",
            &parent,
            "--auto-approve",
            "--json",
            "--message",
            &prompt,
        ],
    )
    .await;
    assert_eq!(followup["success"], true);
    let followup_run = followup["run_id"].as_str().unwrap();
    let inspected = cli_json(
        &binary,
        &api,
        directory.path(),
        &["session", "show", session, "--execution", "--transcript"],
    )
    .await;
    let call_ids = inspected["transcript"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["run_id"] == followup_run)
        .filter(|item| item["tool_result"]["name"] == "introspect")
        .map(|item| item["tool_result"]["tool_use_id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(
        !call_ids.is_empty(),
        "followup must actually call Introspect"
    );
    for item in inspected["transcript"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["run_id"] == followup_run && item["tool_result"].is_object())
    {
        assert_eq!(item["tool_result"]["status"], "completed");
    }
    // Both audit and terminal channels intentionally carry previews. Verify
    // actual user delivery, not whether a preview can be parsed as full JSON.
    let delivered: Value = serde_json::from_str(followup["text"].as_str().unwrap()).unwrap();
    let facts = delivered
        .as_array()
        .unwrap()
        .iter()
        .find(|document| document["run_id"] == child)
        .expect("Introspect must deliver the exact child observation to the user");
    assert_eq!(facts["native_stage_observation"], *observation);
    println!(
        "{}",
        json!({"event":"native_parent_custody_verified", "stage_inclusive_input_tokens":observation["stage_inclusive_input_tokens"], "last_request_input_tokens":observation["last_request_input_tokens"], "stage_usage":observation["stage_usage"]})
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "live Codex task; requires explicit opt-in, model, auth, native sandbox and fresh supervisor binary"]
async fn live_native_codex_two_stages_same_session() {
    assert_eq!(
        std::env::var("ASTRA_NATIVE_CODEX_HARNESS").as_deref(),
        Ok("1"),
        "explicit paid-task opt-in is required"
    );
    let model = std::env::var("ASTRA_NATIVE_CODEX_HARNESS_MODEL")
        .expect("ASTRA_NATIVE_CODEX_HARNESS_MODEL must select the requested model");
    assert!(valid_id(&model), "invalid harness model identity");
    let helper = std::path::PathBuf::from(
        std::env::var_os("ASTRA_NATIVE_HARNESS_SUPERVISOR_BIN")
            .expect("set the absolute path of a freshly built Astra supervisor binary"),
    );
    assert!(
        helper.is_absolute() && helper.is_file(),
        "supervisor binary unavailable"
    );
    // Use the caller-selected disk-backed temporary root. Keeping the live
    // workspace outside the checkout prevents Codex from discovering the
    // checkout's parent AGENTS.md while the fixture grants only this root.
    let base = std::env::temp_dir().join("astra-native-codex-harness");
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
    // Declare a representable local read policy, retain a real policy object
    // and impose the actual Codex readOnly OS sandbox. No Bypass/None,
    // dangerFullAccess, externalSandbox, auto-approval or sandbox retries.
    let mut policy = astra_sandbox::SandboxPolicy::permissive(&root);
    // The live provider task is explicitly opt-in and needs the configured
    // Codex provider network. Production execution still takes this only from
    // the immutable admission ceiling; offline contract tests keep network
    // disabled.
    policy.network_allowed = true;
    *astra_core::sync_poison::recover_rwlock_write(&executor.sandbox_policy) = Some(policy);
    executor.set_read_only_execution();
    executor.set_cli_local_provider_schemas(vec![schema()]);
    executor.set_current_visible_tool_schemas(&[schema()]);

    struct RejectUnexpectedInteraction(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl ProviderInteractionGate for RejectUnexpectedInteraction {
        async fn request_interaction(
            &self,
            request: &ProviderInteractionRequest,
        ) -> ProviderInteractionDecision {
            request
                .validate()
                .expect("canonical native question envelope");
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            ProviderInteractionDecision::Cancelled
        }
    }
    let gate = RejectUnexpectedInteraction(std::sync::atomic::AtomicUsize::new(0));
    let cancel = CancellationToken::new();
    let session = format!("native-live-session-{}", uuid::Uuid::now_v7());
    let first_run = format!("native-live-run-{}", uuid::Uuid::now_v7());
    let second_run = format!("native-live-run-{}", uuid::Uuid::now_v7());
    let token = format!("native-history-{}", uuid::Uuid::now_v7().simple());
    std::fs::write(root.join("opaque-token.txt"), &token).unwrap();
    let tasks = [
        "Use the shell to read opaque-token.txt in the current workspace. Remember its contents for our next stage and reply with those contents only. Do not modify files or ask questions.".to_string(),
        "Repeat only the opaque token I asked you to remember in the previous stage. Do not use tools, access files, or ask questions.".to_string(),
    ];
    let mut native_session: Option<String> = None;
    let mut first_turn: Option<String> = None;
    for (index, (run, task)) in [&first_run, &second_run].into_iter().zip(tasks).enumerate() {
        let identity = astra_turn_types::ToolInvocationIdentity::new(
            "native-adapter-live-harness",
            &session,
            run,
            run,
            "native-stage",
        )
        .expect("canonical fixture invocation identity");
        let mut args = json!({"task": task, "anchor_run_id": first_run, "model": model});
        if let Some(id) = &native_session {
            args["native_session_id"] = json!(id);
        }
        // This short live fixture explicitly admits three minutes per stage;
        // it does not test or alter the product's hours/day budget transport.
        let invocation = ToolInvocationMetadata {
            admission_deadline: Some(std::time::Instant::now() + Duration::from_secs(180)),
            run_id: Some(&identity.run_id),
            turn_chain_id: Some(&identity.turn_chain_id),
            tool_call_id: Some(&identity.invocation_id),
            admission_source: Some(ToolInvocationAdmissionSource::ParentApproval),
            command_timeout_cap_ms: None,
            ..ToolInvocationMetadata::default()
        };
        let started = std::time::Instant::now();
        let mut ceiling = fixture_execution_ceiling(&executor);
        ceiling.network_allowed = true;
        // Fixture-owned approval of the captured installed dependency set,
        // not evidence that the production descriptor/admission gate is wired.
        ceiling.runtime_read_paths = installed_runtime_requirements()
            .expect("installed native requirements")
            .read_paths;
        let outcome = executor
            .execute_native_provider_invocation(
                astra_turn_core::provider_resolution::NativeCollaboratorProtocol::CodexAppServer,
                TOOL_NAME,
                &args,
                invocation,
                Some(&cancel),
                &gate,
                &ceiling,
                None,
                tokio::sync::mpsc::channel(1).1,
                &|| Ok(()),
            )
            .await;
        let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let exact_output = outcome.output.trim() == token;
        let fields = outcome.tool_result_fields.as_ref();
        let native = fields
            .and_then(|fields| fields.get("native_collaborator"))
            .cloned()
            .unwrap_or(Value::Null);
        let usage = fields
            .and_then(|fields| fields.get("collaborator_usage"))
            .and_then(|usage| astra_turn_types::CanonicalTokenUsage::from_json(usage).ok());
        let cached = usage.and_then(|usage| usage.cached_input_tokens());
        let inclusive_input = usage
            .and_then(|usage| usage.input_column())
            .map(|input| input as u64);
        let cache_read_percent = cached
            .zip(inclusive_input.filter(|input| *input > 0))
            .map(|(cached, input)| 100.0 * cached as f64 / input as f64);
        let same_session = native_session.as_deref().and_then(|expected| {
            native["native_session_id"]
                .as_str()
                .map(|actual| actual == expected)
        });
        let different_turn = first_turn.as_deref().and_then(|previous| {
            native["native_turn_id"]
                .as_str()
                .map(|actual| actual != previous)
        });
        // A closed field projection, printed even on failure. Never serialize
        // the whole receipt: it may contain private native error/output data.
        println!(
            "{}",
            json!({
                "event": "native_codex_adapter_live_stage",
                "stage": index + 1,
                "elapsed_ms": elapsed_ms,
                "requested_model": model,
                "is_error": outcome.is_error,
                "failure_reason": outcome.output.rsplit_once("Error: ").map(|(_, reason)| reason),
                "output_trim_exact": exact_output,
                "session_acknowledged": native["session_acknowledged"].as_bool(),
                "turn_acknowledged": native["turn_acknowledged"].as_bool(),
                "same_acknowledged_session": same_session,
                "different_acknowledged_turn": different_turn,
                "native_terminal": native["native_terminal"].as_str()
                    .filter(|status| matches!(*status, "completed" | "failed" | "interrupted")),
                "provider_error_code": native["provider_error_code"].as_i64(),
                "provider_error_class": native["provider_error_class"].as_str(),
                "provider_error_service": native["provider_error_service"].as_str(),
                "usage_scope": "stage_delta",
                "usage_observed": usage.is_some(),
                "usage": usage.map(astra_turn_types::CanonicalTokenUsage::to_json),
                "cache_read": {
                    "coverage": cached.is_some() && inclusive_input.is_some(),
                    "cached_input_tokens": cached,
                    "inclusive_input_tokens": inclusive_input,
                    "percent": cache_read_percent
                },
                "cost_usd": null,
                "cost_observed": false,
                "cleanup": {
                    "target_released": native["target_released"].as_bool(),
                    "settlement_authoritative": native["settlement_authoritative"].as_bool(),
                    "transport_settled_after_terminal": native["transport_settled_after_terminal"].as_bool(),
                    "cleanup_cancelled": native["cleanup_cancelled"].as_bool(),
                    "workspace_effect_settled": fields.and_then(|fields| fields.get("workspace_effect_settled")).and_then(Value::as_bool)
                }
            })
        );
        // Do not expose native error text, output, credentials or config in a
        // failed test. Cleanup has been awaited before either assertion.
        assert!(
            !outcome.is_error,
            "native stage {} failed; inspect sanitized owner evidence",
            index + 1
        );
        assert!(
            exact_output,
            "stage {} output was not exactly the remembered token",
            index + 1
        );
        let fields = outcome
            .tool_result_fields
            .expect("native structured receipt");
        let native = &fields["native_collaborator"];
        assert_eq!(native["session_acknowledged"], true);
        assert_eq!(native["turn_acknowledged"], true);
        assert_eq!(native["dispatch_state"], "acknowledged");
        assert_eq!(native["native_terminal"], "completed");
        assert_eq!(native["output_capped"], false);
        assert_eq!(native["target_released"], true);
        assert_eq!(native["settlement_authoritative"], true);
        assert_eq!(native["transport_settled_after_terminal"], true);
        assert_eq!(fields["workspace_effect_settled"], true);
        let observation = astra_turn_types::project_native_collaborator_observation(
            &fields[astra_turn_types::NATIVE_COLLABORATOR_OBSERVATION_KEY],
        )
        .expect("live stage observation survives the shared bounded projection");
        assert_eq!(
            observation["native_session_id"],
            native["native_session_id"]
        );
        assert_eq!(observation["native_turn_id"], native["native_turn_id"]);
        assert_eq!(observation["native_terminal"], "completed");
        assert_eq!(observation["settlement_authoritative"], true);
        let acknowledged: astra_services::runs::CollaboratorNativeSession = serde_json::from_value(
            fields[astra_services::runs::COLLABORATOR_NATIVE_SESSION_METADATA_KEY].clone(),
        )
        .expect("actual native session ACK metadata");
        assert_eq!(
            acknowledged.provider,
            astra_services::runs::CollaboratorProvider::Codex
        );
        assert_eq!(acknowledged.anchor_run_id, first_run);
        assert_eq!(
            native["native_session_id"].as_str(),
            Some(acknowledged.native_session_id.as_str())
        );
        let turn = native["native_turn_id"]
            .as_str()
            .filter(|id| valid_id(id))
            .expect("actual native turn ACK")
            .to_owned();
        if let Some(session) = &native_session {
            assert_eq!(
                &acknowledged.native_session_id, session,
                "resume changed the native session"
            );
            assert_ne!(
                Some(&turn),
                first_turn.as_ref(),
                "second stage reused the native turn"
            );
        } else {
            native_session = Some(acknowledged.native_session_id);
            first_turn = Some(turn);
        }
    }
    assert_eq!(
        gate.0.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "unexpected native interaction was rejected, not auto-approved"
    );
    drop(executor);
    workspace
        .close()
        .expect("remove only the isolated harness workspace");
    assert!(!root.exists(), "workspace cleanup incomplete");
    println!(
        "{}",
        json!({"event": "native_codex_adapter_live_complete", "stages": 2, "same_acknowledged_session": true, "different_acknowledged_turn": true, "workspace_removed": true})
    );
}

#[cfg(target_os = "linux")]
mod transport {
    use super::*;

    #[test]
    fn supervisor_helper() {
        if astra_sandbox::invocation_supervisor_is_requested()
            && let Some(code) = astra_sandbox::run_invocation_supervisor_if_requested()
        {
            std::process::exit(code);
        }
    }

    pub(super) async fn process(script: &str, token: CancellationToken) -> FramedProcess {
        let (command, owner) = BashInvocationOwner::prepare_with_supervisor_helper(
            std::env::current_exe().unwrap(),
            [
                "--exact".into(),
                "edge_tools::native_codex::tests::transport::supervisor_helper".into(),
                "--nocapture".into(),
                "--quiet".into(),
            ],
            "python3",
            &["-u".into(), "-c".into(), script.into()],
        )
        .unwrap();
        let mut process = owner
            .spawn_framed(
                command,
                FramedProcessLimits {
                    max_frame_bytes: FRAME_BYTES,
                    max_queued_frames: 1,
                    max_stderr_bytes: 128,
                    timeout: None,
                },
                token,
            )
            .unwrap();
        // Only the re-exec libtest fixture emits this prelude. Production
        // never strips text or accepts a non-JSON native frame.
        loop {
            let frame = process.recv_frame().await.expect("helper prelude");
            if frame == b"running 1 test" {
                break;
            }
            assert!(frame.is_empty(), "unexpected test helper prelude");
        }
        process
    }

    #[tokio::test]
    async fn silent_negotiation_is_bounded_without_a_task_cutoff() {
        let token = CancellationToken::new();
        let mut process = process("import time; time.sleep(30)", token.clone()).await;
        let mut evidence = Evidence::default();
        let stage = stage();
        let profile = test_profile();
        let error = tokio::time::timeout(
            Duration::from_secs(7),
            drive(
                &mut process,
                &stage,
                "/workspace",
                &profile,
                &mut evidence,
                OUTPUT_BYTES,
                None,
                &token,
            ),
        )
        .await
        .expect("protocol negotiation must have its own bound")
        .unwrap_err();
        assert_eq!(
            error,
            "native negotiation acknowledgement timed out during initialization/authentication"
        );
        assert!(evidence.thread.is_none() && evidence.turn.is_none());
        assert!(evidence.terminal.is_none());
        assert!(evidence.capability_unavailable);
        assert!(
            process
                .cancel_and_wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    const PREFIX: &str = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
assert recv()['method']=='initialize'
emit({'id':1,'result':{'userAgent':'codex-cli test','codexHome':'/tmp/codex','platformFamily':'unix','platformOs':'linux'}})
assert recv()['method']=='initialized'
request=recv()
assert request['id']==7 and request['method']=='account/read'
emit({'id':7,'result':{'account':None,'requiresOpenaiAuth':False}})
request=recv()
assert request['method']=='thread/start'
assert request['params']['approvalPolicy']=='never'
assert request['params']['permissions'].startswith('astra_admitted_')
assert 'config' in request['params']
profile=request['params']['permissions']
assert request['params']['config']['default_permissions']==profile
config=request['params']['config']['permissions'][profile]
assert 'extends' not in config
assert config['filesystem']['/workspace']=='read'
assert request['params']['config']['mcp_servers']=={}
assert request['params']['config']['features.apps'] is False
assert request['params']['config']['features.plugins'] is False
emit({'id':2,'result':{'thread':{'id':'thread','status':{'type':'idle'}},'model':request['params'].get('model'),'cwd':'/workspace','approvalPolicy':'never','approvalsReviewer':'user','activePermissionProfile':{'id':profile},'sandbox':{'type':'readOnly','networkAccess':False}}})
request=recv()
assert request['method']=='turn/start'
assert request['params']['approvalPolicy']=='never'
assert request['params']['permissions']==profile
assert 'sandboxPolicy' not in request['params']
"#;

    #[tokio::test]
    async fn acknowledged_active_turn_outlives_the_negotiation_deadline() {
        let script = format!(
            "{PREFIX}{}",
            r#"
import time
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
time.sleep(6)
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
for line in sys.stdin: pass
"#
        );
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let mut evidence = Evidence::default();
        tokio::time::timeout(
            Duration::from_secs(9),
            drive(
                &mut process,
                &stage(),
                "/workspace",
                &test_profile(),
                &mut evidence,
                OUTPUT_BYTES,
                None,
                &token,
            ),
        )
        .await
        .expect("the peer completes within the test budget")
        .expect("negotiation deadline must not stop acknowledged work");
        assert_eq!(evidence.terminal.as_deref(), Some("completed"));
        assert!(
            process
                .wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    #[tokio::test]
    async fn model_selector_uses_standard_provider_catalog_before_starting_a_thread() {
        let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
assert recv()['method']=='initialize'
emit({'id':1,'result':{'userAgent':'codex-cli test','codexHome':'/tmp/codex','platformFamily':'unix','platformOs':'linux'}})
assert recv()['method']=='initialized'
request=recv()
assert request['id']==7 and request['method']=='account/read'
emit({'id':7,'result':{'account':None,'requiresOpenaiAuth':False}})
request=recv()
assert request['id']==4
assert request['method']=='model/list'
assert request['params']=={'limit':64,'includeHidden':True}
emit({'id':4,'result':{'data':[
    {'id':'luna-56','model':'gpt-5.6-luna','displayName':'GPT-5.6-Luna'}
], 'nextCursor':'next'}})
request=recv()
assert request['id']==4 and request['method']=='model/list'
assert request['params']['cursor']=='next'
emit({'id':4,'result':{'data':[
    {'id':'luna-6','model':'gpt-6-luna','displayName':'GPT-6-Luna'}
], 'nextCursor':None}})
request=recv()
assert request['id']==2
assert request['method']=='thread/start'
assert request['params']['model']=='gpt-5.6-luna'
assert request['params']['approvalPolicy']=='never'
profile=request['params']['permissions']
emit({'id':2,'result':{'thread':{'id':'thread','status':{'type':'idle'}},'model':'gpt-5.6-luna','cwd':'/workspace','approvalPolicy':'never','approvalsReviewer':'user','activePermissionProfile':{'id':profile},'sandbox':{'type':'readOnly','networkAccess':False}}})
request=recv()
assert request['id']==3
assert request['method']=='turn/start'
assert request['params']['model']=='gpt-5.6-luna'
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
for line in sys.stdin: pass
"#;
        let token = CancellationToken::new();
        let mut process = process(script, token.clone()).await;
        let mut stage = stage();
        stage.model = Some("gPt-5.6-lUnA".into());
        let mut evidence = Evidence::default();
        drive(
            &mut process,
            &stage,
            "/workspace",
            &test_profile(),
            &mut evidence,
            OUTPUT_BYTES,
            None,
            &token,
        )
        .await
        .unwrap();
        assert_eq!(evidence.terminal.as_deref(), Some("completed"));
        assert!(
            process
                .wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    #[tokio::test]
    async fn discovered_provider_catalog_is_reused_without_a_second_model_list() {
        let script = format!(
            "{PREFIX}{}",
            r#"
assert request['params']['model']=='provider-model-v2'
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
for line in sys.stdin: pass
"#
        );
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let mut stage = stage();
        stage.model = Some("v2".into());
        let catalog = astra_turn_types::ProviderModelCatalog::new(vec![
            astra_turn_types::ProviderModelDescriptor {
                selector: "provider-model-v2".into(),
                display_name: "Provider Model V2".into(),
                aliases: vec!["v2".into()],
                reasoning_efforts: vec!["high".into()],
                hidden: false,
            },
        ])
        .unwrap();
        let mut evidence = Evidence::default();
        drive_with_cached_catalog(
            &mut process,
            &stage,
            "/workspace",
            &test_profile(),
            &mut evidence,
            OUTPUT_BYTES,
            None,
            &token,
            None,
            Some(&catalog),
        )
        .await
        .unwrap();
        assert_eq!(
            evidence.resolved_model.as_deref(),
            Some("provider-model-v2")
        );
        assert_eq!(evidence.terminal.as_deref(), Some("completed"));
        assert!(
            process
                .wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    #[tokio::test]
    async fn cached_catalog_effort_rejection_refreshes_before_failing() {
        let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
assert recv()['method']=='initialize'
emit({'id':1,'result':{'userAgent':'codex-cli test','codexHome':'/tmp/codex','platformFamily':'unix','platformOs':'linux'}})
assert recv()['method']=='initialized'
request=recv()
assert request['id']==7 and request['method']=='account/read'
emit({'id':7,'result':{'account':None,'requiresOpenaiAuth':False}})
request=recv()
assert request['id']==4 and request['method']=='model/list'
emit({'id':4,'result':{'data':[{
    'id':'v2',
    'model':'provider-model-v2',
    'displayName':'Provider Model V2',
    'supportedReasoningEfforts':[{'reasoningEffort':'xhigh'}]
}], 'nextCursor':None}})
request=recv()
assert request['method']=='thread/start'
assert request['params']['model']=='provider-model-v2'
emit({'id':2,'result':{'thread':{'id':'thread','status':{'type':'idle'}},'model':request['params']['model'],'cwd':'/workspace','approvalPolicy':'never','approvalsReviewer':'user','activePermissionProfile':{'id':request['params']['permissions']},'sandbox':{'type':'readOnly','networkAccess':False}}})
request=recv()
assert request['method']=='turn/start'
assert request['params']['model']=='provider-model-v2'
assert request['params']['effort']=='xhigh'
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
for line in sys.stdin: pass
"#;
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let mut stage = stage();
        stage.model = Some("v2".into());
        stage.effort = Some("xhigh".into());
        let catalog = astra_turn_types::ProviderModelCatalog::new(vec![
            astra_turn_types::ProviderModelDescriptor {
                selector: "provider-model-v2".into(),
                display_name: "Provider Model V2".into(),
                aliases: vec!["v2".into()],
                reasoning_efforts: vec!["high".into()],
                hidden: false,
            },
        ])
        .unwrap();
        let mut evidence = Evidence::default();
        drive_with_cached_catalog(
            &mut process,
            &stage,
            "/workspace",
            &test_profile(),
            &mut evidence,
            OUTPUT_BYTES,
            None,
            &token,
            None,
            Some(&catalog),
        )
        .await
        .unwrap();
        assert_eq!(
            evidence.resolved_model.as_deref(),
            Some("provider-model-v2")
        );
        assert_eq!(evidence.terminal.as_deref(), Some("completed"));
        assert!(
            process
                .wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    #[tokio::test]
    async fn rejected_model_selection_preserves_healthy_capacity_and_never_starts_a_thread() {
        let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
assert recv()['method']=='initialize'
emit({'id':1,'result':{'userAgent':'codex-cli test','codexHome':'/tmp/codex','platformFamily':'unix','platformOs':'linux'}})
assert recv()['method']=='initialized'
request=recv()
assert request['id']==7 and request['method']=='account/read'
emit({'id':7,'result':{'account':None,'requiresOpenaiAuth':False}})
request=recv()
assert request['id']==4 and request['method']=='model/list'
assert request['params']['includeHidden'] is True
emit({'id':4,'result':json.loads(r'''__CATALOG__''')})
for line in sys.stdin:
    request=json.loads(line)
    assert request['method'] not in ('thread/start','thread/resume','turn/start')
"#;
        let ambiguous = json!([
            {"id":"v1", "model":"provider-v1", "displayName":"Shared"},
            {"id":"v2", "model":"provider-v2", "displayName":"Shared"}
        ]);
        let limited_effort = json!([{"id":"v1", "model":"provider-v1", "displayName":"Single",
            "supportedReasoningEfforts":[{"reasoningEffort":"high"}]}]);
        for (requested, effort, models, expected_error, selection, unavailable) in [
            (
                "Shared",
                None,
                ambiguous,
                "Shared (provider-v1)",
                Some("requires_user_choice"),
                false,
            ),
            (
                "missing-model",
                None,
                json!([]),
                "not available from this provider",
                Some("unavailable"),
                false,
            ),
            (
                "v1",
                Some("xhigh"),
                limited_effort,
                "reasoning",
                None,
                false,
            ),
            (
                "v1",
                None,
                json!([
                    {"id":"v1","model":"provider-v1","displayName":"First"},
                    {"id":"provider-v1","model":"provider-v2","displayName":"Second"}
                ]),
                "model catalog",
                None,
                true,
            ),
        ] {
            let script = script.replace(
                "__CATALOG__",
                &json!({"data":models,"nextCursor":null}).to_string(),
            );
            let token = CancellationToken::new();
            let mut process = process(&script, token.clone()).await;
            let mut stage = stage();
            stage.model = Some(requested.into());
            stage.effort = effort.map(str::to_owned);
            let mut evidence = Evidence::default();
            let error = drive(
                &mut process,
                &stage,
                "/workspace",
                &test_profile(),
                &mut evidence,
                OUTPUT_BYTES,
                None,
                &token,
            )
            .await
            .unwrap_err();
            assert!(error.contains(expected_error), "{error}");
            assert_eq!(
                evidence
                    .model_selection
                    .as_ref()
                    .and_then(|value| value["status"].as_str()),
                selection
            );
            assert_eq!(evidence.capability_unavailable, unavailable);
            assert!(evidence.thread.is_none());
            let outcome = process.cancel_and_wait().await.unwrap();
            assert!(outcome.settlement.unwrap().ownership.is_authoritative());
        }
    }

    #[tokio::test]
    async fn startup_protocol_failures_withdraw_capacity_but_active_thread_refusal_does_not() {
        for (script, unavailable) in [
            (PREFIX.replace("'status':{'type':'idle'}", "'status':{}"), true),
            (PREFIX.replace("'status':{'type':'idle'}", "'status':{'type':''}"), true),
            (PREFIX.replace("emit({'id':2,", "emit({'id':99,"), true),
            (PREFIX.replace("request=recv()\nassert request['method']=='thread/start'", "emit({'id':99,'method':'unexpected/request','params':{}})\nrequest=recv()\nassert request['method']=='thread/start'"), true),
            (PREFIX.replace("'status':{'type':'idle'}", "'status':{'type':'active'}"), false),
        ] {
            let token = CancellationToken::new();
            let mut process = process(&script, token.clone()).await;
            let mut evidence = Evidence::default();
            drive(&mut process, &stage(), "/workspace", &test_profile(), &mut evidence,
                OUTPUT_BYTES, None, &token).await.unwrap_err();
            assert_eq!(evidence.capability_unavailable, unavailable);
            assert!(evidence.turn.is_none());
            assert!(process.cancel_and_wait().await.unwrap().settlement.unwrap().ownership.is_authoritative());
        }
    }

    #[tokio::test]
    async fn model_acknowledgement_mismatch_stops_before_turn_start() {
        let script = r#"
import json,select,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
assert recv()['method']=='initialize'
emit({'id':1,'result':{'userAgent':'codex-cli test','codexHome':'/tmp/codex','platformFamily':'unix','platformOs':'linux'}})
assert recv()['method']=='initialized'
request=recv()
assert request['id']==7 and request['method']=='account/read'
emit({'id':7,'result':{'account':None,'requiresOpenaiAuth':False}})
request=recv()
assert request['id']==4 and request['method']=='model/list'
emit({'id':4,'result':{'data':[{'id':'luna-56','model':'gpt-5.6-luna','displayName':'GPT-5.6-Luna'}], 'nextCursor':None}})
request=recv()
assert request['id']==2 and request['method']=='thread/start'
profile=request['params']['permissions']
emit({'id':2,'result':{'thread':{'id':'thread','status':{'type':'idle'}},'model':'gpt-6-luna','cwd':'/workspace','approvalPolicy':'never','approvalsReviewer':'user','activePermissionProfile':{'id':profile},'sandbox':{'type':'readOnly','networkAccess':False}}})
ready,_,_=select.select([sys.stdin],[],[],1.0)
if ready:
    line=sys.stdin.readline()
    if line:
        request=json.loads(line)
        assert request['method']!='turn/start'
"#;
        let token = CancellationToken::new();
        let mut process = process(script, token.clone()).await;
        let mut stage = stage();
        stage.model = Some("gpt-5.6-luna".into());
        let mut evidence = Evidence::default();
        let error = drive(
            &mut process,
            &stage,
            "/workspace",
            &test_profile(),
            &mut evidence,
            OUTPUT_BYTES,
            None,
            &token,
        )
        .await
        .unwrap_err();
        assert_eq!(error, "native acknowledged a different model");
        assert!(evidence.turn.is_none());
        assert!(
            process
                .wait()
                .await
                .unwrap()
                .status
                .is_some_and(|status| status.success())
        );
    }

    #[tokio::test]
    async fn real_owner_preserves_terminal_and_delta_before_turn_ack() {
        let script = format!(
            "{PREFIX}{}",
            r#"
emit({'method':'item/agentMessage/delta','params':{'threadId':'thread','turnId':'turn','itemId':'answer','delta':'review complete'}})
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
for line in sys.stdin: pass
"#
        );
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let mut evidence = Evidence::default();
        drive(
            &mut process,
            &stage(),
            "/workspace",
            &test_profile(),
            &mut evidence,
            OUTPUT_BYTES,
            None,
            &token,
        )
        .await
        .unwrap();
        assert_eq!(evidence.output, "review complete");
        assert_eq!(evidence.terminal.as_deref(), Some("completed"));
        let outcome = process.wait().await.unwrap();
        assert!(outcome.status.unwrap().success());
        assert!(outcome.settlement.unwrap().ownership.is_authoritative());
    }

    #[tokio::test]
    async fn real_owner_resume_replay_and_usage_receipt_are_fenced_to_current_stage() {
        let script = format!(
            "{}{}",
            PREFIX.replace("=='thread/start'", "=='thread/resume'"),
            r#"
# Resume usage replay arrives after its ACK but before the new turn ACK.
before={'inputTokens':100,'cachedInputTokens':40,'cacheWriteInputTokens':20,'outputTokens':10,'reasoningOutputTokens':2,'totalTokens':110}
emit({'method':'thread/tokenUsage/updated','params':{'threadId':'thread','turnId':'old-turn','tokenUsage':{'total':before,'last':before}}})
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
after={'inputTokens':150,'cachedInputTokens':60,'cacheWriteInputTokens':30,'outputTokens':20,'reasoningOutputTokens':4,'totalTokens':170}
for i in range(2): emit({'method':'thread/tokenUsage/updated','params':{'threadId':'thread','turnId':'turn','tokenUsage':{'total':after,'last':before}}})
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
for line in sys.stdin: pass
"#
        );
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let mut stage = stage();
        stage.native_session_id = Some("thread".into());
        let mut evidence = Evidence::default();
        drive(
            &mut process,
            &stage,
            "/workspace",
            &test_profile(),
            &mut evidence,
            OUTPUT_BYTES,
            None,
            &token,
        )
        .await
        .unwrap();
        assert_eq!(
            evidence.stage_usage().unwrap().unwrap().to_json(),
            json!({"input_tokens":20,"cached_input_tokens":20,"cache_creation_tokens":10,"output_tokens":10,"total_tokens":60})
        );
        assert_eq!(evidence.terminal.as_deref(), Some("completed"));
        assert!(
            process
                .wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    #[tokio::test]
    async fn real_owner_eof_is_not_terminal_or_zero_usage() {
        let script = format!(
            "{PREFIX}{}",
            "emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})"
        );
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let mut evidence = Evidence::default();
        assert!(
            drive(
                &mut process,
                &stage(),
                "/workspace",
                &test_profile(),
                &mut evidence,
                OUTPUT_BYTES,
                None,
                &token
            )
            .await
            .is_err()
        );
        assert_eq!(evidence.turn.as_deref(), Some("turn"));
        assert!(evidence.terminal.is_none());
        assert!(evidence.usage.is_none());
        assert!(
            process
                .cancel_and_wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    pub(super) struct AnswerGate;
    #[async_trait::async_trait]
    impl ProviderInteractionGate for AnswerGate {
        async fn request_interaction(
            &self,
            request: &ProviderInteractionRequest,
        ) -> ProviderInteractionDecision {
            assert_eq!(request.payload["native_request_id"], "native-question");
            assert_eq!(request.payload["params"]["questions"][0]["id"], "q");
            ProviderInteractionDecision::Submitted(
                json!({"answers": {"q": {"answers": ["Proceed"]}}}),
            )
        }
    }

    #[tokio::test]
    async fn real_owner_question_before_ack_preserves_request_and_reply() {
        let script = format!(
            "{PREFIX}{}",
            r#"
emit({'id':'native-question','method':'item/tool/requestUserInput','params':{'threadId':'thread','turnId':'turn','itemId':'item','isBlocking':True,'questions':[{'id':'q','header':'Choice','question':'Proceed?'}]}})
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
answer=recv()
assert answer=={'id':'native-question','result':{'answers':{'q':{'answers':['Proceed']}}}}
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
for line in sys.stdin: pass
"#
        );
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let mut evidence = Evidence::default();
        drive(
            &mut process,
            &stage(),
            "/workspace",
            &test_profile(),
            &mut evidence,
            OUTPUT_BYTES,
            Some(&AnswerGate),
            &token,
        )
        .await
        .unwrap();
        assert_eq!(evidence.terminal.as_deref(), Some("completed"));
        assert!(
            process
                .wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn real_owner_steer_waits_for_interaction_reply_before_returning_ack() {
        let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
request=recv()
assert request['method']=='turn/steer'
emit({'id':'native-question','method':'item/tool/requestUserInput','params':{'threadId':'thread','turnId':'turn','itemId':'item','isBlocking':True,'questions':[{'id':'q','header':'Choice','question':'Proceed?'}]}})
emit({'id':request['id'],'result':{'turnId':'turn'}})
answer=recv()
assert answer=={'id':'native-question','result':{'answers':{'q':{'answers':['Proceed']}}}}
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
for line in sys.stdin: pass
"#;
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let mut evidence = Evidence {
            thread: Some("thread".into()),
            turn: Some("turn".into()),
            ..Evidence::default()
        };
        let input = process.input();
        let mut input_rx = None;
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let ack = submit_stage_input(
            &mut process,
            &input,
            &mut evidence,
            Some(&AnswerGate),
            &token,
            ProviderStageInput::Text {
                input_id: "steer-with-question".into(),
                content: "continue after checking the question".into(),
                correlation_id: None,
                expected_turn_id: Some("turn".into()),
            },
            OUTPUT_BYTES,
            ack_tx,
            &mut input_rx,
        )
        .await;
        assert!(ack.is_ok());
        assert!(ack_rx.await.unwrap().accepted);
        assert!(
            process
                .cancel_and_wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    struct DelayedGate;

    #[async_trait::async_trait]
    impl ProviderInteractionGate for DelayedGate {
        async fn request_interaction(
            &self,
            _: &ProviderInteractionRequest,
        ) -> ProviderInteractionDecision {
            tokio::time::sleep(Duration::from_secs(6)).await;
            ProviderInteractionDecision::Cancelled
        }
    }

    struct RecordingQuestionGate {
        seen: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl ProviderInteractionGate for RecordingQuestionGate {
        async fn request_interaction(
            &self,
            request: &ProviderInteractionRequest,
        ) -> ProviderInteractionDecision {
            *self.seen.lock().expect("question identity lock") = Some(request.request_id.clone());
            ProviderInteractionDecision::Cancelled
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn question_after_provider_ack_retains_exact_question_identity() {
        let script = format!(
            "{PREFIX}{}",
            r#"
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
request=recv()
assert request['method']=='turn/steer'
emit({'id':request['id'],'result':{'turnId':'turn'}})
emit({'id':'native-question','method':'item/tool/requestUserInput','params':{'threadId':'thread','turnId':'turn','itemId':'item','isBlocking':True,'questions':[{'id':'q','header':'Choice','question':'Proceed?'}]}})
for line in sys.stdin: pass
"#
        );
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let (input_tx, input_rx) = tokio::sync::mpsc::channel(1);
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        input_tx
            .send(EdgeInvocationInput {
                input: ProviderStageInput::Text {
                    input_id: "accepted-before-question".into(),
                    content: "continue before asking".into(),
                    correlation_id: None,
                    expected_turn_id: Some("turn".into()),
                },
                ack: ack_tx,
            })
            .await
            .unwrap();
        drop(input_tx);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let gate = RecordingQuestionGate { seen: seen.clone() };
        let mut evidence = Evidence::default();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            drive_with_input(
                &mut process,
                &stage(),
                "/workspace",
                &test_profile(),
                &mut evidence,
                OUTPUT_BYTES,
                Some(&gate),
                &token,
                Some(input_rx),
            ),
        )
        .await
        .expect("question after the steer ACK must be handled promptly");
        assert!(result.is_err());
        assert!(ack_rx.await.unwrap().accepted);
        assert_eq!(
            seen.lock().expect("question identity lock").as_deref(),
            Some("\"native-question\"")
        );
        assert!(
            process
                .cancel_and_wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    struct BlockingAnswerGate {
        entered: std::sync::Arc<tokio::sync::Notify>,
        release: std::sync::Arc<tokio::sync::Notify>,
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn usage_reset_keeps_native_guidance_output_and_terminal_live() {
        let script = format!(
            "{PREFIX}{}",
            r#"
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
def usage(input,output,total):
    counters={'inputTokens':input,'cachedInputTokens':0,'outputTokens':output,'reasoningOutputTokens':0,'totalTokens':total}
    emit({'method':'thread/tokenUsage/updated','params':{'threadId':'thread','turnId':'turn','tokenUsage':{'total':counters,'last':counters}}})
usage(100,10,110)
usage(0,0,64000)
usage(150,20,170)
steer=recv()
assert steer['method']=='turn/steer'
emit({'id':steer['id'],'result':{'turnId':'turn'}})
emit({'method':'item/agentMessage/delta','params':{'threadId':'thread','turnId':'turn','itemId':'answer','delta':'still working'}})
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
for line in sys.stdin: assert json.loads(line).get('method')!='turn/interrupt'
"#
        );
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        tx.send(EdgeInvocationInput {
            input: ProviderStageInput::Text {
                input_id: "after-reset".into(),
                content: "continue".into(),
                correlation_id: None,
                expected_turn_id: Some("turn".into()),
            },
            ack: ack_tx,
        })
        .await
        .unwrap();
        drop(tx);
        let mut evidence = Evidence::default();
        tokio::time::timeout(
            Duration::from_secs(3),
            drive_with_input(
                &mut process,
                &stage(),
                "/workspace",
                &test_profile(),
                &mut evidence,
                OUTPUT_BYTES,
                None,
                &token,
                Some(rx),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(ack_rx.await.unwrap().accepted);
        assert_eq!(evidence.output, "still working");
        assert_eq!(evidence.terminal.as_deref(), Some("completed"));
        assert!(evidence.stage_usage().unwrap().is_none());
        assert!(!evidence.capability_unavailable);
        assert!(
            process
                .wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    #[async_trait::async_trait]
    impl ProviderInteractionGate for BlockingAnswerGate {
        async fn request_interaction(
            &self,
            _: &ProviderInteractionRequest,
        ) -> ProviderInteractionDecision {
            self.entered.notify_one();
            self.release.notified().await;
            ProviderInteractionDecision::Submitted(
                json!({"answers": {"q": {"answers": ["Proceed"]}}}),
            )
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn question_and_guidance_are_independent_of_ack_order() {
        for (answer_before_ack, rejected) in [(false, false), (true, false), (false, true)] {
            let ack_order = if rejected {
                "emit({'id':steer['id'],'error':{'code':-32600,'message':'guidance rejected'}})\nanswer=recv()\nassert answer=={'id':'native-question','result':{'answers':{'q':{'answers':['Proceed']}}}}"
            } else if answer_before_ack {
                "answer=recv()\nassert answer=={'id':'native-question','result':{'answers':{'q':{'answers':['Proceed']}}}}\nemit({'id':steer['id'],'result':{'turnId':'turn'}})"
            } else {
                "emit({'id':steer['id'],'result':{'turnId':'turn'}})\nanswer=recv()\nassert answer=={'id':'native-question','result':{'answers':{'q':{'answers':['Proceed']}}}}"
            };
            let script = format!(
                "{PREFIX}{}{ack_order}{}",
                r#"
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
emit({'id':'native-question','method':'item/tool/requestUserInput','params':{'threadId':'thread','turnId':'turn','itemId':'item','isBlocking':True,'questions':[{'id':'q','header':'Choice','question':'Proceed?'}]}})
steer=recv()
assert steer['method']=='turn/steer'
assert steer['params']['clientUserMessageId']=='guidance-after-question'
assert steer['params']['input'][0]['text']=='please also check the new constraint'
"#,
                r#"
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
for line in sys.stdin: pass
"#
            );
            let token = CancellationToken::new();
            let mut process = process(&script, token.clone()).await;
            let entered = std::sync::Arc::new(tokio::sync::Notify::new());
            let release = std::sync::Arc::new(tokio::sync::Notify::new());
            let gate = BlockingAnswerGate {
                entered: entered.clone(),
                release: release.clone(),
            };
            let (input_tx, input_rx) = tokio::sync::mpsc::channel(1);
            let (ack_tx, mut ack_rx) = tokio::sync::oneshot::channel();
            let mut evidence = Evidence::default();
            let stage = stage();
            let profile = test_profile();
            let mut driving = Box::pin(drive_with_input(
                &mut process,
                &stage,
                "/workspace",
                &profile,
                &mut evidence,
                OUTPUT_BYTES,
                Some(&gate),
                &token,
                Some(input_rx),
            ));
            tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                _ = entered.notified() => {},
                result = &mut driving => panic!("drive ended before the provider question: {result:?}"),
            }
        })
        .await
        .expect("provider question must reach the interaction gate");
            input_tx
                .send(EdgeInvocationInput {
                    input: ProviderStageInput::Text {
                        input_id: "guidance-after-question".into(),
                        content: "please also check the new constraint".into(),
                        correlation_id: None,
                        expected_turn_id: Some("turn".into()),
                    },
                    ack: ack_tx,
                })
                .await
                .unwrap();
            if answer_before_ack {
                // A channel reservation proves the dispatcher dequeued guidance;
                // the provider cannot ACK it until it receives the question answer.
                tokio::time::timeout(Duration::from_secs(1), async {
                tokio::select! {
                    permit = input_tx.reserve() => drop(permit.unwrap()),
                    result = &mut driving => panic!("drive ended before taking guidance: {result:?}"),
                }
            }).await.expect("guidance must be dequeued while the question is pending");
                release.notify_one();
            }
            drop(input_tx);
            let mut finished = None;
            let ack = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                ack = &mut ack_rx => ack.expect("guidance ACK sender must remain live"),
                    result = &mut driving => {
                        finished = Some(result);
                        (&mut ack_rx).await.expect("a completed stage must retain its published ACK")
                    },
            }
        })
        .await
        .expect("question-first guidance must be classified before the ACK deadline");
            assert_eq!(ack.accepted, !rejected);
            assert_eq!(ack.input_id, "guidance-after-question");
            if !rejected {
                assert_eq!(ack.provider_turn_id.as_deref(), Some("turn"));
            }
            if !answer_before_ack {
                release.notify_one();
            }
            if let Some(result) = finished {
                drop(driving);
                result.unwrap();
            } else {
                driving.await.unwrap();
            }
            assert_eq!(evidence.terminal.as_deref(), Some("completed"));
            assert!(
                process
                    .wait()
                    .await
                    .unwrap()
                    .settlement
                    .unwrap()
                    .ownership
                    .is_authoritative()
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn unknown_or_timed_out_ack_ignores_late_acceptance_and_disables_queued_input() {
        for expired in [false, true] {
            let script = format!(
                r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
steer=recv()
assert steer['method']=='turn/steer'
if {internal}:
    emit({{'id':steer['id'],'error':{{'code':-32603,'message':'unknown adoption'}}}})
emit({{'id':'native-question','method':'item/tool/requestUserInput','params':{{'threadId':'thread','turnId':'turn','itemId':'item','questions':[{{'id':'q','header':'Choice','question':'Proceed?'}}]}}}})
answer=recv()
assert answer['id']=='native-question', 'a second steer must not be sent'
emit({{'id':steer['id'],'result':{{'turnId':'turn'}}}})
emit({{'method':'turn/completed','params':{{'threadId':'thread','turn':{{'id':'turn','status':'completed'}}}}}})
for line in sys.stdin: assert json.loads(line).get('method') != 'turn/steer'
"#,
                internal = if expired { "False" } else { "True" }
            );
            let token = CancellationToken::new();
            let mut process = process(&script, token.clone()).await;
            let input = process.input();
            let mut evidence = Evidence {
                thread: Some("thread".into()),
                turn: Some("turn".into()),
                ..Evidence::default()
            };
            let entered = std::sync::Arc::new(tokio::sync::Notify::new());
            let release = std::sync::Arc::new(tokio::sync::Notify::new());
            let gate = BlockingAnswerGate {
                entered: entered.clone(),
                release: release.clone(),
            };
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            let text = |id: &str| ProviderStageInput::Text {
                input_id: id.into(),
                content: "guidance".into(),
                correlation_id: None,
                expected_turn_id: Some("turn".into()),
            };
            let mut pending = start_stage_input(
                &input,
                &mut evidence,
                EdgeInvocationInput {
                    input: text("first"),
                    ack: ack_tx,
                },
            )
            .await
            .unwrap()
            .unwrap();
            if expired {
                pending.deadline = tokio::time::Instant::now();
            }
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let mut driving = Box::pin(drive_active_turn(
                &mut process,
                &input,
                &mut evidence,
                Some(&gate),
                &token,
                OUTPUT_BYTES,
                Some(rx),
                Some(pending),
            ));
            tokio::select! {
                _ = entered.notified() => {}
                result = &mut driving => panic!("question must remain live: {result:?}"),
            }
            tokio::select! {
                result = ack_rx => assert!(result.is_err()),
                result = &mut driving => panic!("unknown adoption must preserve the live question: {result:?}"),
            }
            let (second_ack_tx, second_ack_rx) = tokio::sync::oneshot::channel();
            tx.send(EdgeInvocationInput {
                input: text("second"),
                ack: second_ack_tx,
            })
            .await
            .unwrap();
            release.notify_one();
            tokio::time::timeout(Duration::from_secs(2), driving)
                .await
                .unwrap()
                .unwrap();
            assert!(second_ack_rx.await.is_err());
            assert_eq!(evidence.terminal.as_deref(), Some("completed"));
            // The fixture drains stdin after publishing its terminal frame.
            // Release our input clone instead of relying on a task cutoff
            // to kill an otherwise healthy protocol process.
            drop(input);
            assert!(
                process
                    .wait()
                    .await
                    .unwrap()
                    .settlement
                    .unwrap()
                    .ownership
                    .is_authoritative()
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn conclusive_guidance_refusal_does_not_abort_the_native_task() {
        let script = format!(
            "{PREFIX}{}",
            r#"
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
request=recv()
assert request['method']=='turn/steer'
emit({'id':request['id'],'error':{'code':-32600,'message':'not accepted'}})
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
for line in sys.stdin: pass
"#
        );
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let (input_tx, input_rx) = tokio::sync::mpsc::channel(1);
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        input_tx
            .send(EdgeInvocationInput {
                input: ProviderStageInput::Text {
                    input_id: "steer-error".into(),
                    content: "continue".into(),
                    correlation_id: None,
                    expected_turn_id: Some("turn".into()),
                },
                ack: ack_tx,
            })
            .await
            .unwrap();
        drop(input_tx);
        let stage = stage();
        let profile = test_profile();
        let mut evidence = Evidence::default();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            drive_with_input(
                &mut process,
                &stage,
                "/workspace",
                &profile,
                &mut evidence,
                OUTPUT_BYTES,
                None,
                &token,
                Some(input_rx),
            ),
        )
        .await
        .expect("steer failure must not leave the stage waiting");
        assert!(result.is_ok());
        assert!(!ack_rx.await.unwrap().accepted);
        assert_eq!(evidence.terminal.as_deref(), Some("completed"));
        assert!(
            process
                .cancel_and_wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn real_owner_steer_ack_is_sent_before_a_slow_interaction_finishes() {
        let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
request=recv()
assert request['method']=='turn/steer'
emit({'id':'native-question','method':'item/tool/requestUserInput','params':{'threadId':'thread','turnId':'turn','itemId':'item','isBlocking':True,'questions':[{'id':'q','header':'Choice','question':'Proceed?'}]}})
emit({'id':request['id'],'result':{'turnId':'turn'}})
queued=recv()
assert queued['method']=='turn/steer'
assert queued['params']['clientUserMessageId']=='queued-during-question'
emit({'id':queued['id'],'result':{'turnId':'turn'}})
for line in sys.stdin: pass
"#;
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let mut evidence = Evidence {
            thread: Some("thread".into()),
            turn: Some("turn".into()),
            ..Evidence::default()
        };
        let input = process.input();
        let (input_tx, input_receiver) = tokio::sync::mpsc::channel(1);
        let (queued_ack_tx, mut queued_ack_rx) = tokio::sync::oneshot::channel();
        input_tx
            .send(EdgeInvocationInput {
                input: ProviderStageInput::Text {
                    input_id: "queued-during-question".into(),
                    content: "also continue with this".into(),
                    correlation_id: None,
                    expected_turn_id: Some("turn".into()),
                },
                ack: queued_ack_tx,
            })
            .await
            .unwrap();
        drop(input_tx);
        let mut input_rx = Some(input_receiver);
        let (ack_tx, mut ack_rx) = tokio::sync::oneshot::channel();
        let mut submission = Box::pin(submit_stage_input(
            &mut process,
            &input,
            &mut evidence,
            Some(&DelayedGate),
            &token,
            ProviderStageInput::Text {
                input_id: "steer-before-slow-question".into(),
                content: "continue".into(),
                correlation_id: None,
                expected_turn_id: Some("turn".into()),
            },
            OUTPUT_BYTES,
            ack_tx,
            &mut input_rx,
        ));
        let ack = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                ack = &mut ack_rx => ack.expect("provider ACK sender must remain live"),
                result = &mut submission => panic!("submission ended before its early ACK: {result:?}"),
            }
        })
        .await
        .expect("provider ACK must not wait for user interaction");
        assert!(ack.accepted);
        let queued_ack = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                ack = &mut queued_ack_rx => ack.expect("queued guidance ACK sender must remain live"),
                result = &mut submission => panic!("submission ended before queued guidance was classified: {result:?}"),
            }
        })
        .await
        .expect("queued guidance must be classified before the ACK deadline");
        assert!(queued_ack.accepted);
        assert_eq!(queued_ack.input_id, "queued-during-question");
        token.cancel();
        assert!(submission.await.is_err());
        assert!(
            process
                .cancel_and_wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    struct ApprovalReplyGate(Value);

    #[async_trait::async_trait]
    impl ProviderInteractionGate for ApprovalReplyGate {
        async fn request_interaction(
            &self,
            _: &ProviderInteractionRequest,
        ) -> ProviderInteractionDecision {
            ProviderInteractionDecision::Submitted(self.0.clone())
        }
    }

    #[tokio::test]
    async fn real_owner_workspace_approval_cannot_expand_immutable_ceiling() {
        let sandbox = test_profile();
        let prefix = PREFIX;
        for (method, payload, allowed) in [
            (
                "item/commandExecution/requestApproval",
                json!({"decision":"accept"}),
                false,
            ),
            (
                "item/commandExecution/requestApproval",
                json!({"decision":"acceptForSession"}),
                false,
            ),
            (
                "item/fileChange/requestApproval",
                json!({"decision":"accept"}),
                false,
            ),
            (
                "item/permissions/requestApproval",
                json!({"permissions":{"network":{"enabled":true}}}),
                false,
            ),
            (
                "item/permissions/requestApproval",
                json!({"permissions":{"fileSystem":{"write":["/outside"]}},"scope":"session"}),
                false,
            ),
            // Even an apparently in-root additive grant is unsupported: the
            // adapter cannot prove its native execution/symlink scope is bounded.
            (
                "item/permissions/requestApproval",
                json!({"permissions":{"fileSystem":{"write":["/workspace"]}}}),
                false,
            ),
            (
                "item/commandExecution/requestApproval",
                json!({"decision":"decline"}),
                true,
            ),
            (
                "item/permissions/requestApproval",
                json!({"permissions":{},"scope":"turn"}),
                true,
            ),
        ] {
            let script = format!(
                "{prefix}\nemit({{'id':3,'result':{{'turn':{{'id':'turn','status':'inProgress'}}}}}})\nemit({{'id':'approval','method':'{method}','params':{{'threadId':'thread','turnId':'turn','itemId':'item'}}}})\n{}",
                if allowed {
                    "reply=recv()\nassert reply['id']=='approval'\nassert reply['result'].get('decision')=='decline' or reply['result'].get('permissions')=={}\nemit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})\nfor line in sys.stdin: pass\n"
                } else {
                    // wait() closes input after the rejected reply. A forbidden
                    // grant reaching the actual wire makes the process fail.
                    "assert sys.stdin.readline()==''\n"
                }
            );
            let token = CancellationToken::new();
            let mut process = process(&script, token.clone()).await;
            let mut evidence = Evidence::default();
            let result = drive(
                &mut process,
                &stage(),
                "/workspace",
                &sandbox,
                &mut evidence,
                OUTPUT_BYTES,
                Some(&ApprovalReplyGate(payload)),
                &token,
            )
            .await;
            assert_eq!(result.is_ok(), allowed, "{method}");
            assert_eq!(evidence.terminal.as_deref(), allowed.then_some("completed"));
            let outcome = process.wait().await.unwrap();
            assert!(
                outcome.status.unwrap().success(),
                "forbidden reply reached native wire"
            );
            assert!(outcome.settlement.unwrap().ownership.is_authoritative());
        }
    }

    struct CancelGate(CancellationToken);
    #[async_trait::async_trait]
    impl ProviderInteractionGate for CancelGate {
        async fn request_interaction(
            &self,
            _: &ProviderInteractionRequest,
        ) -> ProviderInteractionDecision {
            self.0.cancel();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn real_owner_cancel_during_question_does_not_invent_terminal() {
        let script = format!(
            "{PREFIX}{}",
            r#"
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
emit({'id':'question','method':'item/tool/requestUserInput','params':{'threadId':'thread','turnId':'turn','itemId':'item','isBlocking':True,'questions':[{'id':'q','header':'Choice','question':'Proceed?'}]}})
for line in sys.stdin: pass
"#
        );
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let mut evidence = Evidence::default();
        assert!(
            drive(
                &mut process,
                &stage(),
                "/workspace",
                &test_profile(),
                &mut evidence,
                OUTPUT_BYTES,
                Some(&CancelGate(token.clone())),
                &token
            )
            .await
            .is_err()
        );
        assert_eq!(evidence.turn.as_deref(), Some("turn"));
        assert!(evidence.terminal.is_none());
        assert!(
            process
                .cancel_and_wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }
}
