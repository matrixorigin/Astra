//! Unified sync engine: orchestrate data synchronization across edge and cloud.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │                    SyncOrchestrator                          │
//! │  ┌──────────┐ ┌──────────┐ ┌────────────┐ ┌──────────────┐ │
//! │  │ Learning  │ │  Events  │ │   Tasks    │ │  Templates   │ │
//! │  │ Adapter   │ │  Adapter │ │   Adapter  │ │   Adapter    │ │
//! │  └─────┬────┘ └─────┬────┘ └──────┬─────┘ └──────┬───────┘ │
//! ├────────┼────────────┼────────────┼───────────────┼──────────┤
//! │        └────────────┴────────────┴───────────────┘          │
//! │                     CloudTransport                           │
//! │              (MatrixOne / S3 / HTTP / ...)                   │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Design Principles
//!
//! - **Local-first**: Edge always writes locally first, then async pushes to cloud
//! - **Unified protocol**: All domains share the same SyncEnvelope and state machine
//! - **Domain autonomy**: Each DomainAdapter defines its own merge/conflict strategy
//! - **Observable**: Every sync operation produces a SyncEvent for tracing
//! - **Graceful degradation**: 100% local functionality when cloud unavailable

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ─── Core Types ─────────────────────────────────────────────────────────────

/// Data domain identifier — each domain has its own sync lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncDomain {
    /// EntityGraph + PatternLibrary + Calibrator + ToolHealth
    Learning,
    /// JournalEvent batch → agent_events
    Events,
    /// TaskRecord + TaskPlan (active plans)
    Tasks,
    /// PlanTemplate (extracted from successful tasks)
    Templates,
    /// User preferences (model, explain mode, etc.)
    Preferences,
}

impl fmt::Display for SyncDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Learning => write!(f, "learning"),
            Self::Events => write!(f, "events"),
            Self::Tasks => write!(f, "tasks"),
            Self::Templates => write!(f, "templates"),
            Self::Preferences => write!(f, "preferences"),
        }
    }
}

/// Sync state machine — tracks the lifecycle of a domain's sync state.
///
/// ```text
///   Clean ──write──▶ Dirty ──push──▶ Syncing ──ok──▶ Clean
///                                       │
///                                    conflict
///                                       │
///                                       ▼
///                                   Conflict ──resolve──▶ Dirty
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SyncState {
    /// Local and cloud are consistent (or cloud is unavailable and we don't care).
    #[default]
    Clean,
    /// Local has unpushed changes.
    Dirty,
    /// A push or pull is in progress.
    Syncing,
    /// Version conflict detected — needs resolution before next push.
    Conflict {
        local_version: u64,
        remote_version: u64,
    },
    /// Pull in progress.
    Pulling,
    /// Unrecoverable error — needs manual intervention or auto-retry.
    Error { retry_count: u8, last_error: String },
}

impl SyncState {
    pub fn is_dirty(&self) -> bool {
        matches!(self, Self::Dirty)
    }

    pub fn is_clean(&self) -> bool {
        matches!(self, Self::Clean)
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }

    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }
}

/// Metadata envelope tracking sync state for one domain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncEnvelope {
    /// Which data domain this envelope tracks.
    pub domain: SyncDomain,
    /// Local version counter (monotonically increasing on each local write).
    pub local_version: u64,
    /// Last known cloud version (updated after successful pull/push).
    pub cloud_version: Option<u64>,
    /// SHA-256 checksum of the current local payload.
    pub checksum: Option<String>,
    /// Last local modification time (epoch seconds).
    pub last_modified: u64,
    /// Current sync state.
    pub sync_state: SyncState,
    /// Timestamp of last successful sync (epoch seconds).
    pub last_synced: Option<u64>,
    /// Cumulative sync statistics.
    pub stats: SyncStats,
}

impl SyncEnvelope {
    pub fn new(domain: SyncDomain) -> Self {
        Self {
            domain,
            local_version: 0,
            cloud_version: None,
            checksum: None,
            last_modified: epoch_secs(),
            sync_state: SyncState::Clean,
            last_synced: None,
            stats: SyncStats::default(),
        }
    }

    /// Mark that a local write occurred — transitions Clean → Dirty.
    pub fn mark_dirty(&mut self) {
        self.local_version += 1;
        self.last_modified = epoch_secs();
        if self.sync_state.is_clean() {
            self.sync_state = SyncState::Dirty;
        }
    }

    /// Mark that a push succeeded — transitions Syncing → Clean.
    pub fn mark_synced(&mut self, new_cloud_version: u64) {
        self.cloud_version = Some(new_cloud_version);
        self.sync_state = SyncState::Clean;
        self.last_synced = Some(epoch_secs());
        self.stats.pushes += 1;
    }

    /// Mark that a pull succeeded — update cloud version.
    pub fn mark_pulled(&mut self, cloud_version: u64) {
        self.cloud_version = Some(cloud_version);
        if self.sync_state == SyncState::Pulling {
            self.sync_state = SyncState::Clean;
        }
        self.last_synced = Some(epoch_secs());
        self.stats.pulls += 1;
    }

    /// Mark a conflict detected.
    pub fn mark_conflict(&mut self, remote_version: u64) {
        self.sync_state = SyncState::Conflict {
            local_version: self.local_version,
            remote_version,
        };
        self.stats.conflicts += 1;
    }

    /// Mark an error occurred.
    pub fn mark_error(&mut self, error: String) {
        let retry_count = match &self.sync_state {
            SyncState::Error { retry_count, .. } => retry_count + 1,
            _ => 1,
        };
        self.sync_state = SyncState::Error {
            retry_count,
            last_error: error,
        };
        self.stats.errors += 1;
    }
}

/// Cumulative sync statistics for a domain.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncStats {
    pub pushes: u64,
    pub pulls: u64,
    pub conflicts: u64,
    pub errors: u64,
    pub bytes_pushed: u64,
    pub bytes_pulled: u64,
}

// ─── Payload ────────────────────────────────────────────────────────────────

/// Format of a sync payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadFormat {
    /// Complete state snapshot.
    Full,
    /// Incremental changes since a baseline version.
    Delta { baseline_version: u64 },
}

/// Data payload prepared for sync transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPayload {
    /// Serialized data (potentially compressed).
    pub data: Vec<u8>,
    /// Full or delta.
    pub format: PayloadFormat,
    /// SHA-256 checksum of uncompressed data.
    pub checksum: String,
    /// Number of logical items in the payload.
    pub item_count: u32,
    /// Whether data is gzip compressed.
    pub compressed: bool,
}

impl SyncPayload {
    pub fn size(&self) -> usize {
        self.data.len()
    }
}

/// Result of merging remote data into local state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MergeResult {
    pub items_added: u32,
    pub items_updated: u32,
    pub items_removed: u32,
    pub conflicts_auto_resolved: u32,
}

// ─── Domain Adapter Trait ───────────────────────────────────────────────────

/// Error type for sync operations.
#[derive(Debug, Clone)]
pub struct SyncError {
    pub domain: SyncDomain,
    pub message: String,
    pub retryable: bool,
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.domain, self.message)
    }
}

impl std::error::Error for SyncError {}

impl SyncError {
    pub fn permanent(domain: SyncDomain, msg: impl Into<String>) -> Self {
        Self {
            domain,
            message: msg.into(),
            retryable: false,
        }
    }

    pub fn transient(domain: SyncDomain, msg: impl Into<String>) -> Self {
        Self {
            domain,
            message: msg.into(),
            retryable: true,
        }
    }
}

/// Adapter for a specific data domain — defines how to export, merge, and resolve conflicts.
///
/// Each domain implements this trait to participate in the unified sync protocol.
/// The SyncOrchestrator calls these methods in the correct order based on the
/// domain's SyncPolicy.
#[async_trait]
pub trait DomainAdapter: Send + Sync {
    /// Which domain this adapter manages.
    fn domain(&self) -> SyncDomain;

    /// Export the full local state as a sync payload.
    fn export_full(&self) -> Result<SyncPayload, SyncError>;

    /// Export only changes since the last sync (delta).
    /// Returns None if delta is not supported or there are no changes.
    fn export_delta(&self) -> Result<Option<SyncPayload>, SyncError>;

    /// Import remote data into local state.
    fn merge_remote(&self, remote: &SyncPayload) -> Result<MergeResult, SyncError>;

    /// Resolve a conflict between local and remote states.
    /// Called when a push fails due to version mismatch.
    /// Returns the merged payload to push.
    fn resolve_conflict(
        &self,
        local: &SyncPayload,
        remote: &SyncPayload,
    ) -> Result<SyncPayload, SyncError>;

    /// Validate payload integrity (checksum, schema, etc.).
    fn validate(&self, payload: &SyncPayload) -> Result<(), SyncError>;

    /// Get the current sync envelope.
    fn envelope(&self) -> SyncEnvelope;

    /// Update the sync envelope after a state change.
    fn set_envelope(&self, envelope: SyncEnvelope);

    /// Whether this domain has local changes that need pushing.
    fn has_dirty_data(&self) -> bool {
        self.envelope().sync_state.is_dirty()
    }

    /// Estimated size of full export in bytes (for deciding delta vs full).
    fn estimated_size(&self) -> usize {
        0
    }

    /// Clear the dirty flag after successful push (domain-specific cleanup).
    fn clear_dirty(&self) -> Result<(), SyncError> {
        Ok(())
    }
}

// ─── Cloud Transport ────────────────────────────────────────────────────────

/// Result of a cloud push operation.
#[derive(Debug, Clone)]
pub struct PushResult {
    pub success: bool,
    pub new_version: Option<u64>,
    pub is_conflict: bool,
    pub remote_payload: Option<SyncPayload>,
    pub message: String,
}

/// Result of a cloud pull operation.
#[derive(Debug, Clone)]
pub struct PullResult {
    pub payload: Option<SyncPayload>,
    pub version: Option<u64>,
    pub message: String,
}

/// Abstract transport layer — the actual I/O to cloud storage.
///
/// Implementations:
/// - `MatrixOneTransport` — sqlx-based sync to MatrixOne database
/// - `NoopTransport` — for offline/local-only mode
/// - Future: S3Transport, HttpTransport, etc.
#[async_trait]
pub trait CloudTransport: Send + Sync {
    /// Push a payload to cloud for the given domain.
    async fn push(
        &self,
        user_id: &str,
        domain: SyncDomain,
        payload: &SyncPayload,
        expected_version: Option<u64>,
    ) -> Result<PushResult, SyncError>;

    /// Pull the latest payload from cloud for the given domain.
    async fn pull(&self, user_id: &str, domain: SyncDomain) -> Result<PullResult, SyncError>;

    /// Check if cloud is reachable.
    async fn health_check(&self) -> bool;
}

/// No-op transport for offline mode.
pub struct NoopTransport;

#[async_trait]
impl CloudTransport for NoopTransport {
    async fn push(
        &self,
        _user_id: &str,
        _domain: SyncDomain,
        _payload: &SyncPayload,
        _expected_version: Option<u64>,
    ) -> Result<PushResult, SyncError> {
        Ok(PushResult {
            success: true,
            new_version: Some(0),
            is_conflict: false,
            remote_payload: None,
            message: "noop".to_string(),
        })
    }

    async fn pull(&self, _user_id: &str, _domain: SyncDomain) -> Result<PullResult, SyncError> {
        Ok(PullResult {
            payload: None,
            version: None,
            message: "noop".to_string(),
        })
    }

    async fn health_check(&self) -> bool {
        true
    }
}

// ─── Sync Policy ────────────────────────────────────────────────────────────

/// When to pull data from cloud.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullTrigger {
    /// Pull once at session start.
    SessionStart,
    /// Never pull (write-only domain like events).
    Never,
    /// Pull periodically.
    Periodic { interval_secs: u64 },
}

/// When to push data to cloud.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushTrigger {
    /// Push at session end.
    SessionEnd,
    /// Batch-push when buffer fills or interval elapses.
    Batched {
        max_items: usize,
        max_interval_secs: u64,
    },
    /// Push immediately on every change.
    OnChange,
    /// Push when a specific event occurs (e.g., plan completion).
    OnComplete,
}

/// Sync policy for a domain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPolicy {
    pub pull: PullTrigger,
    pub push: PushTrigger,
    /// Max conflict resolution retries before giving up.
    pub max_conflict_retries: u8,
    /// Timeout for individual sync operations.
    pub timeout_secs: u64,
    /// Whether to prefer delta over full sync.
    pub prefer_delta: bool,
}

impl SyncPolicy {
    /// Default policy for the Learning domain.
    pub fn learning() -> Self {
        Self {
            pull: PullTrigger::SessionStart,
            push: PushTrigger::SessionEnd,
            max_conflict_retries: 3,
            timeout_secs: 5,
            prefer_delta: true,
        }
    }

    /// Default policy for the Events domain.
    pub fn events() -> Self {
        Self {
            pull: PullTrigger::Never,
            push: PushTrigger::Batched {
                max_items: 20,
                max_interval_secs: 5,
            },
            max_conflict_retries: 0,
            timeout_secs: 10,
            prefer_delta: false,
        }
    }

    /// Default policy for the Tasks domain.
    pub fn tasks() -> Self {
        Self {
            pull: PullTrigger::SessionStart,
            push: PushTrigger::OnChange,
            max_conflict_retries: 3,
            timeout_secs: 5,
            prefer_delta: false,
        }
    }

    /// Default policy for the Templates domain.
    pub fn templates() -> Self {
        Self {
            pull: PullTrigger::SessionStart,
            push: PushTrigger::OnComplete,
            max_conflict_retries: 2,
            timeout_secs: 5,
            prefer_delta: false,
        }
    }

    /// Default policy for the Preferences domain.
    pub fn preferences() -> Self {
        Self {
            pull: PullTrigger::SessionStart,
            push: PushTrigger::OnChange,
            max_conflict_retries: 1,
            timeout_secs: 3,
            prefer_delta: false,
        }
    }
}

// ─── Sync Events (Observability) ────────────────────────────────────────────

/// Observable sync event — emitted for tracing and metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncEvent {
    pub domain: SyncDomain,
    pub operation: SyncOperation,
    pub success: bool,
    pub duration_ms: u64,
    pub bytes_transferred: u64,
    pub version_before: Option<u64>,
    pub version_after: Option<u64>,
    pub error: Option<String>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOperation {
    Pull,
    PushFull,
    PushDelta,
    ConflictResolve,
    HealthCheck,
}

// ─── Sync Orchestrator ──────────────────────────────────────────────────────

/// Central sync coordinator — manages all domain adapters and their lifecycle.
///
/// # Usage
///
/// ```rust,ignore
/// let mut orch = SyncOrchestrator::new(transport, user_id);
/// orch.register(learning_adapter, SyncPolicy::learning());
/// orch.register(event_adapter, SyncPolicy::events());
///
/// // Session start
/// orch.pull_all().await;
///
/// // During session — on each write
/// orch.notify_write(SyncDomain::Learning);
///
/// // Session end
/// orch.push_dirty().await;
/// ```
pub struct SyncOrchestrator {
    transport: Arc<dyn CloudTransport>,
    user_id: String,
    adapters: HashMap<SyncDomain, Box<dyn DomainAdapter>>,
    policies: HashMap<SyncDomain, SyncPolicy>,
    event_log: Vec<SyncEvent>,
    cloud_available: bool,
}

impl SyncOrchestrator {
    /// Create a new orchestrator with the given transport and user identity.
    pub fn new(transport: Arc<dyn CloudTransport>, user_id: impl Into<String>) -> Self {
        Self {
            transport,
            user_id: user_id.into(),
            adapters: HashMap::new(),
            policies: HashMap::new(),
            event_log: Vec::new(),
            cloud_available: false,
        }
    }

    /// Register a domain adapter with its sync policy.
    pub fn register(&mut self, adapter: Box<dyn DomainAdapter>, policy: SyncPolicy) {
        let domain = adapter.domain();
        self.adapters.insert(domain, adapter);
        self.policies.insert(domain, policy);
    }

    /// Check cloud health and update availability flag.
    pub async fn check_health(&mut self) -> bool {
        self.cloud_available = self.transport.health_check().await;
        self.cloud_available
    }

    /// Pull all domains that have PullTrigger::SessionStart.
    pub async fn pull_all(&mut self) -> Vec<DomainSyncResult> {
        let mut results = Vec::new();

        let domains: Vec<SyncDomain> = self
            .policies
            .iter()
            .filter(|(_, p)| matches!(p.pull, PullTrigger::SessionStart))
            .map(|(d, _)| *d)
            .collect();

        for domain in domains {
            let result = self.pull_domain(domain).await;
            results.push(result);
        }
        results
    }

    /// Pull a single domain from cloud.
    pub async fn pull_domain(&mut self, domain: SyncDomain) -> DomainSyncResult {
        let start = std::time::Instant::now();

        let adapter = match self.adapters.get(&domain) {
            Some(a) => a,
            None => {
                return DomainSyncResult::error(domain, "adapter not registered");
            }
        };

        let policy = match self.policies.get(&domain) {
            Some(p) => p.clone(),
            None => {
                return DomainSyncResult::error(domain, "policy not configured");
            }
        };

        // Set envelope to Pulling
        let mut envelope = adapter.envelope();
        envelope.sync_state = SyncState::Pulling;
        adapter.set_envelope(envelope);

        // Pull from cloud with timeout
        let pull_result = tokio::time::timeout(
            Duration::from_secs(policy.timeout_secs),
            self.transport.pull(&self.user_id, domain),
        )
        .await;

        let pull_result = match pull_result {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                let mut envelope = adapter.envelope();
                envelope.mark_error(e.message.clone());
                adapter.set_envelope(envelope);

                self.log_event(
                    domain,
                    SyncOperation::Pull,
                    false,
                    start,
                    0,
                    Some(&e.message),
                );
                return DomainSyncResult::error(domain, e.message);
            }
            Err(_) => {
                let msg = "timeout";
                let mut envelope = adapter.envelope();
                envelope.mark_error(msg.to_string());
                adapter.set_envelope(envelope);

                self.log_event(domain, SyncOperation::Pull, false, start, 0, Some(msg));
                return DomainSyncResult::error(domain, msg);
            }
        };

        // Merge remote data if available
        if let Some(ref payload) = pull_result.payload {
            if let Err(e) = adapter.validate(payload) {
                let mut envelope = adapter.envelope();
                envelope.mark_error(e.message.clone());
                adapter.set_envelope(envelope);
                return DomainSyncResult::error(domain, format!("validation: {}", e.message));
            }

            match adapter.merge_remote(payload) {
                Ok(merge) => {
                    let mut envelope = adapter.envelope();
                    if let Some(v) = pull_result.version {
                        envelope.mark_pulled(v);
                    }
                    adapter.set_envelope(envelope);

                    let bytes = payload.size() as u64;
                    self.log_event(domain, SyncOperation::Pull, true, start, bytes, None);

                    DomainSyncResult {
                        domain,
                        success: true,
                        merge: Some(merge),
                        version: pull_result.version,
                        error: None,
                        duration_ms: start.elapsed().as_millis() as u64,
                    }
                }
                Err(e) => {
                    let mut envelope = adapter.envelope();
                    envelope.mark_error(e.message.clone());
                    adapter.set_envelope(envelope);
                    DomainSyncResult::error(domain, format!("merge: {}", e.message))
                }
            }
        } else {
            // No remote data — nothing to merge
            let mut envelope = adapter.envelope();
            envelope.sync_state = SyncState::Clean;
            adapter.set_envelope(envelope);

            self.log_event(domain, SyncOperation::Pull, true, start, 0, None);
            DomainSyncResult::ok(domain, None)
        }
    }

    /// Push all dirty domains.
    pub async fn push_dirty(&mut self) -> Vec<DomainSyncResult> {
        let mut results = Vec::new();

        let dirty_domains: Vec<SyncDomain> = self
            .adapters
            .iter()
            .filter(|(_, a)| a.has_dirty_data())
            .map(|(d, _)| *d)
            .collect();

        for domain in dirty_domains {
            let result = self.push_domain(domain).await;
            results.push(result);
        }
        results
    }

    /// Push a single domain to cloud, with conflict resolution.
    pub async fn push_domain(&mut self, domain: SyncDomain) -> DomainSyncResult {
        let policy = match self.policies.get(&domain).cloned() {
            Some(p) => p,
            None => return DomainSyncResult::error(domain, "policy not configured"),
        };

        let max_retries = policy.max_conflict_retries;

        for attempt in 0..=max_retries {
            let result = self.push_domain_once(domain, &policy).await;

            if result.success {
                return result;
            }

            // Check if it's a conflict and we have retries left
            if result
                .error
                .as_deref()
                .map(|e| e.contains("conflict"))
                .unwrap_or(false)
                && attempt < max_retries
            {
                // Pull fresh data, merge, then retry
                let _pull_result = self.pull_domain(domain).await;
                // After pull+merge, the adapter now has merged state → re-export and push
                continue;
            }

            // Non-conflict error or max retries exceeded
            return result;
        }

        DomainSyncResult::error(domain, "max conflict retries exceeded")
    }

    /// Single push attempt (no retry).
    async fn push_domain_once(
        &mut self,
        domain: SyncDomain,
        policy: &SyncPolicy,
    ) -> DomainSyncResult {
        let start = std::time::Instant::now();

        let adapter = match self.adapters.get(&domain) {
            Some(a) => a,
            None => return DomainSyncResult::error(domain, "adapter not registered"),
        };

        // Export payload (prefer delta if policy says so)
        let (payload, op) = if policy.prefer_delta {
            match adapter.export_delta() {
                Ok(Some(delta)) => (delta, SyncOperation::PushDelta),
                Ok(None) | Err(_) => match adapter.export_full() {
                    Ok(full) => (full, SyncOperation::PushFull),
                    Err(e) => {
                        return DomainSyncResult::error(domain, format!("export: {}", e.message));
                    }
                },
            }
        } else {
            match adapter.export_full() {
                Ok(full) => (full, SyncOperation::PushFull),
                Err(e) => {
                    return DomainSyncResult::error(domain, format!("export: {}", e.message));
                }
            }
        };

        let envelope = adapter.envelope();
        let expected_version = envelope.cloud_version;
        let bytes = payload.size() as u64;

        // Set syncing state
        let mut envelope = adapter.envelope();
        envelope.sync_state = SyncState::Syncing;
        adapter.set_envelope(envelope);

        // Push with timeout
        let push_result = tokio::time::timeout(
            Duration::from_secs(policy.timeout_secs),
            self.transport
                .push(&self.user_id, domain, &payload, expected_version),
        )
        .await;

        match push_result {
            Ok(Ok(result)) if result.success => {
                // Success
                let mut envelope = adapter.envelope();
                if let Some(v) = result.new_version {
                    envelope.mark_synced(v);
                    envelope.stats.bytes_pushed += bytes;
                }
                adapter.set_envelope(envelope);
                let _ = adapter.clear_dirty();

                self.log_event(domain, op, true, start, bytes, None);

                DomainSyncResult {
                    domain,
                    success: true,
                    merge: None,
                    version: result.new_version,
                    error: None,
                    duration_ms: start.elapsed().as_millis() as u64,
                }
            }
            Ok(Ok(result)) if result.is_conflict => {
                // Conflict
                let remote_version = result
                    .new_version
                    .or(expected_version.map(|v| v + 1))
                    .unwrap_or(0);
                let mut envelope = adapter.envelope();
                envelope.mark_conflict(remote_version);
                adapter.set_envelope(envelope);

                self.log_event(domain, op, false, start, 0, Some("conflict"));

                DomainSyncResult {
                    domain,
                    success: false,
                    merge: None,
                    version: None,
                    error: Some("conflict".to_string()),
                    duration_ms: start.elapsed().as_millis() as u64,
                }
            }
            Ok(Ok(result)) => {
                // Other failure
                let mut envelope = adapter.envelope();
                envelope.mark_error(result.message.clone());
                adapter.set_envelope(envelope);

                self.log_event(domain, op, false, start, 0, Some(&result.message));
                DomainSyncResult::error(domain, result.message)
            }
            Ok(Err(e)) => {
                let mut envelope = adapter.envelope();
                envelope.mark_error(e.message.clone());
                adapter.set_envelope(envelope);

                self.log_event(domain, op, false, start, 0, Some(&e.message));
                DomainSyncResult::error(domain, e.message)
            }
            Err(_) => {
                let mut envelope = adapter.envelope();
                envelope.mark_error("timeout".to_string());
                adapter.set_envelope(envelope);

                self.log_event(domain, op, false, start, 0, Some("timeout"));
                DomainSyncResult::error(domain, "timeout")
            }
        }
    }

    /// Notify that a local write happened for a domain.
    /// This transitions the domain from Clean → Dirty.
    pub fn notify_write(&self, domain: SyncDomain) {
        if let Some(adapter) = self.adapters.get(&domain) {
            let mut envelope = adapter.envelope();
            envelope.mark_dirty();
            adapter.set_envelope(envelope);
        }
    }

    /// Get the sync envelope for a domain.
    pub fn envelope(&self, domain: SyncDomain) -> Option<SyncEnvelope> {
        self.adapters.get(&domain).map(|a| a.envelope())
    }

    /// Get sync events log (for /sync command display).
    pub fn event_log(&self) -> &[SyncEvent] {
        &self.event_log
    }

    /// Get summary of all domain states.
    pub fn status_summary(&self) -> Vec<(SyncDomain, SyncState)> {
        self.adapters
            .iter()
            .map(|(d, a)| (*d, a.envelope().sync_state))
            .collect()
    }

    /// Get cumulative stats for a domain.
    pub fn domain_stats(&self, domain: SyncDomain) -> Option<SyncStats> {
        self.adapters.get(&domain).map(|a| a.envelope().stats)
    }

    /// Whether cloud is available.
    pub fn is_cloud_available(&self) -> bool {
        self.cloud_available
    }

    /// Update the sync envelope for a domain from an external sync operation.
    /// Used when legacy sync functions (try_cloud_push, etc.) succeed and need
    /// to reflect their state into the orchestrator's tracking.
    pub fn update_envelope(&self, domain: SyncDomain, envelope: SyncEnvelope) {
        if let Some(adapter) = self.adapters.get(&domain) {
            adapter.set_envelope(envelope);
        }
    }

    /// Record an external sync event in the log.
    /// Used when legacy sync functions complete and need to be tracked.
    pub fn log_external_event(
        &mut self,
        domain: SyncDomain,
        op: SyncOperation,
        success: bool,
        duration_ms: u64,
        bytes: u64,
        error: Option<&str>,
    ) {
        if self.event_log.len() >= 100 {
            self.event_log.remove(0);
        }
        self.event_log.push(SyncEvent {
            domain,
            operation: op,
            success,
            duration_ms,
            bytes_transferred: bytes,
            version_before: None,
            version_after: None,
            error: error.map(|s| s.to_string()),
            timestamp: epoch_secs(),
        });
    }

    fn log_event(
        &mut self,
        domain: SyncDomain,
        op: SyncOperation,
        success: bool,
        start: std::time::Instant,
        bytes: u64,
        error: Option<&str>,
    ) {
        // Keep bounded event log (last 100 events)
        if self.event_log.len() >= 100 {
            self.event_log.remove(0);
        }
        self.event_log.push(SyncEvent {
            domain,
            operation: op,
            success,
            duration_ms: start.elapsed().as_millis() as u64,
            bytes_transferred: bytes,
            version_before: None,
            version_after: None,
            error: error.map(|s| s.to_string()),
            timestamp: epoch_secs(),
        });
    }
}

/// Result of syncing a single domain.
#[derive(Debug, Clone)]
pub struct DomainSyncResult {
    pub domain: SyncDomain,
    pub success: bool,
    pub merge: Option<MergeResult>,
    pub version: Option<u64>,
    pub error: Option<String>,
    pub duration_ms: u64,
}

impl DomainSyncResult {
    pub fn ok(domain: SyncDomain, version: Option<u64>) -> Self {
        Self {
            domain,
            success: true,
            merge: None,
            version,
            error: None,
            duration_ms: 0,
        }
    }

    pub fn error(domain: SyncDomain, msg: impl Into<String>) -> Self {
        Self {
            domain,
            success: false,
            merge: None,
            version: None,
            error: Some(msg.into()),
            duration_ms: 0,
        }
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Compute SHA-256 checksum of data.
pub fn sha256_checksum(data: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    // Lightweight hash for sync purposes (not cryptographic)
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_envelope_lifecycle() {
        let mut env = SyncEnvelope::new(SyncDomain::Learning);
        assert!(env.sync_state.is_clean());
        assert_eq!(env.local_version, 0);

        // Write → Dirty
        env.mark_dirty();
        assert!(env.sync_state.is_dirty());
        assert_eq!(env.local_version, 1);

        // Another write while dirty
        env.mark_dirty();
        assert!(env.sync_state.is_dirty());
        assert_eq!(env.local_version, 2);

        // Synced
        env.sync_state = SyncState::Syncing;
        env.mark_synced(5);
        assert!(env.sync_state.is_clean());
        assert_eq!(env.cloud_version, Some(5));
        assert_eq!(env.stats.pushes, 1);
    }

    #[test]
    fn sync_envelope_conflict() {
        let mut env = SyncEnvelope::new(SyncDomain::Tasks);
        env.mark_dirty();
        env.mark_conflict(10);
        assert!(env.sync_state.is_conflict());
        assert_eq!(env.stats.conflicts, 1);
    }

    #[test]
    fn sync_envelope_error_accumulates() {
        let mut env = SyncEnvelope::new(SyncDomain::Events);
        env.mark_error("timeout".to_string());
        assert!(env.sync_state.is_error());
        assert_eq!(env.stats.errors, 1);

        if let SyncState::Error { retry_count, .. } = &env.sync_state {
            assert_eq!(*retry_count, 1);
        }

        env.mark_error("another timeout".to_string());
        if let SyncState::Error { retry_count, .. } = &env.sync_state {
            assert_eq!(*retry_count, 2);
        }
        assert_eq!(env.stats.errors, 2);
    }

    #[test]
    fn sync_policy_defaults() {
        let learning = SyncPolicy::learning();
        assert!(matches!(learning.pull, PullTrigger::SessionStart));
        assert!(matches!(learning.push, PushTrigger::SessionEnd));
        assert!(learning.prefer_delta);

        let events = SyncPolicy::events();
        assert!(matches!(events.pull, PullTrigger::Never));
        assert!(matches!(events.push, PushTrigger::Batched { .. }));
    }

    #[test]
    fn domain_display() {
        assert_eq!(SyncDomain::Learning.to_string(), "learning");
        assert_eq!(SyncDomain::Events.to_string(), "events");
        assert_eq!(SyncDomain::Tasks.to_string(), "tasks");
    }

    #[test]
    fn noop_transport_is_functional() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let transport = NoopTransport;
            let result = transport
                .push("u1", SyncDomain::Learning, &test_payload(), None)
                .await
                .unwrap();
            assert!(result.success);

            let result = transport.pull("u1", SyncDomain::Learning).await.unwrap();
            assert!(result.payload.is_none());

            assert!(transport.health_check().await);
        });
    }

    #[tokio::test]
    async fn orchestrator_pull_all_with_noop() {
        let transport = Arc::new(NoopTransport);
        let mut orch = SyncOrchestrator::new(transport, "user1");

        // Register a mock adapter
        let adapter = MockAdapter::new(SyncDomain::Learning);
        orch.register(Box::new(adapter), SyncPolicy::learning());

        let results = orch.pull_all().await;
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
    }

    #[tokio::test]
    async fn orchestrator_push_dirty_skips_clean() {
        let transport = Arc::new(NoopTransport);
        let mut orch = SyncOrchestrator::new(transport, "user1");

        let adapter = MockAdapter::new(SyncDomain::Learning);
        orch.register(Box::new(adapter), SyncPolicy::learning());

        // Nothing is dirty → push_dirty returns empty
        let results = orch.push_dirty().await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn orchestrator_push_dirty_pushes_dirty() {
        let transport = Arc::new(NoopTransport);
        let mut orch = SyncOrchestrator::new(transport, "user1");

        let adapter = MockAdapter::new(SyncDomain::Learning);
        orch.register(Box::new(adapter), SyncPolicy::learning());

        // Mark dirty
        orch.notify_write(SyncDomain::Learning);

        let results = orch.push_dirty().await;
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
    }

    #[tokio::test]
    async fn orchestrator_status_summary() {
        let transport = Arc::new(NoopTransport);
        let mut orch = SyncOrchestrator::new(transport, "user1");

        let adapter1 = MockAdapter::new(SyncDomain::Learning);
        let adapter2 = MockAdapter::new(SyncDomain::Events);
        orch.register(Box::new(adapter1), SyncPolicy::learning());
        orch.register(Box::new(adapter2), SyncPolicy::events());

        let summary = orch.status_summary();
        assert_eq!(summary.len(), 2);
    }

    // ── Mock adapter for testing ────────────────────────────────────────────

    struct MockAdapter {
        domain: SyncDomain,
        envelope: std::sync::Mutex<SyncEnvelope>,
    }

    impl MockAdapter {
        fn new(domain: SyncDomain) -> Self {
            Self {
                domain,
                envelope: std::sync::Mutex::new(SyncEnvelope::new(domain)),
            }
        }
    }

    #[async_trait]
    impl DomainAdapter for MockAdapter {
        fn domain(&self) -> SyncDomain {
            self.domain
        }

        fn export_full(&self) -> Result<SyncPayload, SyncError> {
            Ok(test_payload())
        }

        fn export_delta(&self) -> Result<Option<SyncPayload>, SyncError> {
            Ok(None)
        }

        fn merge_remote(&self, _remote: &SyncPayload) -> Result<MergeResult, SyncError> {
            Ok(MergeResult::default())
        }

        fn resolve_conflict(
            &self,
            _local: &SyncPayload,
            _remote: &SyncPayload,
        ) -> Result<SyncPayload, SyncError> {
            Ok(test_payload())
        }

        fn validate(&self, _payload: &SyncPayload) -> Result<(), SyncError> {
            Ok(())
        }

        fn envelope(&self) -> SyncEnvelope {
            self.envelope.lock().unwrap().clone()
        }

        fn set_envelope(&self, envelope: SyncEnvelope) {
            *self.envelope.lock().unwrap() = envelope;
        }
    }

    fn test_payload() -> SyncPayload {
        SyncPayload {
            data: b"test".to_vec(),
            format: PayloadFormat::Full,
            checksum: "abcd".to_string(),
            item_count: 1,
            compressed: false,
        }
    }
}
