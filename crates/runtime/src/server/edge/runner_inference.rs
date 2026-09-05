//! Bounded inference control/transfer worker for one authenticated connection.
//! All execution state stays in the services ledger. The only local buffers are
//! disposable transport assemblies; losing them never acknowledges custody.

use std::{collections::HashMap, time::Duration};

use astra_core::SharedPool;
use astra_server_types::{
    edge_connection_pool::EdgeConnectionPool,
    edge_ws_protocol::{EdgeClientMessage, EdgeServerMessage},
};
use astra_services::{
    inference_execution::runner::*,
    runner_model_bindings::*,
    service_error::{ServiceError, ServiceErrorKind},
};
use astra_turn_types::runner_inference::*;
use sha2::{Digest, Sha256};
use tokio::sync::{Semaphore, SemaphorePermit, mpsc};

pub(super) const CONTROL_CAPACITY: usize = 32;
const WINDOW_BYTES: usize = 8 * RUNNER_INFERENCE_CHUNK_BYTES;
const MAX_CONNECTION_TRANSFERS: usize = 8;
// Global request/response body reservation; queued chunks add at most the
// bounded transport channel window. No unbounded per-connection body inventory.
static TRANSFER_KIB: Semaphore = Semaphore::const_new(64 * 1024);

struct ResponseAssembly {
    header: RunnerInferenceTerminalTransfer,
    bytes: Vec<u8>,
    credited_through: usize,
    updated: tokio::time::Instant,
    _reservation: SemaphorePermit<'static>,
}

struct RequestTransfer {
    bytes: Vec<u8>,
    offset: usize,
    updated: tokio::time::Instant,
    _reservation: SemaphorePermit<'static>,
}

#[derive(Default)]
struct TransportBuffers {
    responses: HashMap<RunnerInferenceId, ResponseAssembly>,
    requests: HashMap<RunnerInferenceId, RequestTransfer>,
}

fn request_credit_end(
    sent: usize,
    next_offset: u32,
    credit_bytes: u32,
    total: usize,
) -> Result<usize, RunnerInferenceRejection> {
    if next_offset as usize > sent || credit_bytes == 0 || credit_bytes as usize > WINDOW_BYTES {
        return Err(RunnerInferenceRejection::InvalidEvidence);
    }
    Ok((next_offset as usize + credit_bytes as usize).min(total))
}

fn next_request_chunk(
    text: &str,
    offset: usize,
    through: usize,
) -> Option<(usize, RunnerInferenceChunkData)> {
    let mut end = (offset + RUNNER_INFERENCE_CHUNK_BYTES).min(through);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == offset {
        return None;
    }
    RunnerInferenceChunkData::new(text[offset..end].to_owned())
        .ok()
        .map(|data| (end, data))
}

impl ResponseAssembly {
    fn new(header: RunnerInferenceTerminalTransfer) -> Result<Self, RunnerInferenceRejection> {
        header
            .terminal
            .validate_wire_bounds()
            .map_err(|_| RunnerInferenceRejection::InvalidEvidence)?;
        let size = header.response_bytes.get() as usize;
        if size > RUNNER_INFERENCE_ARTIFACT_BYTES {
            return Err(RunnerInferenceRejection::CapacityUnavailable);
        }
        let reservation = TRANSFER_KIB
            .try_acquire_many(size.div_ceil(1024) as u32)
            .map_err(|_| RunnerInferenceRejection::CapacityUnavailable)?;
        Ok(Self {
            header,
            bytes: Vec::with_capacity(size),
            credited_through: size.min(WINDOW_BYTES),
            updated: tokio::time::Instant::now(),
            _reservation: reservation,
        })
    }

    fn append(
        &mut self,
        chunk: &RunnerInferencePayloadChunk,
    ) -> Result<bool, RunnerInferenceRejection> {
        let offset = chunk.offset as usize;
        let data = chunk.data.as_bytes();
        let end = offset
            .checked_add(data.len())
            .ok_or(RunnerInferenceRejection::InvalidEvidence)?;
        if offset < self.bytes.len() {
            if self.bytes.get(offset..end) != Some(data) {
                return Err(RunnerInferenceRejection::InvalidEvidence);
            }
            return Ok(false);
        }
        if offset != self.bytes.len()
            || end > self.credited_through
            || end > self.header.response_bytes.get() as usize
        {
            return Err(RunnerInferenceRejection::InvalidEvidence);
        }
        self.bytes.extend_from_slice(data);
        self.updated = tokio::time::Instant::now();
        Ok(self.bytes.len() == self.header.response_bytes.get() as usize)
    }

    fn credit(&mut self, generation: u64) -> EdgeServerMessage {
        self.credited_through =
            (self.bytes.len() + WINDOW_BYTES).min(self.header.response_bytes.get() as usize);
        EdgeServerMessage::InferenceResponseCredit {
            attempt_id: self.header.attempt.attempt_id.clone(),
            delivery_generation: generation,
            next_offset: self.bytes.len() as u32,
            credit_bytes: (self.credited_through - self.bytes.len()) as u32,
        }
    }
}

fn rejection(error: &ServiceError) -> RunnerInferenceRejection {
    match error.kind {
        ServiceErrorKind::Persistence | ServiceErrorKind::Network | ServiceErrorKind::Internal => {
            RunnerInferenceRejection::StorageUnavailable
        }
        ServiceErrorKind::Conflict | ServiceErrorKind::ConflictTransient => {
            RunnerInferenceRejection::PublicationConflict
        }
        _ => RunnerInferenceRejection::InvalidEvidence,
    }
}

async fn send(
    pool: &EdgeConnectionPool,
    connection: &AuthenticatedRunnerConnection,
    generation: u64,
    message: EdgeServerMessage,
) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(3),
            pool.send_runner_inference_message(
                &connection.user_id,
                connection.runner_id.as_str(),
                generation,
                message
            )
        )
        .await,
        Ok(Ok(()))
    )
}

/// Cheap ingress check before queueing, including current-socket generation.
/// It intentionally grants no durable authority; the worker/service checks that.
pub(super) fn validate_ingress(
    message: &EdgeClientMessage,
    user: &str,
    runner: &str,
    generation: u64,
) -> bool {
    let matches_identity = |identity: &RunnerInferenceAttemptIdentity| {
        identity.user_id == user
            && identity.binding.runner_id.as_str() == runner
            && identity.scope.session_id().is_some_and(|id| id.len() <= 64)
            && identity.scope.run_id().is_none_or(|id| id.len() <= 64)
            && identity.scope.operation_id().len() <= 64
    };
    match message {
        EdgeClientMessage::InferenceHello { .. } => true,
        EdgeClientMessage::InferenceBindingPublish { publication } => {
            publication.change.identity().runner_id.as_str() == runner
        }
        EdgeClientMessage::InferenceStartEvidence {
            grant,
            delivery_generation,
            ..
        } => *delivery_generation == generation && matches_identity(&grant.attempt),
        EdgeClientMessage::InferenceTerminal {
            transfer,
            delivery_generation,
        } => {
            *delivery_generation == generation
                && matches_identity(&transfer.attempt)
                && transfer.terminal.validate_wire_bounds().is_ok()
                && transfer.response_bytes.get() as usize <= RUNNER_INFERENCE_ARTIFACT_BYTES
        }
        EdgeClientMessage::InferenceResponseChunk {
            delivery_generation,
            ..
        }
        | EdgeClientMessage::InferenceRequestCredit {
            delivery_generation,
            ..
        } => *delivery_generation == generation,
        _ => false,
    }
}

pub(super) async fn run(
    db: Option<SharedPool>,
    pool: EdgeConnectionPool,
    connection: AuthenticatedRunnerConnection,
    generation: u64,
    mut receiver: mpsc::Receiver<EdgeClientMessage>,
) {
    let Some(wakeup) = pool.runner_inference_wakeup(
        &connection.user_id,
        connection.runner_id.as_str(),
        generation,
    ) else {
        return;
    };
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut negotiated = false;
    let mut buffers = TransportBuffers::default();
    loop {
        if !pool.is_current_inference_connection(
            &connection.user_id,
            connection.runner_id.as_str(),
            generation,
        ) {
            break;
        }
        tokio::select! {
            biased;
            message = receiver.recv() => {
                let Some(message) = message else { break; };
                if !validate_ingress(&message, &connection.user_id, connection.runner_id.as_str(), generation) { break; }
                let Some(db) = db.as_ref() else {
                    if !send(&pool, &connection, generation, EdgeServerMessage::InferenceHelloAck {
                        negotiation: RunnerInferenceNegotiation::Unavailable { reason: RunnerInferenceRejection::InferenceUnsupported }
                    }).await { break; }
                    continue;
                };
                let wake_delivery = matches!(message, EdgeClientMessage::InferenceHello { .. } | EdgeClientMessage::InferenceBindingPublish { .. });
                let response = handle_message(db, &pool, &connection, generation, &mut negotiated, &mut buffers, message).await;
                if let Some(response) = response {
                    if !send(&pool, &connection, generation, response).await { break; }
                }
                if negotiated && wake_delivery { wakeup.notify_one(); }
            }
            _ = wakeup.notified(), if negotiated => {
                if let Some(db) = db.as_ref() { schedule_deliveries(db, &pool, &connection, generation, &mut buffers.requests).await; }
            }
            _ = interval.tick() => {
                let expired_responses = buffers.responses.iter()
                    .filter(|(_, assembly)| assembly.updated.elapsed() >= Duration::from_secs(30))
                    .map(|(attempt_id, _)| attempt_id.clone())
                    .collect::<Vec<_>>();
                for attempt_id in expired_responses {
                    buffers.responses.remove(&attempt_id);
                    if !send(&pool, &connection, generation, EdgeServerMessage::InferenceRejected {
                        attempt_id: Some(attempt_id),
                        reason: RunnerInferenceRejection::CapacityUnavailable,
                    }).await {
                        return;
                    }
                }
                buffers.requests.retain(|_, transfer| transfer.updated.elapsed() < Duration::from_secs(30));
                if negotiated {
                    if let Some(db) = db.as_ref() { schedule_deliveries(db, &pool, &connection, generation, &mut buffers.requests).await; }
                }
            }
        }
    }
    // Delivery cancellation loses only a transport claim, never the grant or
    // Runner result authority. Durable reconciliation will replay exact facts.
}

async fn handle_message(
    db: &SharedPool,
    pool: &EdgeConnectionPool,
    connection: &AuthenticatedRunnerConnection,
    generation: u64,
    negotiated: &mut bool,
    buffers: &mut TransportBuffers,
    message: EdgeClientMessage,
) -> Option<EdgeServerMessage> {
    if !*negotiated
        && !matches!(
            message,
            EdgeClientMessage::InferenceHello { .. }
                | EdgeClientMessage::InferenceBindingPublish { .. }
        )
    {
        return Some(EdgeServerMessage::InferenceRejected {
            attempt_id: None,
            reason: RunnerInferenceRejection::InferenceUnsupported,
        });
    }
    let TransportBuffers {
        responses: transfers,
        requests,
    } = buffers;
    match message {
        EdgeClientMessage::InferenceHello {
            protocol_version,
            journal_id,
            process_boot_nonce,
        } => {
            let negotiation = match enroll_runner_inference(
                db,
                connection,
                protocol_version,
                &journal_id,
                &process_boot_nonce,
            )
            .await
            {
                Ok(()) => {
                    *negotiated = true;
                    RunnerInferenceNegotiation::accepted(
                        protocol_version,
                        generation,
                        chrono::Utc::now().timestamp_millis().max(0) as u64,
                    )
                }
                Err(error) => RunnerInferenceNegotiation::Unavailable {
                    reason: if protocol_version != RUNNER_INFERENCE_PROTOCOL_VERSION {
                        RunnerInferenceRejection::ProtocolVersionUnsupported
                    } else {
                        rejection(&error)
                    },
                },
            };
            Some(EdgeServerMessage::InferenceHelloAck { negotiation })
        }
        EdgeClientMessage::InferenceBindingPublish { publication } => {
            let checked = pool.validate_runner_inference_publication(
                &connection.user_id,
                connection.runner_id.as_str(),
                generation,
                &publication,
            );
            let result = match checked {
                Err(reason) => Err(reason),
                Ok(()) if !*negotiated => Err(RunnerInferenceRejection::InferenceUnsupported),
                Ok(()) => publish_runner_binding(db, connection, &publication)
                    .await
                    .map_err(|error| rejection(&error)),
            };
            Some(match result {
                Ok(receipt) => EdgeServerMessage::InferenceBindingAck { receipt },
                Err(reason) => EdgeServerMessage::InferenceBindingRejected {
                    rejection: RunnerInferenceBindingRejection {
                        operation_id: publication.operation_id.clone(),
                        reason,
                    },
                },
            })
        }
        EdgeClientMessage::InferenceStartEvidence {
            grant, evidence, ..
        } => match record_runner_start_evidence(db, connection, &grant, evidence).await {
            Ok(()) => None,
            Err(error) => Some(EdgeServerMessage::InferenceRejected {
                attempt_id: Some(grant.attempt.attempt_id),
                reason: rejection(&error),
            }),
        },
        EdgeClientMessage::InferenceTerminal { transfer, .. } => {
            let id = transfer.attempt.attempt_id.clone();
            if let Some(existing) = transfers.get_mut(&id) {
                return Some(if existing.header == *transfer {
                    existing.credit(generation)
                } else {
                    EdgeServerMessage::InferenceRejected {
                        attempt_id: Some(id),
                        reason: RunnerInferenceRejection::InvalidEvidence,
                    }
                });
            }
            let result = if transfers.len() >= MAX_CONNECTION_TRANSFERS {
                Err(RunnerInferenceRejection::CapacityUnavailable)
            } else {
                validate_runner_terminal_attempt(db, connection, &transfer.attempt)
                    .await
                    .map_err(|error| rejection(&error))
            };
            let result = result.and_then(|()| ResponseAssembly::new(*transfer));
            Some(match result {
                Ok(mut assembly) => {
                    let credit = assembly.credit(generation);
                    transfers.insert(id, assembly);
                    credit
                }
                Err(reason) => EdgeServerMessage::InferenceRejected {
                    attempt_id: Some(id),
                    reason,
                },
            })
        }
        EdgeClientMessage::InferenceResponseChunk {
            attempt_id, chunk, ..
        } => {
            let Some(assembly) = transfers.get_mut(&attempt_id) else {
                return Some(EdgeServerMessage::InferenceRejected {
                    attempt_id: Some(attempt_id),
                    reason: RunnerInferenceRejection::InvalidEvidence,
                });
            };
            match assembly.append(&chunk) {
                Err(reason) => {
                    transfers.remove(&attempt_id);
                    Some(EdgeServerMessage::InferenceRejected {
                        attempt_id: Some(attempt_id),
                        reason,
                    })
                }
                Ok(false) => {
                    if assembly.credited_through - assembly.bytes.len()
                        < RUNNER_INFERENCE_CHUNK_BYTES
                    {
                        Some(assembly.credit(generation))
                    } else {
                        None
                    }
                }
                Ok(true) => {
                    let assembly = transfers.remove(&attempt_id).expect("present assembly");
                    let valid_hash = format!("{:x}", Sha256::digest(&assembly.bytes))
                        == assembly.header.response_sha256.as_str();
                    let response =
                        serde_json::from_slice::<RunnerInferenceResponse>(&assembly.bytes);
                    let valid_response = response.as_ref().is_ok_and(|response| {
                        response.events.len() <= 131_072
                            && (assembly.header.terminal.status
                                != InferenceTerminalStatus::Succeeded
                                || response.transport.status
                                    == RunnerInferenceTransportStatus::Complete)
                    });
                    if !valid_hash || !valid_response {
                        return Some(EdgeServerMessage::InferenceRejected {
                            attempt_id: Some(attempt_id),
                            reason: RunnerInferenceRejection::InvalidEvidence,
                        });
                    }
                    match take_runner_terminal_custody(
                        db,
                        connection,
                        &assembly.header.attempt,
                        &assembly.header.terminal,
                        &assembly.bytes,
                        &assembly.header.terminal_sha256,
                    )
                    .await
                    {
                        Ok(ack) => Some(EdgeServerMessage::InferenceTerminalAck {
                            ack: Box::new(ack),
                            delivery_generation: generation,
                        }),
                        Err(error) => Some(EdgeServerMessage::InferenceRejected {
                            attempt_id: Some(attempt_id),
                            reason: rejection(&error),
                        }),
                    }
                }
            }
        }
        EdgeClientMessage::InferenceRequestCredit {
            attempt_id,
            next_offset,
            credit_bytes,
            ..
        } => {
            let Some(transfer) = requests.get_mut(&attempt_id) else {
                return Some(EdgeServerMessage::InferenceRejected {
                    attempt_id: Some(attempt_id),
                    reason: RunnerInferenceRejection::InvalidEvidence,
                });
            };
            let through = match request_credit_end(
                transfer.offset,
                next_offset,
                credit_bytes,
                transfer.bytes.len(),
            ) {
                Ok(through) => through,
                Err(reason) => {
                    requests.remove(&attempt_id);
                    return Some(EdgeServerMessage::InferenceRejected {
                        attempt_id: Some(attempt_id),
                        reason,
                    });
                }
            };
            let Ok(text) = std::str::from_utf8(&transfer.bytes) else {
                return None;
            };
            if through <= transfer.offset {
                return None;
            }
            while transfer.offset < through {
                let Some((end, data)) = next_request_chunk(text, transfer.offset, through) else {
                    break;
                };
                if !send(
                    pool,
                    connection,
                    generation,
                    EdgeServerMessage::InferenceRequestChunk {
                        attempt_id: attempt_id.clone(),
                        delivery_generation: generation,
                        chunk: RunnerInferencePayloadChunk {
                            offset: transfer.offset as u32,
                            data,
                        },
                    },
                )
                .await
                {
                    requests.remove(&attempt_id);
                    return None;
                }
                transfer.offset = end;
                transfer.updated = tokio::time::Instant::now();
            }
            if transfer.offset == text.len() {
                requests.remove(&attempt_id);
            }
            None
        }
        _ => None,
    }
}

async fn schedule_deliveries(
    db: &SharedPool,
    pool: &EdgeConnectionPool,
    connection: &AuthenticatedRunnerConnection,
    generation: u64,
    requests: &mut HashMap<RunnerInferenceId, RequestTransfer>,
) {
    let Ok(grants) = list_runner_reconciliation(db, connection, 8).await else {
        return;
    };
    for grant in grants {
        // Do not take a durable delivery claim that this connection cannot
        // materialize. Existing transfers remain claimable so cancellation
        // and reconciliation are never blocked behind admission capacity.
        if requests.len() >= MAX_CONNECTION_TRANSFERS
            && !requests.contains_key(&grant.attempt.attempt_id)
        {
            continue;
        }
        let Ok(Some(claim)) = claim_runner_delivery(db, connection, &grant.attempt).await else {
            continue;
        };
        match claim.action {
            RunnerDeliveryAction::Dispatch(grant) => {
                if requests.len() >= MAX_CONNECTION_TRANSFERS
                    || requests.contains_key(&grant.attempt.attempt_id)
                {
                    continue;
                }
                let size = grant.attempt.request.byte_len.get();
                if size > RUNNER_INFERENCE_ARTIFACT_BYTES as u64 {
                    continue;
                }
                let Ok(reservation) =
                    TRANSFER_KIB.try_acquire_many((size as usize).div_ceil(1024) as u32)
                else {
                    continue;
                };
                let Ok(bytes) = load_runner_request_custody(db, connection, &grant).await else {
                    continue;
                };
                let id = grant.attempt.attempt_id.clone();
                if !send(
                    pool,
                    connection,
                    generation,
                    EdgeServerMessage::InferenceDispatch {
                        grant: Box::new(grant),
                        delivery_generation: generation,
                    },
                )
                .await
                {
                    return;
                }
                requests.insert(
                    id,
                    RequestTransfer {
                        bytes: bytes.into_bytes(),
                        offset: 0,
                        updated: tokio::time::Instant::now(),
                        _reservation: reservation,
                    },
                );
            }
            RunnerDeliveryAction::Cancel(grant) => {
                requests.remove(&grant.attempt.attempt_id);
                if !send(
                    pool,
                    connection,
                    generation,
                    EdgeServerMessage::InferenceCancel {
                        grant: Box::new(grant),
                        delivery_generation: generation,
                    },
                )
                .await
                {
                    return;
                }
            }
            RunnerDeliveryAction::Reconcile(grant) => {
                requests.remove(&grant.attempt.attempt_id);
                if !send(
                    pool,
                    connection,
                    generation,
                    EdgeServerMessage::InferenceReconcile {
                        grant: Box::new(grant),
                        delivery_generation: generation,
                    },
                )
                .await
                {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> RunnerInferenceId {
        RunnerInferenceId::new(value).unwrap()
    }

    fn header(byte_len: usize) -> RunnerInferenceTerminalTransfer {
        serde_json::from_value(serde_json::json!({
            "attempt": {
                "user_id": "owner", "scope": {"kind":"session", "session_id":"session", "turn":0, "round":0, "operation_id":"operation", "logical_attempt":0},
                "invocation_id":"invocation", "attempt_id":"attempt",
                "binding":{"runner_id":"runner","journal_id":"journal","binding_id":"binding","binding_revision":1,"profile_revision":1},
                "request":{"artifact_id":"request","sha256":"a".repeat(64),"byte_len":2}
            },
            "terminal":{"status":"succeeded","usage":{"input":{"fresh_input_tokens":0,"cache_read_tokens":0,"cache_creation_tokens":0},"output_tokens":0},
                "usage_status":"unavailable","provider_response_id":null,"error_kind":null,"error_message":null},
            "response_sha256":"b".repeat(64),"response_bytes":byte_len,"terminal_sha256":"c".repeat(64)
        })).unwrap()
    }

    fn chunk(offset: usize, data: &str) -> RunnerInferencePayloadChunk {
        RunnerInferencePayloadChunk {
            offset: offset as u32,
            data: RunnerInferenceChunkData::new(data.into()).unwrap(),
        }
    }

    #[test]
    fn runner_request_credit_is_cumulative_across_utf8_short_chunks() {
        let text = format!("abc{}", "界".repeat(WINDOW_BYTES));
        let through = request_credit_end(0, 0, WINDOW_BYTES as u32, text.len()).unwrap();
        let mut offset = 0;
        let mut bytes = Vec::new();
        while let Some((end, data)) = next_request_chunk(&text, offset, through) {
            bytes.extend_from_slice(data.as_bytes());
            offset = end;
            if offset == through {
                break;
            }
        }
        assert_eq!(bytes, text.as_bytes()[..offset]);
        assert!(through - offset < 4);
        // Credit was generated before the previous window's last short chunk
        // arrived. Its absolute right edge still authorizes forward progress.
        let early = offset - 9;
        let next =
            request_credit_end(offset, early as u32, WINDOW_BYTES as u32, text.len()).unwrap();
        assert!(next > offset);
        assert!(next_request_chunk(&text, offset, next).is_some());
        assert!(
            request_credit_end(offset, offset as u32 + 1, WINDOW_BYTES as u32, text.len()).is_err()
        );
        assert!(
            request_credit_end(offset, offset as u32, WINDOW_BYTES as u32 + 1, text.len()).is_err()
        );
    }

    #[test]
    fn runner_response_assembly_rejects_gaps_conflicting_duplicates_and_uncredited_bytes() {
        let mut assembly = ResponseAssembly::new(header(WINDOW_BYTES + 1)).unwrap();
        assert!(!assembly.append(&chunk(0, "a")).unwrap());
        assert!(
            !assembly.append(&chunk(0, "a")).unwrap(),
            "identical duplicate adds no bytes"
        );
        assert_eq!(
            assembly.append(&chunk(0, "b")),
            Err(RunnerInferenceRejection::InvalidEvidence)
        );
        assert_eq!(
            assembly.append(&chunk(2, "a")),
            Err(RunnerInferenceRejection::InvalidEvidence)
        );
        assert_eq!(assembly.bytes, b"a");
        while assembly.bytes.len() < WINDOW_BYTES {
            let size = (WINDOW_BYTES - assembly.bytes.len()).min(RUNNER_INFERENCE_CHUNK_BYTES);
            assert!(
                !assembly
                    .append(&chunk(assembly.bytes.len(), &"a".repeat(size)))
                    .unwrap()
            );
        }
        assert_eq!(
            assembly.append(&chunk(WINDOW_BYTES, "z")),
            Err(RunnerInferenceRejection::InvalidEvidence)
        );
        assert!(
            matches!(assembly.credit(7), EdgeServerMessage::InferenceResponseCredit {
            next_offset, credit_bytes: 1, delivery_generation: 7, ..
        } if next_offset == WINDOW_BYTES as u32)
        );
        assert!(assembly.append(&chunk(WINDOW_BYTES, "z")).unwrap());
        assert_eq!(assembly.bytes.len(), WINDOW_BYTES + 1);
        assert!(ResponseAssembly::new(header(RUNNER_INFERENCE_ARTIFACT_BYTES + 1)).is_err());
    }

    #[test]
    fn runner_inference_ingress_fences_owner_runner_generation_and_metadata_bounds() {
        let valid = EdgeClientMessage::InferenceTerminal {
            transfer: Box::new(header(16)),
            delivery_generation: 7,
        };
        assert!(validate_ingress(&valid, "owner", "runner", 7));
        assert!(!validate_ingress(&valid, "other-owner", "runner", 7));
        assert!(!validate_ingress(&valid, "owner", "other-runner", 7));
        assert!(!validate_ingress(&valid, "owner", "runner", 8));
        let mut forged = header(16);
        forged.terminal.error_message = Some("private-canary".repeat(500));
        let forged = EdgeClientMessage::InferenceTerminal {
            transfer: Box::new(forged),
            delivery_generation: 7,
        };
        assert!(!validate_ingress(&forged, "owner", "runner", 7));
        assert!(!format!("{forged:?}").contains("private-canary"));
    }

    #[tokio::test]
    async fn runner_inference_worker_without_durable_owner_never_accepts_or_enrolls() {
        let pool = EdgeConnectionPool::new();
        let (outbound, mut messages) = mpsc::channel(4);
        let generation = pool.register("owner", "runner", None, None, outbound);
        let (sender, receiver) = mpsc::channel(CONTROL_CAPACITY);
        let connection = AuthenticatedRunnerConnection {
            user_id: "owner".into(),
            runner_id: id("runner"),
            edge_id: "edge".into(),
        };
        let worker = tokio::spawn(run(None, pool.clone(), connection, generation, receiver));
        sender
            .send(EdgeClientMessage::InferenceHello {
                protocol_version: 1,
                journal_id: id("journal"),
                process_boot_nonce: id("boot"),
            })
            .await
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(1), messages.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            reply,
            EdgeServerMessage::InferenceHelloAck {
                negotiation: RunnerInferenceNegotiation::Unavailable {
                    reason: RunnerInferenceRejection::InferenceUnsupported
                }
            }
        ));
        assert!(pool.get_pending_requests_for_user("owner").is_empty());
        assert!(pool.get_all_user_edges("owner")[0].capabilities.is_none());
        drop(sender);
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
    }
}
