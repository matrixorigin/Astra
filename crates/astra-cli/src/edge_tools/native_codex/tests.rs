use super::*;
use astra_tools::tool_engine::{ToolInvocationAdmissionSource, ToolInvocationMetadata};

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
emit({'id':6,'result':{}})
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
assert request['id']==8 and request['method']=='config/read'
emit({'id':8,'result':{'config':{'additional':{'mcp_servers':{}}},'origins':{}}})
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
emit({'id':6,'result':{'turnId':'turn'}})
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
assert request['id']==8 and request['method']=='config/read'
emit({'id':8,'result':{'config':{'additional':{'mcp_servers':{}}},'origins':{}}})
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
fn native_stage_budget_requires_immutable_work_deadline_not_command_cap() {
    let metadata = ToolInvocationMetadata {
        command_timeout_cap_ms: Some(86_400_000),
        ..Default::default()
    };
    assert!(native_stage_remaining(metadata).is_err());
    let deadline = std::time::Instant::now() + Duration::from_secs(43_200);
    for command_timeout_cap_ms in [None, Some(1), Some(86_400_000)] {
        let remaining = native_stage_remaining(ToolInvocationMetadata {
            admission_deadline: Some(deadline),
            command_timeout_cap_ms,
            ..Default::default()
        })
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
        provider_stage_input_id: None,
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
                question: "Choose second?".into(),
                answers: vec![" D ".into()],
                multi_select: false,
                annotation: None,
            },
            astra_tools::AskUserQuestionAnswer {
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
    assert!(question_prompt(&request).unwrap_err().contains("duplicate"));
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
    let resume = thread_request(
        &stage,
        "/workspace",
        &profile,
        stage.model.as_deref(),
        &json!({}),
    );
    assert_eq!(resume["method"], "thread/resume");
    assert_eq!(resume["params"]["threadId"], "native-thread");
    assert_eq!(resume["params"]["excludeTurns"], true);
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
    assert!(
        evidence
            .notification(
                "thread/tokenUsage/updated",
                &usage_event("turn", 149, 60, 30, 20),
                OUTPUT_BYTES
            )
            .is_err()
    );
    let mut evidence = active();
    assert!(
        evidence
            .notification(
                "thread/tokenUsage/updated",
                &usage_event("turn", 30, 20, 11, 10),
                OUTPUT_BYTES
            )
            .is_err()
    );
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
            &json!({"threadId": "thread", "turnId": "turn", "delta": "你好completed"}),
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
            &json!({"threadId": "thread", "turnId": "turn", "delta": "x".repeat(OUTPUT_BYTES + 1)}),
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
    assert_eq!(evidence.final_output.as_deref(), Some("final answer"));
    assert!(evidence.output_capped);
}

#[test]
fn original_native_questions_and_rpc_id_type_are_preserved() {
    let envelope = json!({"id": "native-question", "method": "item/tool/requestUserInput", "params": {"threadId": "thread", "turnId": "turn", "itemId": "item", "questions": [{"id": "q", "question": "Which?"}], "isBlocking": true}});
    let request = interaction_request(&envelope, &active(), None).unwrap();
    assert_eq!(request.payload["native_request_id"], envelope["id"]);
    assert_eq!(request.payload["params"], envelope["params"]);
    assert_eq!(request.request_id, "\"native-question\"");
    let mut wrong = envelope;
    wrong["params"]["turnId"] = json!("other");
    assert!(interaction_request(&wrong, &active(), None).is_err());
}

#[tokio::test]
async fn selected_cli_entrypoint_requires_binding_policy_and_admitted_budget() {
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
    // An explicit local sandbox bypass still cannot supply execution time.
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
    assert!(denied.output.contains("requires an admitted stage budget"));
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
            &fixture_execution_ceiling(&executor),
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
                max_queued_frames: 4,
                max_stderr_bytes: 4096,
                timeout: Duration::from_secs(40),
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
                }}), &mut evidence, OUTPUT_BYTES, None, Some(&cancel), None, None).await?;
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
    assert!(result["text"].as_str().unwrap().contains(&marker));
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
                    max_queued_frames: 4,
                    max_stderr_bytes: 128,
                    timeout: Duration::from_secs(5),
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
assert request['id']==8 and request['method']=='config/read'
emit({'id':8,'result':{'config':{'additional':{'mcp_servers':{}}},'origins':{}}})
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
    async fn native_stage_disables_all_effective_mcp_servers_before_thread_start() {
        let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
assert recv()['method']=='initialize'
emit({'id':1,'result':{'userAgent':'codex-cli test','codexHome':'/home/user/.codex','platformFamily':'unix','platformOs':'linux'}})
assert recv()['method']=='initialized'
request=recv()
assert request['id']==7 and request['method']=='account/read'
emit({'id':7,'result':{'account':None,'requiresOpenaiAuth':False}})
request=recv()
assert request['id']==8 and request['method']=='config/read'
emit({'id':8,'result':{'config':{'additional':{'mcp_servers':{
    'personal-tools': {'command':'do-not-copy'},
    'workspace-tools': {'url':'https://example.invalid/mcp'}
}}},'origins':{}}})
request=recv()
assert request['method']=='thread/start'
assert request['params']['config']['mcp_servers']=={
    'personal-tools': {'enabled':False},
    'workspace-tools': {'enabled':False}
}
assert 'command' not in request['params']['config']['mcp_servers']['personal-tools']
emit({'id':2,'result':{'thread':{'id':'thread','status':{'type':'idle'}},'cwd':'/workspace','approvalPolicy':'never','approvalsReviewer':'user','activePermissionProfile':{'id':request['params']['permissions']},'sandbox':{'type':'readOnly','networkAccess':False}}})
request=recv()
assert request['method']=='turn/start'
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
for line in sys.stdin: pass
"#;
        let token = CancellationToken::new();
        let mut process = process(script, token.clone()).await;
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
    async fn native_stage_fails_closed_when_effective_mcp_config_cannot_be_read() {
        let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
assert recv()['method']=='initialize'
emit({'id':1,'result':{'userAgent':'codex-cli test','codexHome':'/home/user/.codex','platformFamily':'unix','platformOs':'linux'}})
assert recv()['method']=='initialized'
request=recv()
assert request['id']==7 and request['method']=='account/read'
emit({'id':7,'result':{'account':None,'requiresOpenaiAuth':False}})
request=recv()
assert request['id']==8 and request['method']=='config/read'
emit({'id':8,'error':{'code':'config-unavailable','message':'config unavailable'}})
for line in sys.stdin:
    request=json.loads(line)
    assert request['method'] not in ('thread/start','thread/resume','turn/start')
"#;
        let token = CancellationToken::new();
        let mut process = process(script, token.clone()).await;
        let mut evidence = Evidence::default();
        let error = drive(
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
        .unwrap_err();
        assert_eq!(error, "native config read request rejected");
        assert!(evidence.thread.is_none());
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
assert request['id']==8 and request['method']=='config/read'
emit({'id':8,'result':{'config':{'additional':{'mcp_servers':{}}},'origins':{}}})
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
assert request['id']==8 and request['method']=='config/read'
emit({'id':8,'result':{'config':{'additional':{'mcp_servers':{}}},'origins':{}}})
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
    async fn ambiguous_model_name_stops_before_provider_start_and_returns_user_choices() {
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
assert request['id']==8 and request['method']=='config/read'
emit({'id':8,'result':{'config':{'additional':{'mcp_servers':{}}},'origins':{}}})
request=recv()
assert request['id']==4 and request['method']=='model/list'
assert request['params']['includeHidden'] is True
emit({'id':4,'result':{'data':[
    {'id':'luna-56','model':'gpt-5.6-luna','displayName':'Luna'},
    {'id':'luna-6','model':'gpt-6-luna','displayName':'Luna'}
], 'nextCursor':None}})
for line in sys.stdin:
    request=json.loads(line)
    assert request['method'] not in ('thread/start','thread/resume','turn/start')
"#;
        let token = CancellationToken::new();
        let mut process = process(script, token.clone()).await;
        let mut stage = stage();
        stage.model = Some("luna".into());
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
        assert!(error.contains("Luna (gpt-5.6-luna)"));
        assert!(error.contains("Luna (gpt-6-luna)"));
        assert_eq!(
            evidence.model_selection,
            Some(json!({
                "status": "requires_user_choice",
                "requested": "luna",
                "choices": ["Luna (gpt-5.6-luna)", "Luna (gpt-6-luna)"]
            }))
        );
        assert!(evidence.thread.is_none());
        let outcome = process.cancel_and_wait().await.unwrap();
        assert!(outcome.settlement.unwrap().ownership.is_authoritative());
    }

    #[tokio::test]
    async fn unavailable_model_stops_before_thread_start_with_an_actionable_error() {
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
assert request['id']==8 and request['method']=='config/read'
emit({'id':8,'result':{'config':{'additional':{'mcp_servers':{}}},'origins':{}}})
request=recv()
assert request['id']==4 and request['method']=='model/list'
emit({'id':4,'result':{'data':[], 'nextCursor':None}})
for line in sys.stdin:
    request=json.loads(line)
    assert request['method'] not in ('thread/start','thread/resume','turn/start')
"#;
        let token = CancellationToken::new();
        let mut process = process(script, token.clone()).await;
        let mut stage = stage();
        stage.model = Some("model-that-is-not-currently-listed".into());
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
        assert!(error.contains("not available from this provider"));
        assert!(evidence.thread.is_none());
        let outcome = process.cancel_and_wait().await.unwrap();
        assert!(outcome.settlement.unwrap().ownership.is_authoritative());
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
assert request['id']==8 and request['method']=='config/read'
emit({'id':8,'result':{'config':{'additional':{'mcp_servers':{}}},'origins':{}}})
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
emit({'method':'item/agentMessage/delta','params':{'threadId':'thread','turnId':'turn','delta':'review complete'}})
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

    struct AnswerGate;
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
emit({'id':6,'result':{'turnId':'turn'}})
answer=recv()
assert answer=={'id':'native-question','result':{'answers':{'q':{'answers':['Proceed']}}}}
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

    struct RecordingInputFenceGate {
        seen: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl ProviderInteractionGate for RecordingInputFenceGate {
        async fn request_interaction(
            &self,
            request: &ProviderInteractionRequest,
        ) -> ProviderInteractionDecision {
            *self.seen.lock().expect("input fence lock") = request.provider_stage_input_id.clone();
            ProviderInteractionDecision::Cancelled
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn question_after_provider_ack_keeps_the_provisional_input_fence() {
        let script = format!(
            "{PREFIX}{}",
            r#"
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
request=recv()
assert request['method']=='turn/steer'
emit({'id':6,'result':{'turnId':'turn'}})
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
        let gate = RecordingInputFenceGate { seen: seen.clone() };
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
            seen.lock().expect("input fence lock").as_deref(),
            Some("accepted-before-question")
        );
        assert_eq!(
            evidence.last_accepted_stage_input_id.as_deref(),
            Some("accepted-before-question")
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
    async fn question_first_classifies_later_guidance_before_the_ack_deadline() {
        let script = format!(
            "{PREFIX}{}",
            r#"
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
emit({'id':'native-question','method':'item/tool/requestUserInput','params':{'threadId':'thread','turnId':'turn','itemId':'item','isBlocking':True,'questions':[{'id':'q','header':'Choice','question':'Proceed?'}]}})
answer=recv()
assert answer=={'id':'native-question','result':{'answers':{'q':{'answers':['Proceed']}}}}
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
        drop(input_tx);
        let ack = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                ack = &mut ack_rx => ack.expect("guidance ACK sender must remain live"),
                result = &mut driving => panic!("drive ended before guidance was classified: {result:?}"),
            }
        })
        .await
        .expect("question-first guidance must be classified before the ACK deadline");
        assert!(!ack.accepted);
        release.notify_one();
        driving.await.unwrap();
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
    async fn steer_interaction_failure_stops_the_native_stage_instead_of_being_swallowed() {
        let script = format!(
            "{PREFIX}{}",
            r#"
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
request=recv()
assert request['method']=='turn/steer'
emit({'id':6,'error':{'code':'rejected','message':'not accepted'}})
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
        assert_eq!(result.unwrap_err(), "native request rejected");
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
    async fn real_owner_steer_ack_is_sent_before_a_slow_interaction_finishes() {
        let script = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
request=recv()
assert request['method']=='turn/steer'
emit({'id':'native-question','method':'item/tool/requestUserInput','params':{'threadId':'thread','turnId':'turn','itemId':'item','isBlocking':True,'questions':[{'id':'q','header':'Choice','question':'Proceed?'}]}})
emit({'id':6,'result':{'turnId':'turn'}})
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
        assert!(!queued_ack.accepted);
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
