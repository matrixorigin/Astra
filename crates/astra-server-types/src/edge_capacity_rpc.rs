//! Live, generation-fenced transport for explicitly configured Edge capacity
//! providers. This is not a model tool or an invocation ledger. The caller owns
//! authorization, end-to-end sealing, idempotency and execution receipts.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::{Semaphore, oneshot};

use crate::edge_connection_pool::{EdgeConnectionInfo, EdgeConnectionPool};
use crate::edge_ws_protocol::EdgeServerMessage;
pub use crate::edge_ws_protocol::{
    CapacityRpcFailure, EdgeCapacityProvider, MAX_CAPACITY_RPC_INFLIGHT,
    MAX_CAPACITY_RPC_TIMEOUT_SECS, SealedCapacityPayload, valid_capacity_identity,
};

pub fn advertised_provider(
    info: &EdgeConnectionInfo,
    provider: &str,
) -> Option<EdgeCapacityProvider> {
    let providers: Vec<EdgeCapacityProvider> =
        serde_json::from_value(info.capabilities.as_ref()?.get("capacity_rpc_v1")?.clone()).ok()?;
    providers.into_iter().find(|item| item.id == provider)
}

#[derive(Debug)]
struct PendingCapacityRpc {
    user_id: String,
    edge_id: String,
    generation: u64,
    sender: oneshot::Sender<Result<SealedCapacityPayload, CapacityRpcFailure>>,
}

#[derive(Debug, Clone)]
pub(crate) struct CapacityRpcBroker {
    waiters: Arc<DashMap<String, PendingCapacityRpc>>,
    slots: Arc<Semaphore>,
}

impl Default for CapacityRpcBroker {
    fn default() -> Self {
        Self {
            waiters: Arc::new(DashMap::new()),
            slots: Arc::new(Semaphore::new(MAX_CAPACITY_RPC_INFLIGHT)),
        }
    }
}

impl CapacityRpcBroker {
    pub(crate) fn disconnect(&self, user_id: &str, edge_id: &str, generation: u64) {
        let ids: Vec<_> = self
            .waiters
            .iter()
            .filter(|item| {
                item.user_id == user_id && item.edge_id == edge_id && item.generation == generation
            })
            .map(|item| item.key().clone())
            .collect();
        for id in ids {
            if let Some((_, pending)) = self.waiters.remove(&id) {
                let _ = pending.sender.send(Err(CapacityRpcFailure::OutcomeUnknown));
            }
        }
    }
}

struct WaiterGuard {
    id: String,
    waiters: Arc<DashMap<String, PendingCapacityRpc>>,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        self.waiters.remove(&self.id);
    }
}

pub struct EdgeCapacityRpcRequest<'a> {
    pub user_id: &'a str,
    pub workspace_id: &'a str,
    pub edge_id: &'a str,
    pub provider_id: &'a str,
    pub connection_id: String,
    pub timeout_secs: u64,
    pub payload: SealedCapacityPayload,
}

impl EdgeConnectionPool {
    pub async fn call_capacity_provider(
        &self,
        request: EdgeCapacityRpcRequest<'_>,
    ) -> Result<SealedCapacityPayload, CapacityRpcFailure> {
        if [
            request.user_id,
            request.workspace_id,
            request.edge_id,
            request.provider_id,
        ]
        .iter()
        .any(|value| !valid_capacity_identity(value))
            || !valid_capacity_identity(&request.connection_id)
            || !(1..=MAX_CAPACITY_RPC_TIMEOUT_SECS).contains(&request.timeout_secs)
            || !request.payload.is_valid()
        {
            return Err(CapacityRpcFailure::Invalid);
        }
        let info = self
            .find_user_edge_by_agent_and_workspace(
                request.user_id,
                request.edge_id,
                Some(request.workspace_id),
            )
            .ok_or(CapacityRpcFailure::Offline)?;
        if info.connection_id != request.connection_id {
            return Err(CapacityRpcFailure::BindingChanged);
        }
        let _permit = self
            .capacity_rpc
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| CapacityRpcFailure::Busy)?;
        let id = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        self.capacity_rpc.waiters.insert(
            id.clone(),
            PendingCapacityRpc {
                user_id: request.user_id.to_owned(),
                edge_id: request.edge_id.to_owned(),
                generation: info.generation,
                sender,
            },
        );
        let _guard = WaiterGuard {
            id: id.clone(),
            waiters: self.capacity_rpc.waiters.clone(),
        };
        self.send_capacity_request(
            &request,
            EdgeServerMessage::CapacityRequest {
                request_id: id,
                connection_generation: info.generation,
                provider_id: request.provider_id.to_owned(),
                payload: request.payload.clone(),
                timeout_secs: request.timeout_secs,
            },
        )?;
        match tokio::time::timeout(Duration::from_secs(request.timeout_secs), receiver).await {
            Ok(Ok(result)) => result,
            _ => Err(CapacityRpcFailure::OutcomeUnknown),
        }
    }

    pub fn complete_capacity_rpc(
        &self,
        user_id: &str,
        edge_id: &str,
        socket_generation: u64,
        request_id: &str,
        claimed_generation: u64,
        result: Result<SealedCapacityPayload, CapacityRpcFailure>,
    ) -> bool {
        if socket_generation != claimed_generation
            || result.as_ref().is_ok_and(|payload| !payload.is_valid())
        {
            return false;
        }
        let Some((_, pending)) = self
            .capacity_rpc
            .waiters
            .remove_if(request_id, |_, pending| {
                pending.user_id == user_id
                    && pending.edge_id == edge_id
                    && pending.generation == socket_generation
            })
        else {
            return false;
        };
        pending.sender.send(result).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn route(pool: &EdgeConnectionPool, generation: u64) -> EdgeCapacityRpcRequest<'static> {
        let info = pool
            .find_user_edge_by_agent_and_workspace("owner", "runner", Some("workspace"))
            .unwrap();
        EdgeCapacityRpcRequest {
            user_id: "owner",
            workspace_id: "workspace",
            edge_id: "runner",
            provider_id: "approved",
            connection_id: if generation == info.generation {
                info.connection_id
            } else {
                format!("ws-{}", uuid::Uuid::new_v4())
            },
            timeout_secs: 5,
            payload: SealedCapacityPayload("c2VhbGVk".into()),
        }
    }

    fn connect(
        pool: &EdgeConnectionPool,
    ) -> (
        u64,
        mpsc::Receiver<crate::edge_connection_pool::EdgeOutbound>,
    ) {
        let (sender, receiver) = mpsc::channel(8);
        let generation = pool.register_with_capabilities(
            "owner",
            "runner",
            None,
            None,
            Some(json!({"capacity_rpc_v1":[{"id":"approved","public_key":"public"}]})),
            Some("workspace".into()),
            sender,
        );
        (generation, receiver)
    }

    #[tokio::test]
    async fn capacity_rpc_rejects_wrong_scope_generation_and_unapproved_provider_before_send() {
        let pool = EdgeConnectionPool::new();
        let (generation, mut receiver) = connect(&pool);
        for (request, expected) in [
            (
                EdgeCapacityRpcRequest {
                    workspace_id: "other",
                    ..route(&pool, generation)
                },
                CapacityRpcFailure::Offline,
            ),
            (
                route(&pool, generation + 1),
                CapacityRpcFailure::BindingChanged,
            ),
            (
                EdgeCapacityRpcRequest {
                    user_id: "other",
                    ..route(&pool, generation)
                },
                CapacityRpcFailure::Offline,
            ),
            (
                EdgeCapacityRpcRequest {
                    provider_id: "unapproved",
                    ..route(&pool, generation)
                },
                CapacityRpcFailure::Unsupported,
            ),
            (
                EdgeCapacityRpcRequest {
                    timeout_secs: 131,
                    ..route(&pool, generation)
                },
                CapacityRpcFailure::Invalid,
            ),
        ] {
            assert_eq!(
                pool.call_capacity_provider(request).await.unwrap_err(),
                expected
            );
            assert!(receiver.try_recv().is_err());
            assert!(pool.capacity_rpc.waiters.is_empty());
        }
    }

    #[tokio::test]
    async fn capacity_rpc_fences_result_identity_and_disconnect_never_replays() {
        let pool = EdgeConnectionPool::new();
        let (generation, mut receiver) = connect(&pool);
        let call_pool = pool.clone();
        let call = tokio::spawn(async move {
            call_pool
                .call_capacity_provider(route(&call_pool, generation))
                .await
        });
        let EdgeServerMessage::CapacityRequest { request_id, .. } =
            receiver.recv().await.unwrap().into_attempted_message()
        else {
            panic!("capacity frame");
        };
        assert!(!pool.complete_capacity_rpc(
            "other",
            "runner",
            generation,
            &request_id,
            generation,
            Ok(SealedCapacityPayload("cmVzdWx0".into()))
        ));
        assert!(!pool.complete_capacity_rpc(
            "owner",
            "runner",
            generation + 1,
            &request_id,
            generation,
            Ok(SealedCapacityPayload("cmVzdWx0".into()))
        ));
        assert!(pool.complete_capacity_rpc(
            "owner",
            "runner",
            generation,
            &request_id,
            generation,
            Ok(SealedCapacityPayload("cmVzdWx0".into()))
        ));
        assert_eq!(call.await.unwrap().unwrap().0, "cmVzdWx0");
        assert!(!pool.complete_capacity_rpc(
            "owner",
            "runner",
            generation,
            &request_id,
            generation,
            Ok(SealedCapacityPayload("cmVzdWx0".into()))
        ));

        let call_pool = pool.clone();
        let call = tokio::spawn(async move {
            call_pool
                .call_capacity_provider(route(&call_pool, generation))
                .await
        });
        receiver.recv().await.unwrap();
        pool.unregister_generation("owner", "runner", generation);
        assert_eq!(
            call.await.unwrap().unwrap_err(),
            CapacityRpcFailure::OutcomeUnknown
        );
        let (next, mut new_receiver) = connect(&pool);
        assert!(next > generation);
        assert!(new_receiver.try_recv().is_err());
        assert!(pool.capacity_rpc.waiters.is_empty());
        assert!(pool.get_pending_requests_for_user("owner").is_empty());
    }

    #[tokio::test]
    async fn canceled_capacity_waiter_is_removed_without_retaining_or_replaying_payload() {
        let pool = EdgeConnectionPool::new();
        let (generation, mut receiver) = connect(&pool);
        let call_pool = pool.clone();
        let call = tokio::spawn(async move {
            call_pool
                .call_capacity_provider(route(&call_pool, generation))
                .await
        });
        receiver.recv().await.unwrap();
        assert_eq!(pool.capacity_rpc.waiters.len(), 1);
        call.abort();
        let _ = call.await;
        assert!(pool.capacity_rpc.waiters.is_empty());
        assert_eq!(
            pool.capacity_rpc.slots.available_permits(),
            MAX_CAPACITY_RPC_INFLIGHT
        );
    }

    #[test]
    fn sealed_wire_budget_and_debug_never_expand_or_print_payload() {
        let payload =
            SealedCapacityPayload("a".repeat(crate::edge_ws_protocol::MAX_SEALED_CAPACITY_BYTES));
        assert!(payload.is_valid());
        let message = EdgeServerMessage::CapacityRequest {
            request_id: "id".into(),
            connection_generation: 1,
            provider_id: "provider".into(),
            payload,
            timeout_secs: 130,
        };
        assert!(
            serde_json::to_vec(&message).unwrap().len()
                < crate::edge_ws_protocol::MAX_EDGE_MESSAGE_BYTES
        );
        assert!(!format!("{message:?}").contains(&"a".repeat(100)));
        assert!(
            !SealedCapacityPayload(
                "a".repeat(crate::edge_ws_protocol::MAX_SEALED_CAPACITY_BYTES + 1)
            )
            .is_valid()
        );
        assert!(!SealedCapacityPayload("\"plaintext\n".into()).is_valid());
    }

    #[tokio::test]
    async fn capacity_lease_cannot_cross_process_restart_even_when_local_generation_repeats() {
        let first = EdgeConnectionPool::new();
        let (first_generation, _first_socket) = connect(&first);
        let request = route(&first, first_generation);
        let restarted = EdgeConnectionPool::new();
        let (next_generation, mut next_socket) = connect(&restarted);
        assert_eq!(first_generation, next_generation);
        assert_eq!(
            restarted.call_capacity_provider(request).await.unwrap_err(),
            CapacityRpcFailure::BindingChanged
        );
        assert!(next_socket.try_recv().is_err());
    }
}
