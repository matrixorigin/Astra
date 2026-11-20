#![cfg(unix)]

use astra_credentials::{
    LocalModelScope,
    native::{Environment, NativeSession, NativeStore},
    native_device::NativeDeviceAttachment,
};
use astra_edge::local_host::{Installation, ManagedClient};
use astra_server_types::edge_ws_protocol::{EdgeClientMessage, EdgeServerMessage};
use astra_turn_types::runner_inference::{
    RUNNER_INFERENCE_PROTOCOL_VERSION, RunnerInferenceNegotiation,
};
use futures_util::{SinkExt, StreamExt};
use std::{process::Stdio, time::Duration};
use tokio::{net::TcpListener, process::Command};

#[tokio::test]
async fn native_inference_binary_authenticates_without_tools_shares_ipc_and_stops() {
    let root = tempfile::tempdir().unwrap();
    let _override = astra_credentials::set_test_credentials_dir(root.path().join("credentials"));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let native = NativeStore::with_directory(root.path().join("native"));
    let session = native
        .publish(NativeSession {
            environment: Environment {
                issuer: origin.clone(),
                astra_url: origin.clone(),
                moi_url: format!("{origin}/newmoi"),
                authorization_endpoint: format!("{origin}/protocol/openid-connect/auth"),
                token_endpoint: format!("{origin}/protocol/openid-connect/token"),
                revocation_endpoint: format!("{origin}/protocol/openid-connect/revoke"),
                jwks_uri: format!("{origin}/protocol/openid-connect/certs"),
            },
            generation: String::new(),
            subject: "synthetic-uc-user".into(),
            session_id: "synthetic-uc-session".into(),
            astra_user_id: "synthetic-user".into(),
            moi_principal_id: "synthetic-moi-user".into(),
            catalog_user_id: "synthetic-catalog-user".into(),
            access_token: "synthetic-platform-access".into(),
            refresh_token: "synthetic-platform-refresh".into(),
            expires_at: astra_credentials::native::unix_now().unwrap() + 3600,
            workspace_id: None,
            role_id: None,
            refresh_pending: false,
        })
        .unwrap()
        .0;
    native
        .select_workspace(&session, Some("synthetic-workspace"))
        .unwrap();
    let session = native.current().unwrap();
    let scope = LocalModelScope::for_native_session(&session).unwrap();
    let installation = Installation::open(&scope).unwrap();
    let reference = NativeDeviceAttachment::new(&native, &session).unwrap();
    let (hello_tx, hello_rx) = tokio::sync::oneshot::channel();
    let (reject_tx, reject_rx) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        // Tungstenite requires its unboxed HTTP response error in this callback.
        #[allow(clippy::result_large_err)]
        fn check_request(
            request: &tokio_tungstenite::tungstenite::handshake::server::Request,
            response: tokio_tungstenite::tungstenite::handshake::server::Response,
        ) -> Result<
            tokio_tungstenite::tungstenite::handshake::server::Response,
            tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
        > {
            assert_eq!(request.uri().path(), "/edge/ws");
            assert_eq!(
                request.headers().get("authorization").unwrap(),
                "Bearer synthetic-platform-access"
            );
            Ok(response)
        }
        let mut socket = tokio_tungstenite::accept_hdr_async(tcp, check_request)
            .await
            .unwrap();
        let frame = socket.next().await.unwrap().unwrap();
        let message: EdgeClientMessage = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        assert!(matches!(
            message,
            EdgeClientMessage::Auth {
                capabilities: None,
                workspace_dir: None,
                ..
            }
        ));
        let auth = EdgeServerMessage::AuthOk {
            user_id: "synthetic-user".into(),
            edge_id: "synthetic-edge".into(),
            interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
        };
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&auth).unwrap().into(),
            ))
            .await
            .unwrap();
        loop {
            let frame = socket.next().await.unwrap().unwrap();
            if !frame.is_text() {
                continue;
            }
            let message: EdgeClientMessage =
                serde_json::from_str(frame.to_text().unwrap()).unwrap();
            if let EdgeClientMessage::InferenceHello { .. } = message {
                break;
            }
        }
        let ack = EdgeServerMessage::InferenceHelloAck {
            negotiation: RunnerInferenceNegotiation::accepted(
                RUNNER_INFERENCE_PROTOCOL_VERSION,
                1,
                1_000_000,
            ),
        };
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&ack).unwrap().into(),
            ))
            .await
            .unwrap();
        hello_tx.send(()).unwrap();
        reject_rx.await.unwrap();
        let tool = EdgeServerMessage::ToolCancel {
            request_id: "not-an-inference-request".into(),
            delivery_generation: 1,
        };
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&tool).unwrap().into(),
            ))
            .await
            .unwrap();
        // The inference-only process closes on a tool frame rather than
        // constructing a tool executor or acknowledging a tool result.
        loop {
            match socket.next().await {
                None
                | Some(Err(_))
                | Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => break,
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                    let message: EdgeClientMessage = serde_json::from_str(&text).unwrap();
                    assert!(matches!(message, EdgeClientMessage::Ping { .. }));
                }
                _ => {}
            }
        }
    });
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_astra-edge"));
        command
            .env_clear()
            .env("HOME", root.path())
            .env("ASTRA_CLI_CREDENTIALS_DIR", root.path().join("credentials"))
            .args(["--inference-only", "--native-owner-dir"])
            .arg(native.directory())
            .args([
                "--expected-inference-owner",
                "synthetic-user",
                "--server-url",
                &origin,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command
    };
    let mut child = command().spawn().unwrap();
    tokio::time::timeout(Duration::from_secs(15), hello_rx)
        .await
        .unwrap()
        .unwrap();
    let client = tokio::time::timeout(
        Duration::from_secs(5),
        ManagedClient::connect_native(&installation, scope.clone(), reference.clone()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(client.is_alive());
    let other = tokio::time::timeout(Duration::from_secs(5), command().status())
        .await
        .unwrap()
        .unwrap();
    assert!(
        other.success(),
        "second launch must join the same installation"
    );
    let second = tokio::time::timeout(
        Duration::from_secs(5),
        ManagedClient::connect_native(&installation, scope, reference),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(second.is_alive());
    drop(client);
    assert!(
        second.is_alive(),
        "one terminal detaching cannot kill another lease"
    );
    reject_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();
    let pid = child.id().unwrap();
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGTERM) }, 0);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
