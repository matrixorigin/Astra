//! Concrete inference-only process mode. Lifecycle, IPC, provider dispatch and
//! journals remain in their existing owners. This never constructs an executor.
use super::*;
use astra_edge::{
    inference_host::{InferenceHost, InferenceOwner},
    local_host::{HostStatus, Installation, ManagedHost},
};
use astra_server_types::edge_ws_protocol::EdgeServerMessage;
use futures_util::SinkExt;

pub(super) async fn run(args: Args) -> Result<(), &'static str> {
    if args.token.is_some()
        || args.capacity_providers.is_some()
        || args.profile.is_some()
        || args.edge_id.is_some()
    {
        return Err("inference-only native mode does not accept tool/provider credentials");
    }
    let store = astra_credentials::native::NativeStore::with_directory(
        args.native_owner_dir
            .ok_or("a private native owner reference is required")?,
    );
    let owner = args
        .expected_inference_owner
        .ok_or("expected native owner is required")?;
    let current = store
        .current_off_runtime()
        .await
        .map_err(|_| "native sign-in is unavailable")?;
    if current.astra_user_id != owner {
        return Err("native owner changed");
    }
    let scope = astra_credentials::LocalModelScope::for_native_session(&current)
        .map_err(|_| "native sign-in must be renewed")?;
    if args
        .server_url
        .as_deref()
        .is_some_and(|url| url != current.environment.astra_url)
    {
        return Err("native owner and Server origin differ");
    }
    let installation = {
        let scope = scope.clone();
        tokio::task::spawn_blocking(move || Installation::open(&scope))
            .await
            .map_err(|_| "local installation is unavailable")?
            .map_err(|_| "local installation is unavailable")?
    };
    let runner_id = installation.runner_id.clone();
    let control = match ManagedHost::bind(installation) {
        Ok(control) => control,
        Err(astra_edge::inference_host::InferenceHostError::AlreadyRunning) => return Ok(()),
        Err(_) => return Err("local host storage or socket is unavailable"),
    };
    let transport = astra_inference_adapter::transport::ProviderTransport::build(
        astra_core::net::runner_provider_client_builder()
            .map_err(|_| "device network policy is invalid")?,
    )
    .map_err(|_| "device network policy is invalid")?;
    let host = InferenceHost::open(
        scope.root().join("inference-journal"),
        InferenceOwner {
            deployment_identity: scope.identity().into(),
            user_id: owner,
            runner_id: astra_turn_types::runner_inference::RunnerInferenceId::new(runner_id)
                .map_err(|_| "invalid runner identity")?,
        },
        scope.models().path().to_owned(),
        scope.root().join("model-secrets"),
        transport,
        Some(store.clone()),
    )
    .await
    .map_err(|_| "device journal is unavailable; existing state was preserved")?;
    control
        .install(host.clone())
        .await
        .map_err(|_| "cannot install the local inference owner")?;
    let url = astra_edge::edge_ws_url(&current.environment.astra_url)
        .map_err(|_| "invalid native Server origin")?;
    let mut delay = Duration::from_secs(1);
    let mut bootstrapping = true;
    let stop = CancellationToken::new();
    let stop_signal = stop.clone();
    tokio::spawn(async move {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = signal.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
            stop_signal.cancel();
        }
    });
    loop {
        let connection = async {
            // The native owner alone performs platform rotation. The tool
            // Runner's token manager never sees or persists these credentials.
            let (connection_store, session) = if bootstrapping {
                (store.clone(), current.clone())
            } else {
                host.native_connection_owner()
                    .await
                    .map_err(|_| "no active native owner")?
            };
            let credential = connection_store
                .credential("astra", Some(&session.generation))
                .await
                .map_err(|_| "native sign-in must be renewed")?;
            if credential.subject != session.subject
                || credential.endpoint != current.environment.astra_url
            {
                return Err("native account or origin changed");
            }
            control.report_status(HostStatus::Connecting);
            let socket = super::connect_edge_websocket(
                &url,
                &credential.access_token,
                host.owner_runner_id(),
            )
            .await
            .map_err(|_| "Server connection is unavailable")?;
            let message = EdgeClientMessage::Auth {
                edge_agent_id: host.owner_runner_id().into(),
                materialization_id: host.journal_id().as_str().into(),
                interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
                hostname: None,
                workspace_dir: None,
                capabilities: None,
            };
            let (socket, user_id, _) = astra_edge::authenticate_connection(
                socket,
                message,
                None,
                &CancellationToken::new(),
            )
            .await
            .map_err(|_| "native authentication failed")?;
            if user_id != current.astra_user_id {
                return Err("Server authenticated a different owner");
            }
            control.report_status(HostStatus::Ready);
            bootstrapping = false;
            serve(socket, host.clone()).await
        };
        tokio::select! {
            _ = control.shutdown_requested() => return Ok(()),
            _ = stop.cancelled() => return Ok(()),
            result = connection => {
                let _ = result;
                control.report_status(HostStatus::ConnectionUnavailable);
            }
        }
        tokio::select! {
            _ = control.shutdown_requested() => return Ok(()),
            _ = stop.cancelled() => return Ok(()),
            _ = tokio::time::sleep(delay) => {},
        }
        delay = (delay * 2).min(Duration::from_secs(10));
    }
}

async fn serve(
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    host: Arc<InferenceHost>,
) -> Result<(), &'static str> {
    let (mut write, mut read) = socket.split();
    let mut worker = astra_edge::inference_connection::InferenceConnectionWorker::spawn(host);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            message = worker.messages.recv() => {
                let message = message.ok_or("inference worker stopped")?;
                write.send(tokio_tungstenite::tungstenite::Message::Text(serde_json::to_string(&message)
                    .map_err(|_| "invalid inference frame")?.into())).await.map_err(|_| "Server connection ended")?;
            },
            frame = read.next() => match frame {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                    let message: EdgeServerMessage = serde_json::from_str(&text).map_err(|_| "invalid Server frame")?;
                    if matches!(message, EdgeServerMessage::Pong {}) { continue; }
                    if !message.is_inference_message() { return Err("non-inference dispatch rejected by device model host"); }
                    worker.commands.try_send(message).map_err(|_| "inference queue is full")?;
                },
                Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(data))) => {
                    write.send(tokio_tungstenite::tungstenite::Message::Pong(data)).await.map_err(|_| "Server connection ended")?;
                },
                Some(Ok(tokio_tungstenite::tungstenite::Message::Pong(_))) => {},
                _ => return Err("Server connection ended"),
            },
            _ = heartbeat.tick() => {
                write.send(tokio_tungstenite::tungstenite::Message::Text(serde_json::to_string(&EdgeClientMessage::Ping {})
                    .map_err(|_| "invalid heartbeat")?.into())).await.map_err(|_| "Server connection ended")?;
            }
        }
    }
}
