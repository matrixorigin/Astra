//! Durable adapters around the existing extraction and governance algorithms.
//! The journal checkpoints provider work; it does not introduce a second
//! memory schema, authority resolver, extractor or governance sequence.

use crate::turn::cloud::memoria_compact::{
    HttpMemoriaPort, encode_episode_memory, encode_scene_memory,
};
use astra_memoria::{MemoriaMemory, MemoriaOperationError, MemoriaPort, ReflectSummary};
use astra_services::session_memory_lifecycle::{
    SessionMemoryLifecycleStore, SessionMemoryWork, SessionMemoryWorkClaim,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct FinalizationPlan {
    pub fingerprint: Option<String>,
    pub owner: String,
    pub facts: astra_turn_types::session_facts::SessionFacts,
    pub memories: Vec<MemoriaMemory>,
}

#[derive(Clone)]
pub(crate) struct DurableMemoriaPort {
    pub inner: Arc<dyn MemoriaPort>,
    pub store: SessionMemoryLifecycleStore,
    pub claim: SessionMemoryWorkClaim,
}

impl DurableMemoriaPort {
    async fn transport(&self, write: bool) -> Result<(HttpMemoriaPort, String), String> {
        let transport = self
            .inner
            .resolve_tool_transport(write)
            .await?
            .ok_or("durable memory work requires an owner-bound HTTP provider")?;
        let owner = transport.owner_user_id.clone();
        Ok((
            HttpMemoriaPort::from_transport(transport).for_durable_session(),
            owner,
        ))
    }

    pub async fn prepare_plan(&self) -> Result<FinalizationPlan, String> {
        // The recovery owner gates writes separately. Both a new inventory and
        // a retained plan contain memory content and need fresh read authority.
        let (client, owner) = self.transport(false).await?;
        let checked_owner = owner.clone();
        let previous = self
            .store
            .inspect_fenced_state(
                &self.claim.user_id,
                &self.claim.session_id,
                &self.claim.token,
                true,
                move |work| {
                    Box::pin(async move {
                        ensure_settled(work)?;
                        if let Some(plan) = work.plan.clone() {
                            let plan: FinalizationPlan =
                                serde_json::from_value(plan).map_err(|error| error.to_string())?;
                            if plan.owner != checked_owner {
                                return Err(
                                    "session memory authority changed during finalization".into()
                                );
                            }
                            return Ok((Some(plan), work.completed_fingerprint.clone()));
                        }
                        Ok((None, work.completed_fingerprint.clone()))
                    })
                },
            )
            .await?;
        if let (Some(plan), _) = previous {
            return Ok(plan);
        }
        let generation = previous.1;
        // Optional backend reads never hold the Session admission fence.
        let memories = tokio::time::timeout(
            Duration::from_secs(10),
            client.retrieve_scoped_inventory(&self.claim.session_id, 4096, &["working"]),
        )
        .await
        .map_err(|_| "finalization snapshot deadline exceeded")?
        .map_err(|error| error.to_string())?;
        self.store.with_fenced_state(&self.claim.user_id,&self.claim.session_id,Some(&self.claim.token),true,move |work| Box::pin(async move {
            ensure_settled(work)?;
            if work.completed_fingerprint!=generation {return Err("finalization snapshot generation changed during retrieval".into());}
            if work.working_memory_id.as_ref().is_some_and(|id|!memories.iter().any(|memory|&memory.memory_id==id)) {
                return Err("final working receipt is not reconciled; retain working memory".into());
            }
            if work.latest_turn.is_none()&&!memories.is_empty() {return Err("final extraction freshness is unknown; retain working memory until a canonical turn settles".into());}
            let facts=work.latest_facts.as_ref().map(|facts|serde_json::from_value(facts.clone())).transpose().map_err(|error|error.to_string())?.unwrap_or_default();
            let plan=FinalizationPlan{fingerprint:work.completed_fingerprint.clone(),owner,facts,memories};
            work.plan=Some(serde_json::to_value(&plan).map_err(|error|error.to_string())?);
            Ok(plan)
        })).await
    }

    async fn write(
        &self,
        stage: &str,
        content: &str,
        memory_type: &str,
        tier: &str,
    ) -> Result<String, String> {
        let (client, owner) = self.transport(true).await?;
        let stage = stage.to_owned();
        let content = content.to_owned();
        let memory_type = memory_type.to_owned();
        let tier = tier.to_owned();
        let session = self.claim.session_id.clone();
        let expected_active = self.claim.work.active_fingerprint.clone();
        let key_session = session.clone();
        let (prepared, cached) = self.store.with_fenced_state(&self.claim.user_id, &self.claim.session_id, Some(&self.claim.token), stage != "working", move |work| Box::pin(async move {
            let generation = if stage == "working" {
                if work.active_request.is_none() || work.active_fingerprint != expected_active { return Err("extraction generation was superseded".into()); }
                work.active_fingerprint.clone()
            } else {
                ensure_current_plan(work, &owner)?;
                work.completed_fingerprint.clone()
            };
            let identity = json!([key_session, generation, stage, content]);
            let key = format!("astra.memory.v1:{:x}", Sha256::digest(serde_json::to_vec(&identity).map_err(|error| error.to_string())?));
            let prepared = json!({"owner":owner, "key":key, "content":content, "memory_type":memory_type, "tier":tier});
            if stage == "working" {
                if let Some(existing) = work.prepared_working.as_ref() {
                    if existing != &prepared { return Err("uncertain working write must replay its frozen payload".into()); }
                } else { work.prepared_working = Some(prepared.clone()); }
            }
            let cached = work.receipts.get(&key).cloned();
            Ok((prepared, cached))
        })).await?;
        let prepared_owner = prepared["owner"]
            .as_str()
            .ok_or("missing prepared owner")?
            .to_string();
        let key = prepared["key"]
            .as_str()
            .ok_or("missing prepared key")?
            .to_string();
        let is_working = prepared["memory_type"] == "working";
        if let Some(id) = cached {
            return Ok(id);
        }
        let id = tokio::time::timeout(
            Duration::from_secs(3),
            client.store_idempotent(
                prepared["content"]
                    .as_str()
                    .ok_or("missing prepared content")?,
                prepared["memory_type"]
                    .as_str()
                    .ok_or("missing prepared memory type")?,
                &self.claim.session_id,
                prepared["tier"].as_str().ok_or("missing prepared tier")?,
                &key,
            ),
        )
        .await
        .map_err(|_| "durable memory write deadline exceeded")??;
        self.store
            .with_fenced_state(
                &self.claim.user_id,
                &self.claim.session_id,
                Some(&self.claim.token),
                false,
                move |work| {
                    Box::pin(async move {
                        if !is_working {
                            ensure_current_plan(work, &prepared_owner)?;
                        } else if work.prepared_working.as_ref() != Some(&prepared) {
                            return Err("working write checkpoint was superseded".into());
                        }
                        work.receipts.insert(key, id.clone());
                        if is_working {
                            work.working_memory_id = Some(id.clone());
                        }
                        Ok(id)
                    })
                },
            )
            .await
    }

    pub async fn replay_prepared_working(&self, prepared: &Value) -> Result<(), String> {
        self.write(
            "working",
            prepared["content"]
                .as_str()
                .ok_or("missing replay content")?,
            "working",
            "T3",
        )
        .await?;
        Ok(())
    }

    pub async fn finish_governance(
        &self,
        report: &crate::turn::cloud::session_end_governance::SessionEndReport,
    ) -> Result<(), String> {
        let optional_complete = report.consolidation_complete()
            && !report.reflection_failed
            && report.scene_persist_failures == 0;
        let report = serde_json::to_value(report).map_err(|error| error.to_string())?;
        self.store
            .finish_claim(&self.claim, true, optional_complete, move |work| {
                Box::pin(async move {
                    ensure_settled(work)?;
                    work.optional_complete = optional_complete && work.consolidation_complete;
                    work.last_report = Some(report);
                    Ok(())
                })
            })
            .await
    }
}

fn ensure_settled(work: &SessionMemoryWork) -> Result<(), String> {
    if work.extraction_pending() || work.completed_turn.unwrap_or(0) < work.canonical_turn {
        return Err("final extraction generation has not settled".into());
    }
    if !work.finalize_requested {
        return Err("session memory finalization was superseded by a follow-up turn".into());
    }
    Ok(())
}

fn ensure_current_plan(work: &SessionMemoryWork, owner: &str) -> Result<FinalizationPlan, String> {
    ensure_settled(work)?;
    let plan: FinalizationPlan =
        serde_json::from_value(work.plan.clone().ok_or("missing finalization plan")?)
            .map_err(|error| error.to_string())?;
    if plan.fingerprint != work.latest_fingerprint || plan.owner != owner {
        return Err("session memory finalization scope was superseded".into());
    }
    Ok(plan)
}

#[async_trait::async_trait]
impl MemoriaPort for DurableMemoriaPort {
    async fn admits_operation(&self, write: bool) -> Result<bool, String> {
        if self.inner.admits_operation(write).await? {
            Ok(true)
        } else {
            Err("durable memory maintenance awaits current consent".into())
        }
    }

    async fn retrieve_ext(
        &self,
        query: &str,
        session_id: Option<&str>,
        top_k: usize,
        strict: bool,
    ) -> Result<Vec<MemoriaMemory>, MemoriaOperationError> {
        let (client, owner) = self.transport(false).await?;
        if let Some(plan) = self.claim.work.plan.clone() {
            let plan: FinalizationPlan =
                serde_json::from_value(plan).map_err(|error| error.to_string())?;
            if plan.owner != owner || session_id != Some(self.claim.session_id.as_str()) || !strict
            {
                return Err("memory_scope_violation: invalid frozen finalization scope".into());
            }
            let mut memories = plan.memories;
            memories.sort_by_key(|memory| {
                std::cmp::Reverse(
                    super::runner::decode_session_memory_snapshot(
                        &memory.content,
                        &self.claim.session_id,
                    )
                    .map(|snapshot| snapshot.updated_turn)
                    .unwrap_or(0),
                )
            });
            return Ok(memories.into_iter().take(top_k).collect());
        }
        client.retrieve_ext(query, session_id, top_k, strict).await
    }

    async fn retrieve_scoped_typed(
        &self,
        query: &str,
        session_id: &str,
        top_k: usize,
        memory_types: &[&str],
    ) -> Result<Vec<MemoriaMemory>, MemoriaOperationError> {
        if self.claim.work.plan.is_some() {
            let mut memories = self
                .retrieve_ext(query, Some(session_id), top_k, true)
                .await?;
            memories.retain(|memory| memory_types.contains(&memory.memory_type.as_str()));
            return Ok(memories);
        }
        let memories = self
            .transport(false)
            .await?
            .0
            .retrieve_scoped_typed(query, session_id, top_k, memory_types)
            .await?;
        let snapshot = memories
            .iter()
            .filter_map(|memory| {
                super::runner::decode_session_memory_snapshot(&memory.content, session_id)
                    .map(|snapshot| (snapshot.updated_turn, memory.memory_id.clone()))
            })
            .max_by_key(|(turn, _)| *turn)
            .map(|(_, id)| id);
        self.store
            .with_fenced_state(
                &self.claim.user_id,
                &self.claim.session_id,
                Some(&self.claim.token),
                false,
                move |work| {
                    Box::pin(async move {
                        // Reads before this generation writes may establish the
                        // observed snapshot identity. Once its write is prepared,
                        // the frozen payload/confirmed receipt owns that identity:
                        // best-effort cleanup can read a page of only stale rows.
                        if work.prepared_working.is_none() {
                            work.working_memory_id = snapshot;
                        }
                        Ok(())
                    })
                },
            )
            .await?;
        Ok(memories)
    }

    async fn store(
        &self,
        content: &str,
        memory_type: &str,
        session_id: Option<&str>,
        tier: Option<&str>,
    ) -> Result<String, String> {
        if memory_type != "working"
            || session_id != Some(self.claim.session_id.as_str())
            || tier != Some("T3")
        {
            return Err("durable extraction requires its exact working-memory scope".into());
        }
        self.write("working", content, memory_type, "T3").await
    }

    async fn store_episode(&self, session_id: &str, overview: &str) -> Result<String, String> {
        if session_id != self.claim.session_id {
            return Err("episode session scope mismatch".into());
        }
        self.write(
            "episode",
            &encode_episode_memory(session_id, overview),
            "episodic",
            "T3",
        )
        .await
    }

    async fn store_scene(
        &self,
        session_id: &str,
        signal: &str,
        summary: &str,
    ) -> Result<String, String> {
        if session_id != self.claim.session_id {
            return Err("scene session scope mismatch".into());
        }
        self.write(
            "scene",
            &encode_scene_memory(signal, summary),
            "semantic",
            "T4",
        )
        .await
    }

    async fn purge_working(&self, session_id: &str) -> Result<u64, String> {
        if session_id != self.claim.session_id {
            return Err("purge session scope mismatch".into());
        }
        let plan: FinalizationPlan =
            serde_json::from_value(self.claim.work.plan.clone().ok_or("missing purge plan")?)
                .map_err(|error| error.to_string())?;
        let mut count = 0;
        for memory in &plan.memories {
            let (client, owner) = self.transport(true).await?;
            let id = memory.memory_id.clone();
            let purged = self
                .store
                .with_fenced_state(
                    &self.claim.user_id,
                    session_id,
                    Some(&self.claim.token),
                    true,
                    move |work| {
                        Box::pin(async move {
                            ensure_current_plan(work, &owner)?;
                            if work.purged_ids.contains(&id) {
                                return Ok(false);
                            }
                            tokio::time::timeout(
                                Duration::from_secs(3),
                                client.delete_confirmed_or_absent(&id),
                            )
                            .await
                            .map_err(|_| "durable working purge deadline exceeded")??;
                            work.purged_ids.insert(id);
                            Ok(true)
                        })
                    },
                )
                .await?;
            count += u64::from(purged);
        }
        let owner = plan.owner;
        self.store
            .with_fenced_state(
                &self.claim.user_id,
                session_id,
                Some(&self.claim.token),
                true,
                move |work| {
                    Box::pin(async move {
                        ensure_current_plan(work, &owner)?;
                        work.consolidation_complete = true;
                        Ok(())
                    })
                },
            )
            .await?;
        Ok(count)
    }

    async fn delete(&self, memory_id: &str) -> Result<(), String> {
        let (client, _) = self.transport(true).await?;
        let id = memory_id.to_string();
        self.store
            .inspect_fenced_state(
                &self.claim.user_id,
                &self.claim.session_id,
                &self.claim.token,
                false,
                move |_work| {
                    Box::pin(async move {
                        tokio::time::timeout(
                            Duration::from_secs(1),
                            client.delete_confirmed_or_absent(&id),
                        )
                        .await
                        .map_err(|_| "stale snapshot cleanup deadline exceeded")?
                    })
                },
            )
            .await
    }

    async fn reflect_session(
        &self,
        session_id: &str,
        force: bool,
    ) -> Result<ReflectSummary, String> {
        if session_id != self.claim.session_id {
            return Err("reflection session scope mismatch".into());
        }
        // Reflection returns candidate content as well as creating memories.
        let transport = self
            .inner
            .resolve_read_write_transport()
            .await?
            .ok_or("reflection requires a read/write memory transport")?;
        let owner = transport.owner_user_id.clone();
        let client = HttpMemoriaPort::from_transport(transport).for_durable_session();
        let checked_owner = owner.clone();
        let cached = self
            .store
            .inspect_fenced_state(
                &self.claim.user_id,
                session_id,
                &self.claim.token,
                false,
                move |work| {
                    Box::pin(async move {
                        ensure_current_plan(work, &checked_owner)?;
                        Ok(work.reflection.clone())
                    })
                },
            )
            .await?;
        if let Some(reflection) = cached {
            return serde_json::from_value(reflection).map_err(|error| error.to_string());
        }
        let reflection = tokio::time::timeout(
            Duration::from_secs(10),
            client.reflect_session(session_id, force),
        )
        .await
        .map_err(|_| "durable reflection deadline exceeded")??;
        let result = reflection.clone();
        self.store
            .with_fenced_state(
                &self.claim.user_id,
                session_id,
                Some(&self.claim.token),
                false,
                move |work| {
                    Box::pin(async move {
                        ensure_current_plan(work, &owner)?;
                        work.reflection = Some(
                            serde_json::to_value(&reflection).map_err(|error| error.to_string())?,
                        );
                        Ok(())
                    })
                },
            )
            .await?;
        Ok(result)
    }
}

#[cfg(test)]
#[path = "durable_tests.rs"]
mod tests;
