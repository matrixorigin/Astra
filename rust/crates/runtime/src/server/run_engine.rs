//! Durable run execution engine.
//!
//! `RunEngine` orchestrates agentic run execution with persistence backing via
//! [`RunStateStore`]. It bridges the gap between volatile in-memory run state
//! (used for low-latency queries) and durable storage (for crash recovery).
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────┐     ┌──────────────┐
//! │ RunLifecycleService │────▶│  RunEngine   │
//! │   (HTTP handlers)   │     │              │
//! └─────────────────────┘     │  start_run() │
//!                             │  persist()   │
//!                             │  resume()    │
//!                             │  recover()   │
//!                             └──────┬───────┘
//!                                    │
//!                             ┌──────▼───────┐
//!                             │ RunStateStore │
//!                             │  (durable)   │
//!                             └──────────────┘
//! ```
//!
//! # Lifecycle
//!
//! 1. `start_run()` — Creates a durable record, returns run_id
//! 2. `persist_status()` — Syncs status changes to store
//! 3. `persist_checkpoint()` — Saves checkpoint for crash recovery
//! 4. `persist_usage()` — Updates token/tool counts
//! 5. `recover_active_runs()` — On startup, loads runs that were active when process died
//! 6. `load_run()` — Loads a run from store (cache miss path)

use std::sync::Arc;

use astra_services::runs::{DurableRunRecord, RunStateStore};

use astra_core::STATUS_RUNNING;

/// Durable run execution engine.
///
/// Wraps a [`RunStateStore`] and provides high-level operations for
/// durable run management. The engine is designed to be composed into
/// `AgenticRunLifecycleService` alongside the volatile in-memory cache.
/// Wraps a [`RunStateStore`] with higher-level operations for durable run
/// management: create, status transitions, usage/checkpoint persistence,
/// event logging, and recovery.
#[derive(Clone)]
pub struct RunEngine {
    store: Arc<dyn RunStateStore>,
}

impl RunEngine {
    /// Create a new engine backed by the given store.
    pub fn new(store: Arc<dyn RunStateStore>) -> Self {
        Self { store }
    }

    /// Create a durable run record in the store.
    ///
    /// Called by `create_run()` in the lifecycle service after the in-memory
    /// RunState is inserted. This ensures the run survives process restarts.
    pub async fn start_run(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
    ) -> Result<(), String> {
        self.start_run_ext(run_id, user_id, session_id, None, None, None, None)
            .await
    }

    /// Extended version of `start_run` with delegation metadata.
    pub async fn start_run_ext(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
        parent_run_id: Option<&str>,
        delegation_id: Option<&str>,
        agent_id: Option<&str>,
        retry_of: Option<&str>,
    ) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        let record = DurableRunRecord {
            run_id: run_id.to_string(),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            parent_run_id: parent_run_id.map(ToString::to_string),
            delegation_id: delegation_id.map(ToString::to_string),
            agent_id: agent_id.map(ToString::to_string),
            retry_of: retry_of.map(ToString::to_string),
            status: STATUS_RUNNING.to_string(),
            waiting_for: None,
            checkpoint_json: None,
            error_message: None,
            retry_count: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
            events: vec![serde_json::json!({"event_type": "run_started", "data": {}})],
            created_at: now.clone(),
            updated_at: now,
        };
        self.store.insert_run(record).await
    }

    /// Persist a status change to the durable store.
    pub async fn persist_status(
        &self,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        self.store
            .update_run_status(run_id, status, waiting_for, error_message)
            .await
    }

    /// Persist token/tool usage counters.
    pub async fn persist_usage(
        &self,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String> {
        self.store
            .update_run_usage(run_id, prompt_tokens, completion_tokens, tool_calls)
            .await
    }

    /// Save a checkpoint for crash recovery.
    pub async fn persist_checkpoint(
        &self,
        run_id: &str,
        checkpoint_json: &str,
    ) -> Result<bool, String> {
        self.store.save_checkpoint(run_id, checkpoint_json).await
    }

    /// Append an event to the durable event log.
    pub async fn append_event(&self, run_id: &str, event: serde_json::Value) -> Result<(), String> {
        self.store.append_event(run_id, event).await
    }

    /// Load a run from the durable store (cache miss or recovery path).
    pub async fn load_run(&self, run_id: &str) -> Result<Option<DurableRunRecord>, String> {
        self.store.load_run(run_id).await
    }

    /// Find all runs in WAITING status (for the resume engine to re-evaluate).
    pub async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        self.store.find_waiting_runs().await
    }

    /// Find all sub-runs belonging to a delegation.
    pub async fn find_sub_runs(
        &self,
        delegation_id: &str,
    ) -> Result<Vec<DurableRunRecord>, String> {
        self.store.find_sub_runs(delegation_id).await
    }

    /// Persist the verification-gate retry count for a run.
    pub async fn persist_retry_count(
        &self,
        run_id: &str,
        retry_count: u32,
    ) -> Result<bool, String> {
        self.store.update_retry_count(run_id, retry_count).await
    }

    /// Recover active runs after a crash/restart.
    ///
    /// Loads all runs with status `running` or `waiting` from the store.
    /// These represent runs that were in-flight when the process died and
    /// need to be either resumed or marked as failed.
    pub async fn recover_active_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        // Waiting runs can be resumed; running runs were interrupted
        let waiting = self.store.find_waiting_runs().await?;
        // Running runs at crash time should be marked as needing recovery
        // For now, return waiting runs; running-at-crash will be addressed
        // when we implement the background execution spawner
        Ok(waiting)
    }

    /// List runs for a user (delegates to store).
    pub async fn list_user_runs(
        &self,
        user_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<DurableRunRecord>, i64), String> {
        self.store.list_user_runs(user_id, limit, offset).await
    }

    /// Access the underlying store (for advanced queries).
    pub fn store(&self) -> &Arc<dyn RunStateStore> {
        &self.store
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::runs::InMemoryRunStateStore;

    fn test_engine() -> RunEngine {
        RunEngine::new(Arc::new(InMemoryRunStateStore::new()))
    }

    #[tokio::test]
    async fn start_and_load_run() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        let run = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(run.run_id, "run-1");
        assert_eq!(run.user_id, "user-1");
        assert_eq!(run.session_id, "sess-1");
        assert_eq!(run.status, "running");
        assert_eq!(run.events.len(), 1);
    }

    #[tokio::test]
    async fn start_run_ext_persists_retry_linkage() {
        let engine = test_engine();
        engine
            .start_run_ext(
                "run-retry",
                "user-1",
                "sess-1",
                Some("parent-1"),
                Some("del-1"),
                Some("coder"),
                Some("run-original"),
            )
            .await
            .unwrap();

        let run = engine.load_run("run-retry").await.unwrap().unwrap();
        assert_eq!(run.parent_run_id.as_deref(), Some("parent-1"));
        assert_eq!(run.delegation_id.as_deref(), Some("del-1"));
        assert_eq!(run.agent_id.as_deref(), Some("coder"));
        assert_eq!(run.retry_of.as_deref(), Some("run-original"));
    }

    #[tokio::test]
    async fn load_nonexistent_returns_none() {
        let engine = test_engine();
        assert!(engine.load_run("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn persist_status_updates() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        let ok = engine
            .persist_status("run-1", "paused", Some("user_resume"), None)
            .await
            .unwrap();
        assert!(ok);
        let run = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(run.status, "paused");
        assert_eq!(run.waiting_for.as_deref(), Some("user_resume"));
    }

    #[tokio::test]
    async fn persist_status_nonexistent_returns_false() {
        let engine = test_engine();
        let ok = engine
            .persist_status("nope", "failed", None, Some("crash"))
            .await
            .unwrap();
        assert!(!ok);
    }

    #[tokio::test]
    async fn persist_usage_updates() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine.persist_usage("run-1", 1000, 500, 7).await.unwrap();
        let run = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(run.total_prompt_tokens, 1000);
        assert_eq!(run.total_completion_tokens, 500);
        assert_eq!(run.total_tool_calls, 7);
    }

    #[tokio::test]
    async fn persist_checkpoint_saves() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        let ck = r#"{"messages":[],"turn":3}"#;
        engine.persist_checkpoint("run-1", ck).await.unwrap();
        let run = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(run.checkpoint_json.as_deref(), Some(ck));
    }

    #[tokio::test]
    async fn append_event_accumulates() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine
            .append_event(
                "run-1",
                serde_json::json!({"event_type": "tool_call_start"}),
            )
            .await
            .unwrap();
        engine
            .append_event("run-1", serde_json::json!({"event_type": "tool_result"}))
            .await
            .unwrap();
        let run = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(run.events.len(), 3); // run_started + 2 appended
    }

    #[tokio::test]
    async fn find_waiting_runs_filters_correctly() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine.start_run("run-2", "user-1", "sess-2").await.unwrap();
        engine
            .persist_status("run-2", "waiting", Some("tool_approval"), None)
            .await
            .unwrap();
        let waiting = engine.find_waiting_runs().await.unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].run_id, "run-2");
    }

    #[tokio::test]
    async fn list_user_runs_pagination() {
        let engine = test_engine();
        for i in 0..5 {
            engine
                .start_run(&format!("run-{i}"), "user-1", &format!("sess-{i}"))
                .await
                .unwrap();
        }
        engine
            .start_run("run-other", "user-2", "sess-other")
            .await
            .unwrap();
        let (runs, total) = engine.list_user_runs("user-1", 2, 0).await.unwrap();
        assert_eq!(total, 5);
        assert_eq!(runs.len(), 2);
        let (runs2, _) = engine.list_user_runs("user-1", 10, 3).await.unwrap();
        assert_eq!(runs2.len(), 2);
    }

    #[tokio::test]
    async fn recover_active_runs_returns_waiting() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine.start_run("run-2", "user-1", "sess-2").await.unwrap();
        engine
            .persist_status("run-1", "waiting", Some("user_resume"), None)
            .await
            .unwrap();
        engine
            .persist_status("run-2", "completed", None, None)
            .await
            .unwrap();
        let active = engine.recover_active_runs().await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].run_id, "run-1");
    }

    #[tokio::test]
    async fn full_lifecycle_start_pause_resume_complete() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

        // Simulate pause
        engine
            .persist_status("run-1", "paused", Some("user_resume"), None)
            .await
            .unwrap();
        engine
            .append_event("run-1", serde_json::json!({"event_type": "run_paused"}))
            .await
            .unwrap();

        // Simulate resume
        engine
            .persist_status("run-1", "running", None, None)
            .await
            .unwrap();
        engine
            .append_event("run-1", serde_json::json!({"event_type": "run_resumed"}))
            .await
            .unwrap();

        // Simulate completion
        engine.persist_usage("run-1", 2000, 800, 12).await.unwrap();
        engine
            .persist_checkpoint("run-1", r#"{"final": true}"#)
            .await
            .unwrap();
        engine
            .persist_status("run-1", "completed", None, None)
            .await
            .unwrap();
        engine
            .append_event(
                "run-1",
                serde_json::json!({"event_type": "run_finished", "data": {}}),
            )
            .await
            .unwrap();

        let run = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(run.status, "completed");
        assert_eq!(run.total_prompt_tokens, 2000);
        assert_eq!(run.total_completion_tokens, 800);
        assert_eq!(run.total_tool_calls, 12);
        assert_eq!(run.checkpoint_json.as_deref(), Some(r#"{"final": true}"#));
        // run_started + run_paused + run_resumed + run_finished = 4
        assert_eq!(run.events.len(), 4);
        assert!(run.waiting_for.is_none());
    }

    #[tokio::test]
    async fn error_message_persists() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine
            .persist_status("run-1", "failed", None, Some("OOM killed"))
            .await
            .unwrap();
        let run = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(run.status, "failed");
        assert_eq!(run.error_message.as_deref(), Some("OOM killed"));
    }
}
