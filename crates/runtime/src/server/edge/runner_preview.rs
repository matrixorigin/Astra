//! Subscribe on the current socket owner before making the grant discoverable.
//! The stream transports provisional batches only. It cannot dispatch or ACK.
use astra_core::{ClassifiedError, ErrorKind, SharedPool};
use astra_server_types::edge_connection_pool::EdgeConnectionPool;
use astra_turn_types::runner_inference::{
    RUNNER_INFERENCE_PROGRESS_BATCH_BYTES, RunnerInferenceAttemptIdentity,
    RunnerInferenceDispatchGrant, RunnerInferenceProgressBatch,
};
use axum::{
    Json,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{Stream, StreamExt, TryStreamExt};
use hmac::{Hmac, Mac};
use std::{pin::Pin, sync::OnceLock};
use tokio::io::AsyncReadExt;

pub(crate) type StreamHandle = Pin<Box<dyn Stream<Item = RunnerInferenceProgressBatch> + Send>>;
const PATH: &str = "/service/edges/inference/preview";
const PROOF_HEADER: &str = "x-astra-preview-proof";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Subscription {
    connection_id: String,
    attempt: RunnerInferenceAttemptIdentity,
    expires_at: i64,
}

impl Subscription {
    // Never forward the reusable service credential to an advertised origin.
    fn proof(&self, key: &[u8]) -> Option<Hmac<sha2::Sha256>> {
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key).ok()?;
        mac.update(PATH.as_bytes());
        mac.update(&serde_json::to_vec(self).ok()?);
        Some(mac)
    }
}

fn local_stream(pool: &EdgeConnectionPool, sub: Subscription) -> Result<StreamHandle, StatusCode> {
    if !pool.owns_inference_socket(
        &sub.attempt.user_id,
        sub.attempt.binding.runner_id.as_str(),
        &sub.connection_id,
    ) {
        return Err(StatusCode::CONFLICT);
    }
    let mut rx = pool
        .subscribe_runner_inference_progress(&sub.attempt)
        .ok_or(StatusCode::TOO_MANY_REQUESTS)?;
    Ok(Box::pin(async_stream::stream! {
        while let Ok(batch) = rx.recv().await {
            if batch.attempt == sub.attempt { yield batch; }
        }
    }))
}

fn key() -> Option<String> {
    std::env::var(super::edge_service_status_handler::SERVICE_KEY_ENV)
        .ok()
        .filter(|s| !s.is_empty())
}

pub(crate) async fn serve(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(sub): Json<Subscription>,
) -> Result<Body, StatusCode> {
    let key = key().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    serve_on_owner(&state.edge_connection_pool, &key, headers, sub)
}

fn serve_on_owner(
    pool: &EdgeConnectionPool,
    key: &str,
    headers: HeaderMap,
    sub: Subscription,
) -> Result<Body, StatusCode> {
    let now = chrono::Utc::now().timestamp();
    if !(now..=now + 30).contains(&sub.expires_at) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let proof = headers
        .get(PROOF_HEADER)
        .and_then(|v| URL_SAFE_NO_PAD.decode(v.as_bytes()).ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    sub.proof(key.as_bytes())
        .ok_or(StatusCode::UNAUTHORIZED)?
        .verify_slice(&proof)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    let stream = local_stream(pool, sub)?.map(|batch| {
        let data = serde_json::to_vec(&batch).map_err(std::io::Error::other)?;
        let mut frame = Vec::with_capacity(4 + data.len());
        frame.extend_from_slice(&(data.len() as u32).to_be_bytes());
        frame.extend(data);
        Ok::<_, std::io::Error>(frame)
    });
    Ok(Body::from_stream(stream))
}

fn unavailable() -> ClassifiedError {
    ClassifiedError::new(
        ErrorKind::StreamTransport,
        "Live device output is unavailable. Reconnect the inference host and verify the Server relay configuration.",
    )
}

fn busy() -> ClassifiedError {
    ClassifiedError::new(
        ErrorKind::ResourceLimit,
        "Device live-output capacity is full. Retry after another device request completes.",
    )
}

pub(crate) async fn subscribe(
    db: &SharedPool,
    pool: &EdgeConnectionPool,
    grant: &RunnerInferenceDispatchGrant,
) -> Result<StreamHandle, ClassifiedError> {
    let attempt = &grant.attempt;
    let (connection_id, origin) = astra_services::runner_model_bindings::runner_preview_socket(
        db,
        attempt,
        &grant.process_boot_nonce,
    )
    .await
    .map_err(|_| unavailable())?
    .ok_or_else(unavailable)?;
    let sub = Subscription {
        connection_id,
        attempt: attempt.clone(),
        expires_at: chrono::Utc::now().timestamp() + 15,
    };
    if pool.owns_inference_socket(
        &attempt.user_id,
        attempt.binding.runner_id.as_str(),
        &sub.connection_id,
    ) {
        return local_stream(pool, sub).map_err(|s| {
            if s == StatusCode::TOO_MANY_REQUESTS {
                busy()
            } else {
                unavailable()
            }
        });
    }
    let origin = super::edge_capacity_handler::validate_relay_origin(
        origin.as_deref().ok_or_else(unavailable)?,
    )
    .map_err(|_| unavailable())?;
    remote_stream(origin, key().as_deref().ok_or_else(unavailable)?, sub).await
}

async fn remote_stream(
    origin: reqwest::Url,
    key: &str,
    sub: Subscription,
) -> Result<StreamHandle, ClassifiedError> {
    static CLIENT: OnceLock<Result<reqwest::Client, reqwest::Error>> = OnceLock::new();
    let client = CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .build()
        })
        .as_ref()
        .map_err(|_| unavailable())?;
    let signature = sub
        .proof(key.as_bytes())
        .ok_or_else(unavailable)?
        .finalize()
        .into_bytes();
    let response = client
        .post(origin.join(PATH).map_err(|_| unavailable())?)
        .json(&sub)
        .header(PROOF_HEADER, URL_SAFE_NO_PAD.encode(signature))
        .send()
        .await
        .map_err(|_| unavailable())?;
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        return Err(busy());
    }
    if response.status() != StatusCode::OK {
        return Err(unavailable());
    }
    let mut reader =
        tokio_util::io::StreamReader::new(response.bytes_stream().map_err(std::io::Error::other));
    Ok(Box::pin(async_stream::stream! {
        while let Ok(size) = reader.read_u32().await {
            if size == 0 || size as usize > RUNNER_INFERENCE_PROGRESS_BATCH_BYTES { break; }
            let mut data = vec![0; size as usize];
            if reader.read_exact(&mut data).await.is_err() { break; }
            let Ok(batch) = serde_json::from_slice::<RunnerInferenceProgressBatch>(&data) else { break };
            if batch.attempt != sub.attempt || batch.validate_wire_bounds().is_err() { break; }
            yield batch;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::runner_inference::{
        RunnerInferenceProgressEvent, RunnerInferenceProviderEvent,
    };
    use axum::{Router, routing::post};

    #[tokio::test]
    async fn remote_runner_preview_delivers_before_custody_and_rejects_busy_or_forged_subscription()
    {
        let run_pool = EdgeConnectionPool::new();
        let socket_pool = EdgeConnectionPool::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let gen_id = socket_pool.register("user", "runner", None, None, tx);
        socket_pool.mark_inference_enrolled("user", "runner", gen_id);
        let connection_id = socket_pool.local_inference_connection_ids().pop().unwrap();
        let attempt: RunnerInferenceAttemptIdentity = serde_json::from_value(serde_json::json!({
            "user_id":"user","scope":{"kind":"run","session_id":"s","run_id":"r","turn":1,"round":1,"operation_id":"op","logical_attempt":0},
            "invocation_id":"inv","attempt_id":"att",
            "binding":{"runner_id":"runner","journal_id":"j","binding_id":"b","binding_revision":1,"profile_revision":1},
            "request":{"artifact_id":"art","sha256":"a".repeat(64),"byte_len":10}
        })).unwrap();
        let sub = || Subscription {
            connection_id: connection_id.clone(),
            attempt: attempt.clone(),
            expires_at: chrono::Utc::now().timestamp() + 15,
        };
        let socket = socket_pool.clone();
        let key = "ephemeral-fixture-key";
        let router = Router::new().route(
            PATH,
            post(move |headers: HeaderMap, Json(sub): Json<Subscription>| {
                let pool = socket.clone();
                async move { serve_on_owner(&pool, key, headers, sub) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin: reqwest::Url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        assert!(!run_pool.owns_inference_socket("user", "runner", &connection_id));
        assert!(remote_stream(origin.clone(), "wrong", sub()).await.is_err());
        let mut batches = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            remote_stream(origin.clone(), key, sub()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            matches!(remote_stream(origin,key,sub()).await, Err(e) if e.kind == ErrorKind::ResourceLimit)
        );
        let event = RunnerInferenceProgressEvent {
            sequence: 0,
            event: RunnerInferenceProviderEvent::Json(
                serde_json::json!({"type":"response.output_text.delta","delta":"live"}),
            ),
        };
        let batch = RunnerInferenceProgressBatch::new(attempt.clone(), vec![event]).unwrap();
        socket_pool
            .publish_runner_inference_progress(batch.clone())
            .unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), batches.next())
                .await
                .unwrap(),
            Some(batch)
        );
        server.abort();
    }
}
