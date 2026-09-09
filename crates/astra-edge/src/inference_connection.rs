//! Bounded connection-local transfer bookkeeping. Reconnect discards partial
//! transfers, not execution fences or unacknowledged terminal custody.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use astra_server_types::edge_ws_protocol::{EdgeClientMessage, EdgeServerMessage};
use astra_turn_types::runner_inference::*;
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::inference_host::{
    DispatchOutcome, GrantClock, InferenceHost, InferenceHostError, InferencePreview,
    RetainedTerminal,
};

struct PendingProgress {
    attempt: RunnerInferenceAttemptIdentity,
    events: Vec<RunnerInferenceProgressEvent>,
    last_sequence: u64,
    created: Instant,
}

/// The host admits at most four provider executions. Keep disposable progress
/// assembly no larger than that active set even if terminal polling is delayed
/// by a storage outage; terminal custody remains the recovery path.
const MAX_PENDING_PROGRESS: usize = 4;

#[cfg(test)]
pub(crate) const MAX_PENDING_PROGRESS_FOR_TEST: usize = MAX_PENDING_PROGRESS;

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

type HostActionResult = (
    RunnerInferenceDispatchGrant,
    Result<DispatchOutcome, InferenceHostError>,
);

pub struct InferenceConnection {
    host: Arc<InferenceHost>,
    hello_sent: Instant,
    generation: Option<u64>,
    clock: Option<GrantClock>,
    incoming: HashMap<String, Assembly>,
    outgoing: Option<Outgoing>,
    publication_sent: Option<RunnerInferenceId>,
    hello_retry_at: Option<Instant>,
    publication_retry_at: Option<Instant>,
    pending_progress: HashMap<String, PendingProgress>,
    actions: tokio::task::JoinSet<HostActionResult>,
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
            hello_retry_at: None,
            publication_retry_at: None,
            pending_progress: HashMap::new(),
            actions: tokio::task::JoinSet::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn pending_progress_len_for_test(&self) -> usize {
        self.pending_progress.len()
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

    /// Convert one disposable host preview into a bounded wire batch.  The
    /// first event is sent immediately to minimize TTFB; subsequent events are
    /// coalesced until the byte/timer budget or a provider terminal marker.
    pub(crate) async fn handle_preview(
        &mut self,
        preview: InferencePreview,
    ) -> Result<Vec<EdgeClientMessage>, InferenceHostError> {
        let Some(generation) = self.generation else {
            // Negotiation has not completed (or was refused).  The host keeps
            // terminal custody independently; dropping this preview is safe.
            return Ok(Vec::new());
        };
        if !self.host.is_attempt_active(&preview.attempt).await {
            // A provider task removes itself from the active-attempt set only
            // after terminal custody is durable. A broadcast event that was
            // queued before that transition is therefore stale; do not let it
            // recreate connection-local progress after terminal delivery.
            return Ok(Vec::new());
        }
        let key = preview.attempt.attempt_id.as_str().to_owned();
        let item = RunnerInferenceProgressEvent {
            sequence: preview.sequence,
            event: preview.event,
        };
        let terminal_marker = matches!(
            &item.event,
            RunnerInferenceProviderEvent::Done | RunnerInferenceProviderEvent::Eof
        );
        let first = !self.pending_progress.contains_key(&key);
        if first {
            let Ok(batch) = RunnerInferenceProgressBatch::new(preview.attempt, vec![item.clone()])
            else {
                // The event is too large for the complete bounded envelope.
                // It remains available in terminal custody; expose the gap.
                return Ok(Vec::new());
            };
            if !terminal_marker {
                if self.pending_progress.len() >= MAX_PENDING_PROGRESS {
                    // Do not evict another active attempt's sequence
                    // watermark. The next terminal poll retires stale entries;
                    // this attempt still has complete custody available.
                    return Ok(Vec::new());
                }
                self.pending_progress.insert(
                    key,
                    PendingProgress {
                        attempt: batch.attempt.clone(),
                        events: Vec::new(),
                        last_sequence: item.sequence,
                        created: Instant::now(),
                    },
                );
            }
            // The first provider event is deliberately prompt.  This is still
            // one bounded batch and does not couple provider I/O to the socket.
            return Ok(vec![Self::progress_message(batch, generation)]);
        }

        let (attempt, mut events, last_sequence) = {
            let pending = self
                .pending_progress
                .get(&key)
                .ok_or(InferenceHostError::IdentityConflict)?;
            if pending.attempt != preview.attempt {
                return Err(InferenceHostError::IdentityConflict);
            }
            (
                pending.attempt.clone(),
                pending.events.clone(),
                pending.last_sequence,
            )
        };
        // Duplicate provider events can only be a faulty host or a replay from
        // a reconnect.  They are disposable and must never duplicate client
        // output, so silently retain the monotonic watermark and drop them.
        if item.sequence <= last_sequence {
            return Ok(Vec::new());
        }
        events.push(item.clone());
        let mut messages = Vec::new();
        match RunnerInferenceProgressBatch::new(attempt.clone(), events.clone()) {
            Ok(batch) => {
                let flush_now = terminal_marker
                    || serde_json::to_vec(&batch)
                        .map(|bytes| bytes.len() >= RUNNER_INFERENCE_PROGRESS_BATCH_BYTES)
                        .unwrap_or(true);
                if flush_now {
                    {
                        let pending = self
                            .pending_progress
                            .get_mut(&key)
                            .ok_or(InferenceHostError::IdentityConflict)?;
                        pending.last_sequence = item.sequence;
                        pending.events.clear();
                        pending.created = Instant::now();
                    }
                    messages.push(Self::progress_message(batch, generation));
                    if terminal_marker {
                        self.pending_progress.remove(&key);
                    }
                } else {
                    let pending = self
                        .pending_progress
                        .get_mut(&key)
                        .ok_or(InferenceHostError::IdentityConflict)?;
                    pending.last_sequence = item.sequence;
                    pending.events = events;
                }
            }
            Err(_) => {
                // The candidate crossed the byte budget.  Flush the previous
                // batch and retain this event as the first event of the next
                // bounded batch.  An oversized singleton is a visible gap.
                let (pending_attempt, previous) = {
                    let pending = self
                        .pending_progress
                        .get_mut(&key)
                        .ok_or(InferenceHostError::IdentityConflict)?;
                    let previous = if !pending.events.is_empty() {
                        RunnerInferenceProgressBatch::new(
                            pending.attempt.clone(),
                            std::mem::take(&mut pending.events),
                        )
                        .ok()
                    } else {
                        None
                    };
                    pending.last_sequence = item.sequence;
                    pending.created = Instant::now();
                    (pending.attempt.clone(), previous)
                };
                if let Some(previous) = previous {
                    messages.push(Self::progress_message(previous, generation));
                }
                if terminal_marker {
                    if let Ok(singleton) =
                        RunnerInferenceProgressBatch::new(pending_attempt, vec![item])
                    {
                        messages.push(Self::progress_message(singleton, generation));
                    }
                    self.pending_progress.remove(&key);
                } else if RunnerInferenceProgressBatch::new(pending_attempt, vec![item.clone()])
                    .is_ok()
                    && let Some(pending) = self.pending_progress.get_mut(&key)
                {
                    pending.events.push(item);
                }
            }
        }
        Ok(messages)
    }

    fn progress_message(batch: RunnerInferenceProgressBatch, generation: u64) -> EdgeClientMessage {
        EdgeClientMessage::InferenceProgress {
            progress: Box::new(batch),
            delivery_generation: generation,
        }
    }

    /// Flush progress that has reached the 50ms latency budget.  Progress is
    /// best-effort; terminal/control polling remains on the awaited channel.
    pub(crate) fn flush_progress(&mut self) -> Vec<EdgeClientMessage> {
        let Some(generation) = self.generation else {
            self.pending_progress.clear();
            return Vec::new();
        };
        let expired = self
            .pending_progress
            .iter()
            .filter(|(_, pending)| {
                !pending.events.is_empty() && pending.created.elapsed() >= Duration::from_millis(50)
            })
            .map(|(attempt_id, _)| attempt_id.clone())
            .collect::<Vec<_>>();
        let mut messages = Vec::new();
        for attempt_id in expired {
            let Some(pending) = self.pending_progress.get_mut(&attempt_id) else {
                continue;
            };
            let events = std::mem::take(&mut pending.events);
            pending.created = Instant::now();
            if let Ok(batch) = RunnerInferenceProgressBatch::new(pending.attempt.clone(), events) {
                messages.push(Self::progress_message(batch, generation));
            }
        }
        messages
    }

    pub async fn handle(
        &mut self,
        message: EdgeServerMessage,
    ) -> Result<Vec<EdgeClientMessage>, InferenceHostError> {
        match message {
            EdgeServerMessage::InferenceHelloAck { negotiation } => {
                match negotiation {
                    RunnerInferenceNegotiation::Unavailable { reason } => {
                        self.generation = None;
                        self.clock = None;
                        self.pending_progress.clear();
                        self.hello_retry_at = matches!(
                            reason,
                            RunnerInferenceRejection::StorageUnavailable
                                | RunnerInferenceRejection::CapacityUnavailable
                        )
                        .then(|| Instant::now() + Duration::from_secs(5));
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
                        if self.generation != Some(delivery_generation) {
                            // A reconnect has a new transport generation. Any
                            // old preview assembly is disposable and must not
                            // be mixed with the new socket's sequence stream.
                            self.pending_progress.clear();
                        }
                        self.clock = Some(GrantClock::observed(
                            server_unix_ms,
                            self.hello_sent,
                            Instant::now(),
                        )?);
                        self.generation = Some(delivery_generation);
                        self.hello_retry_at = None;
                    }
                }
                self.poll().await
            }
            EdgeServerMessage::InferenceBindingAck { receipt } => {
                self.host.publication_ack(receipt).await?;
                self.publication_sent = None;
                self.publication_retry_at = None;
                self.poll().await
            }
            EdgeServerMessage::InferenceBindingRejected { rejection } => {
                // Do not manufacture a newer publication revision to override a
                // rejection. Keep the durable operation available for repair.
                if self.publication_sent.as_ref() == Some(&rejection.operation_id) {
                    self.publication_retry_at = matches!(
                        rejection.reason,
                        RunnerInferenceRejection::StorageUnavailable
                            | RunnerInferenceRejection::CapacityUnavailable
                    )
                    .then(|| Instant::now() + Duration::from_secs(5));
                }
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
                if self.incoming.len() >= RUNNER_INFERENCE_MAX_TRANSFERS {
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
                if self.actions.len() >= RUNNER_INFERENCE_MAX_TRANSFERS * 2 {
                    return Err(InferenceHostError::Capacity);
                }
                let host = self.host.clone();
                let clock = self.clock.ok_or(InferenceHostError::WrongIncarnation)?;
                self.actions.spawn(async move {
                    let result = host
                        .dispatch(assembly.grant.clone(), request_json, clock)
                        .await;
                    (assembly.grant, result)
                });
                Ok(Vec::new())
            }
            EdgeServerMessage::InferenceCancel {
                grant,
                delivery_generation,
            } => {
                self.fence_generation(delivery_generation)?;
                self.incoming.remove(grant.attempt.attempt_id.as_str());
                let signal = self.host.signal_cancel(&grant)?;
                if self.actions.len() >= RUNNER_INFERENCE_MAX_TRANSFERS * 2 {
                    return Err(InferenceHostError::Capacity);
                }
                let host = self.host.clone();
                self.actions.spawn(async move {
                    let _signal = signal;
                    let result = host.cancel(&grant).await;
                    (*grant, result)
                });
                Ok(Vec::new())
            }
            EdgeServerMessage::InferenceReconcile {
                grant,
                delivery_generation,
            } => {
                self.fence_generation(delivery_generation)?;
                let mut result = self.host.reconcile(&grant).await?;
                if matches!(result, DispatchOutcome::Unknown)
                    && grant.process_boot_nonce == *self.host.process_boot_nonce()
                    && let Some(clock) = self.clock
                    && grant.start_before_unix_ms <= clock.latest_server_time()
                {
                    // A queued grant may expire before request transfer. Only
                    // the exact current incarnation can now seal no-start;
                    // a replaced process must keep absence as unknown.
                    result = self
                        .host
                        .dispatch((*grant).clone(), String::new(), clock)
                        .await?;
                }
                self.outcome(*grant, result).await
            }
            EdgeServerMessage::InferenceTerminalAck {
                ack,
                delivery_generation,
            } => {
                self.fence_generation(delivery_generation)?;
                self.host.acknowledge((*ack).clone()).await?;
                self.pending_progress
                    .remove(ack.attempt.attempt_id.as_str());
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
            EdgeServerMessage::InferenceRejected { attempt_id, reason } => {
                if let Some(attempt_id) = attempt_id.as_ref() {
                    self.pending_progress.remove(attempt_id.as_str());
                }
                match reason {
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
                }
            }
            _ => Err(InferenceHostError::InvalidRequest),
        }
    }

    pub async fn poll(&mut self) -> Result<Vec<EdgeClientMessage>, InferenceHostError> {
        if self.generation.is_none() {
            if self.hello_retry_at.is_some_and(|at| Instant::now() >= at) {
                self.hello_retry_at = None;
                return Ok(vec![self.hello()]);
            }
            return Ok(Vec::new());
        }
        if self
            .publication_retry_at
            .is_some_and(|at| Instant::now() >= at)
        {
            self.publication_sent = None;
            self.publication_retry_at = None;
        }
        let mut messages = Vec::new();
        while let Some(action) = self.actions.try_join_next() {
            let (grant, result) = action.map_err(|_| InferenceHostError::JournalIo)?;
            messages.extend(self.outcome(grant, result?).await?);
        }
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
        let pending = self.host.pending(MAX_PENDING_PROGRESS).await?;
        for (grant, _) in &pending {
            // A terminal outcome (including provider transport failure) owns
            // the sequence's end. Retire only the disposable preview assembly;
            // response bytes remain in the journal until its ACK.
            self.pending_progress
                .remove(grant.attempt.attempt_id.as_str());
        }
        if self.outgoing.is_none()
            && let Some((grant, payload)) = pending.into_iter().next()
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
                self.pending_progress
                    .remove(grant.attempt.attempt_id.as_str());
                Ok(vec![EdgeClientMessage::InferenceStartEvidence {
                    grant: Box::new(grant),
                    delivery_generation,
                    evidence,
                }])
            }
            DispatchOutcome::Terminal(payload) => {
                self.pending_progress
                    .remove(grant.attempt.attempt_id.as_str());
                if self.outgoing.is_none() {
                    self.begin_transfer(grant, payload)
                } else {
                    Ok(Vec::new())
                }
            }
            DispatchOutcome::Acknowledged | DispatchOutcome::Unknown => {
                self.pending_progress
                    .remove(grant.attempt.attempt_id.as_str());
                Ok(Vec::new())
            }
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
    ingress: tokio::task::JoinHandle<()>,
}

impl InferenceConnectionWorker {
    pub fn spawn(host: Arc<InferenceHost>) -> Self {
        let (commands, mut ingress_input) = tokio::sync::mpsc::channel(32);
        let (forward, mut input) = tokio::sync::mpsc::channel(32);
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let ingress_host = host.clone();
        let ingress_generation = generation.clone();
        let ingress = tokio::spawn(async move {
            while let Some(message) = ingress_input.recv().await {
                let signal = match &message {
                    EdgeServerMessage::InferenceCancel {
                        grant,
                        delivery_generation,
                    } if *delivery_generation != 0
                        && *delivery_generation
                            == ingress_generation.load(std::sync::atomic::Ordering::Acquire) =>
                    {
                        ingress_host.signal_cancel(grant).ok()
                    }
                    _ => None,
                };
                if forward.send((message, signal)).await.is_err() {
                    break;
                }
            }
        });
        let (output, messages) = tokio::sync::mpsc::channel(16);
        let task = tokio::spawn(async move {
            let mut connection = InferenceConnection::new(host.clone());
            let mut preview_rx = host.subscribe_preview();
            let mut preview_open = true;
            if output.send(connection.hello()).await.is_err() {
                return;
            }
            let mut poll = tokio::time::interval(std::time::Duration::from_secs(1));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut progress_flush = tokio::time::interval(Duration::from_millis(10));
            progress_flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let result = tokio::select! {
                    message = input.recv() => match message {
                        Some((message, _signal)) => connection.handle(message).await,
                        None => break,
                    },
                    preview = preview_rx.recv(), if preview_open => match preview {
                        Ok(preview) => connection.handle_preview(preview).await,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Ok(Vec::new()),
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            preview_open = false;
                            Ok(Vec::new())
                        }
                    },
                    _ = host.terminal_ready() => connection.poll().await,
                    _ = poll.tick() => connection.poll().await,
                    _ = progress_flush.tick() => Ok(connection.flush_progress()),
                    action = connection.actions.join_next(), if !connection.actions.is_empty() => {
                        match action {
                            Some(Ok((grant, result))) => match result {
                                Ok(outcome) => connection.outcome(grant, outcome).await,
                                Err(error) => Err(error),
                            },
                            _ => Err(InferenceHostError::JournalIo),
                        }
                    },
                };
                generation.store(
                    connection.generation.unwrap_or(0),
                    std::sync::atomic::Ordering::Release,
                );
                match result {
                    Ok(messages) => {
                        for message in messages {
                            // Progress is disposable and must not occupy the
                            // bounded control/terminal output lane. A full
                            // lane is represented by a sequence gap; terminal
                            // custody remains available on the next poll.
                            if matches!(&message, EdgeClientMessage::InferenceProgress { .. }) {
                                let _ = output.try_send(message);
                            } else if output.send(message).await.is_err() {
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
            ingress,
        }
    }
}

impl Drop for InferenceConnectionWorker {
    fn drop(&mut self) {
        self.task.abort();
        self.ingress.abort();
    }
}
