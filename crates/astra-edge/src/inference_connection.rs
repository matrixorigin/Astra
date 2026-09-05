//! Bounded connection-local transfer bookkeeping. Reconnect discards partial
//! transfers, not execution fences or unacknowledged terminal custody.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use astra_server_types::edge_ws_protocol::{EdgeClientMessage, EdgeServerMessage};
use astra_turn_types::runner_inference::*;
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::inference_host::{
    DispatchOutcome, GrantClock, InferenceHost, InferenceHostError, RetainedTerminal,
};

struct Assembly {
    grant: RunnerInferenceDispatchGrant,
    bytes: Vec<u8>,
    credit_end: usize,
}
struct Outgoing {
    grant: RunnerInferenceDispatchGrant,
    payload: RetainedTerminal,
    sent: usize,
}

pub struct InferenceConnection {
    host: Arc<InferenceHost>,
    hello_sent: Instant,
    generation: Option<u64>,
    clock: Option<GrantClock>,
    incoming: HashMap<String, Assembly>,
    outgoing: Option<Outgoing>,
    publication_sent: Option<RunnerInferenceId>,
}

impl InferenceConnection {
    pub fn new(host: Arc<InferenceHost>) -> Self {
        Self {
            host,
            hello_sent: Instant::now(),
            generation: None,
            clock: None,
            incoming: HashMap::new(),
            outgoing: None,
            publication_sent: None,
        }
    }

    pub fn hello(&mut self) -> EdgeClientMessage {
        self.hello_sent = Instant::now();
        EdgeClientMessage::InferenceHello {
            protocol_version: RUNNER_INFERENCE_PROTOCOL_VERSION,
            journal_id: self.host.journal_id().clone(),
            process_boot_nonce: self.host.process_boot_nonce().clone(),
        }
    }

    fn fence_generation(&self, generation: u64) -> Result<(), InferenceHostError> {
        if self.generation != Some(generation) {
            return Err(InferenceHostError::WrongIncarnation);
        }
        Ok(())
    }

    pub async fn handle(
        &mut self,
        message: EdgeServerMessage,
    ) -> Result<Vec<EdgeClientMessage>, InferenceHostError> {
        match message {
            EdgeServerMessage::InferenceHelloAck { negotiation } => {
                match negotiation {
                    RunnerInferenceNegotiation::Unavailable { .. } => {
                        self.generation = None;
                        self.clock = None;
                        return Ok(Vec::new());
                    }
                    RunnerInferenceNegotiation::Accepted {
                        protocol_version,
                        delivery_generation,
                        max_artifact_bytes,
                        server_unix_ms,
                    } => {
                        if protocol_version != RUNNER_INFERENCE_PROTOCOL_VERSION
                            || max_artifact_bytes as usize != RUNNER_INFERENCE_ARTIFACT_BYTES
                            || delivery_generation == 0
                        {
                            return Err(InferenceHostError::InvalidRequest);
                        }
                        self.clock = Some(GrantClock::observed(
                            server_unix_ms,
                            self.hello_sent,
                            Instant::now(),
                        )?);
                        self.generation = Some(delivery_generation);
                    }
                }
                self.poll().await
            }
            EdgeServerMessage::InferenceBindingAck { receipt } => {
                self.host.publication_ack(receipt).await?;
                self.publication_sent = None;
                self.poll().await
            }
            EdgeServerMessage::InferenceBindingRejected { .. } => {
                // Do not manufacture a newer publication revision to override a
                // rejection. Keep the durable operation available for repair.
                Ok(Vec::new())
            }
            EdgeServerMessage::InferenceDispatch {
                grant,
                delivery_generation,
            } => {
                self.fence_generation(delivery_generation)?;
                let result = self.host.reconcile(&grant).await?;
                if !matches!(result, DispatchOutcome::Unknown) {
                    return self.outcome(*grant, result).await;
                }
                if grant.process_boot_nonce != *self.host.process_boot_nonce()
                    || grant.attempt.request.byte_len.get() > RUNNER_INFERENCE_ARTIFACT_BYTES as u64
                {
                    return Err(InferenceHostError::InvalidRequest);
                }
                if let Some(previous) = self.incoming.get(grant.attempt.attempt_id.as_str()) {
                    if previous.grant != *grant {
                        return Err(InferenceHostError::IdentityConflict);
                    }
                    return Ok(Vec::new());
                }
                if self.incoming.len() >= 4 {
                    return Err(InferenceHostError::Capacity);
                }
                let attempt_id = grant.attempt.attempt_id.clone();
                self.incoming.insert(
                    grant.attempt.attempt_id.as_str().to_owned(),
                    Assembly {
                        grant: *grant,
                        bytes: Vec::new(),
                        credit_end: RUNNER_INFERENCE_CHUNK_BYTES * 8,
                    },
                );
                Ok(vec![EdgeClientMessage::InferenceRequestCredit {
                    attempt_id,
                    delivery_generation,
                    next_offset: 0,
                    credit_bytes: (RUNNER_INFERENCE_CHUNK_BYTES * 8) as u32,
                }])
            }
            EdgeServerMessage::InferenceRequestChunk {
                attempt_id,
                delivery_generation,
                chunk,
            } => {
                self.fence_generation(delivery_generation)?;
                let assembly = self
                    .incoming
                    .get_mut(attempt_id.as_str())
                    .ok_or(InferenceHostError::InvalidRequest)?;
                let offset = chunk.offset as usize;
                let bytes = chunk.data.as_bytes();
                if offset < assembly.bytes.len() {
                    if assembly.bytes.get(offset..offset + bytes.len()) != Some(bytes) {
                        return Err(InferenceHostError::IdentityConflict);
                    }
                    return Ok(Vec::new());
                }
                if offset != assembly.bytes.len()
                    || offset.saturating_add(bytes.len()) > assembly.credit_end
                    || bytes.len()
                        > (assembly.grant.attempt.request.byte_len.get() as usize)
                            .saturating_sub(offset)
                {
                    return Err(InferenceHostError::InvalidRequest);
                }
                assembly.bytes.extend_from_slice(bytes);
                if assembly.bytes.len() as u64 != assembly.grant.attempt.request.byte_len.get() {
                    // Replenish before the final chunk: UTF-8 boundaries may
                    // leave a few bytes unused in each bounded frame.
                    if assembly.credit_end.saturating_sub(assembly.bytes.len())
                        < RUNNER_INFERENCE_CHUNK_BYTES
                    {
                        assembly.credit_end =
                            assembly.bytes.len() + RUNNER_INFERENCE_CHUNK_BYTES * 8;
                        return Ok(vec![EdgeClientMessage::InferenceRequestCredit {
                            attempt_id,
                            delivery_generation,
                            next_offset: assembly.bytes.len() as u32,
                            credit_bytes: (RUNNER_INFERENCE_CHUNK_BYTES * 8) as u32,
                        }]);
                    }
                    return Ok(Vec::new());
                }
                let assembly = self
                    .incoming
                    .remove(attempt_id.as_str())
                    .ok_or(InferenceHostError::InvalidRequest)?;
                let request_json = String::from_utf8(assembly.bytes)
                    .map_err(|_| InferenceHostError::InvalidRequest)?;
                let result = self
                    .host
                    .dispatch(
                        assembly.grant.clone(),
                        request_json,
                        self.clock.ok_or(InferenceHostError::WrongIncarnation)?,
                    )
                    .await?;
                self.outcome(assembly.grant, result).await
            }
            EdgeServerMessage::InferenceCancel {
                grant,
                delivery_generation,
            } => {
                self.fence_generation(delivery_generation)?;
                self.incoming.remove(grant.attempt.attempt_id.as_str());
                let result = self.host.cancel(&grant).await?;
                self.outcome(*grant, result).await
            }
            EdgeServerMessage::InferenceReconcile {
                grant,
                delivery_generation,
            } => {
                self.fence_generation(delivery_generation)?;
                let result = self.host.reconcile(&grant).await?;
                self.outcome(*grant, result).await
            }
            EdgeServerMessage::InferenceTerminalAck {
                ack,
                delivery_generation,
            } => {
                self.fence_generation(delivery_generation)?;
                self.host.acknowledge((*ack).clone()).await?;
                if self
                    .outgoing
                    .as_ref()
                    .is_some_and(|outgoing| outgoing.grant.attempt == ack.attempt)
                {
                    self.outgoing = None;
                }
                self.poll().await
            }
            EdgeServerMessage::InferenceResponseCredit {
                attempt_id,
                delivery_generation,
                next_offset,
                credit_bytes,
            } => {
                self.fence_generation(delivery_generation)?;
                let outgoing = self
                    .outgoing
                    .as_mut()
                    .filter(|outgoing| outgoing.grant.attempt.attempt_id == attempt_id)
                    .ok_or(InferenceHostError::IdentityConflict)?;
                let start = next_offset as usize;
                if start > outgoing.sent
                    || credit_bytes == 0
                    || credit_bytes as usize > RUNNER_INFERENCE_CHUNK_BYTES * 8
                    || !outgoing.payload.response_json.is_char_boundary(start)
                {
                    return Err(InferenceHostError::InvalidRequest);
                }
                let stop = start
                    .saturating_add(credit_bytes as usize)
                    .min(outgoing.payload.response_json.len());
                let mut offset = outgoing.sent;
                let mut messages = Vec::new();
                while offset < stop {
                    let mut end = (offset + RUNNER_INFERENCE_CHUNK_BYTES).min(stop);
                    while !outgoing.payload.response_json.is_char_boundary(end) {
                        end -= 1;
                    }
                    if end == offset {
                        break;
                    }
                    let data = RunnerInferenceChunkData::new(
                        outgoing.payload.response_json[offset..end].to_owned(),
                    )
                    .map_err(|_| InferenceHostError::InvalidRequest)?;
                    messages.push(EdgeClientMessage::InferenceResponseChunk {
                        attempt_id: attempt_id.clone(),
                        delivery_generation,
                        chunk: RunnerInferencePayloadChunk {
                            offset: offset as u32,
                            data,
                        },
                    });
                    offset = end;
                }
                outgoing.sent = outgoing.sent.max(offset);
                Ok(messages)
            }
            EdgeServerMessage::InferenceRejected { attempt_id, reason } => match reason {
                RunnerInferenceRejection::StorageUnavailable
                | RunnerInferenceRejection::CapacityUnavailable
                | RunnerInferenceRejection::PublicationConflict => {
                    // The Server did not accept custody. Drop only disposable
                    // transfer state; the journal remains the source of truth
                    // and the next bounded poll reoffers the identical fact.
                    if attempt_id.as_ref().is_some_and(|attempt_id| {
                        self.outgoing.as_ref().is_some_and(|outgoing| {
                            outgoing.grant.attempt.attempt_id == *attempt_id
                        })
                    }) {
                        self.outgoing = None;
                    }
                    Ok(Vec::new())
                }
                RunnerInferenceRejection::InferenceUnsupported
                | RunnerInferenceRejection::ProtocolVersionUnsupported
                | RunnerInferenceRejection::ConnectionSuperseded
                | RunnerInferenceRejection::BindingIdentityMismatch
                | RunnerInferenceRejection::InvalidEvidence => {
                    Err(InferenceHostError::InvalidRequest)
                }
            },
            _ => Err(InferenceHostError::InvalidRequest),
        }
    }

    pub async fn poll(&mut self) -> Result<Vec<EdgeClientMessage>, InferenceHostError> {
        if self.generation.is_none() {
            return Ok(Vec::new());
        }
        let mut messages = Vec::new();
        // Configuration repair must not prevent custody delivery for an
        // already-fenced attempt. Publication remains pending until repaired.
        if let Ok(Some(publication)) = self.host.next_publication().await
            && self.publication_sent.as_ref() != Some(&publication.operation_id)
        {
            self.publication_sent = Some(publication.operation_id.clone());
            messages.push(EdgeClientMessage::InferenceBindingPublish {
                publication: Box::new(publication),
            });
        }
        if self.outgoing.is_none()
            && let Some((grant, payload)) = self.host.pending(1).await?.pop()
        {
            messages.extend(self.begin_transfer(grant, payload)?);
        }
        if let Some(clock) = self.clock {
            let expired: Vec<_> = self
                .incoming
                .values()
                .filter(|assembly| {
                    assembly.grant.start_before_unix_ms <= clock.latest_server_time()
                })
                .map(|assembly| assembly.grant.clone())
                .collect();
            for grant in expired {
                self.incoming.remove(grant.attempt.attempt_id.as_str());
                let result = self
                    .host
                    .dispatch(grant.clone(), String::new(), clock)
                    .await?;
                messages.extend(self.outcome(grant, result).await?);
            }
        }
        Ok(messages)
    }

    async fn outcome(
        &mut self,
        grant: RunnerInferenceDispatchGrant,
        result: DispatchOutcome,
    ) -> Result<Vec<EdgeClientMessage>, InferenceHostError> {
        let delivery_generation = self
            .generation
            .ok_or(InferenceHostError::WrongIncarnation)?;
        match result {
            DispatchOutcome::Started | DispatchOutcome::Active => {
                Ok(vec![EdgeClientMessage::InferenceStartEvidence {
                    grant: Box::new(grant),
                    delivery_generation,
                    evidence: RunnerInferenceStartEvidence::FenceCommitted,
                }])
            }
            DispatchOutcome::NotStarted(evidence) => {
                Ok(vec![EdgeClientMessage::InferenceStartEvidence {
                    grant: Box::new(grant),
                    delivery_generation,
                    evidence,
                }])
            }
            DispatchOutcome::Terminal(payload) if self.outgoing.is_none() => {
                self.begin_transfer(grant, payload)
            }
            _ => Ok(Vec::new()),
        }
    }

    fn begin_transfer(
        &mut self,
        grant: RunnerInferenceDispatchGrant,
        payload: RetainedTerminal,
    ) -> Result<Vec<EdgeClientMessage>, InferenceHostError> {
        let transfer = RunnerInferenceTerminalTransfer {
            attempt: grant.attempt.clone(),
            terminal: payload.terminal.clone(),
            response_sha256: RunnerInferenceDigest::new(format!(
                "{:x}",
                Sha256::digest(payload.response_json.as_bytes())
            ))
            .map_err(|_| InferenceHostError::Corrupt)?,
            response_bytes: NonZeroU32::new(payload.response_json.len() as u32)
                .ok_or(InferenceHostError::Corrupt)?,
            terminal_sha256: payload.terminal_sha256.clone(),
        };
        self.outgoing = Some(Outgoing {
            grant,
            payload,
            sent: 0,
        });
        Ok(vec![EdgeClientMessage::InferenceTerminal {
            transfer: Box::new(transfer),
            delivery_generation: self
                .generation
                .ok_or(InferenceHostError::WrongIncarnation)?,
        }])
    }
}

/// Disk work and provider bookkeeping never block the tool socket reader.
/// Dropping this worker discards only delivery state; the host retains active
/// provider tasks and durable terminal custody across connection replacement.
pub struct InferenceConnectionWorker {
    pub commands: tokio::sync::mpsc::Sender<EdgeServerMessage>,
    pub messages: tokio::sync::mpsc::Receiver<EdgeClientMessage>,
    task: tokio::task::JoinHandle<()>,
}

impl InferenceConnectionWorker {
    pub fn spawn(host: Arc<InferenceHost>) -> Self {
        let (commands, mut input) = tokio::sync::mpsc::channel(32);
        let (output, messages) = tokio::sync::mpsc::channel(16);
        let task = tokio::spawn(async move {
            let mut connection = InferenceConnection::new(host.clone());
            if output.send(connection.hello()).await.is_err() {
                return;
            }
            let mut poll = tokio::time::interval(std::time::Duration::from_secs(1));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let result = tokio::select! {
                    message = input.recv() => match message {
                        Some(message) => connection.handle(message).await,
                        None => break,
                    },
                    _ = host.terminal_ready() => connection.poll().await,
                    _ = poll.tick() => connection.poll().await,
                };
                match result {
                    Ok(messages) => {
                        for message in messages {
                            if output.send(message).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(category = %error, "Runner inference connection stopped; durable custody retained");
                        break;
                    }
                }
            }
        });
        Self {
            commands,
            messages,
            task,
        }
    }
}

impl Drop for InferenceConnectionWorker {
    fn drop(&mut self) {
        self.task.abort();
    }
}
