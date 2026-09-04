use crate::auth::DatabaseUserRecord;
use crate::auth::session::SessionRecord;
use astra_core::{
    ErrorResponse, MatrixOneSettings, connect_matrixone, identity::USER_ID_MAX_LEN, internal_error,
    release_global_connections,
};
use axum::{Json, http::StatusCode};
use sha2::Digest;
use sqlx::{Execute, Executor, MySql, QueryBuilder, Row, Transaction, query, query_scalar};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Deref;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use uuid::Uuid;

const CAUSAL_EDGE_KIND: &str = "causal";
const EDGE_PENDING_DISPATCH_IDENTITY_COLUMNS: &[&str] = &[
    "user_id",
    "session_id",
    "run_id",
    "turn_chain_id",
    "request_id",
];
const EDGE_PENDING_DISPATCH_LEGACY_COLUMNS: &[&str] = &[
    "user_id",
    "edge_agent_id",
    "request_id",
    "payload_json",
    "result_json",
    "status",
    "pod_id",
    "dispatched_at",
    "completed_at",
    "created_at",
];
const EDGE_PENDING_DISPATCH_LEGACY_PRIMARY_KEY: &[&str] = &["user_id", "request_id"];
const EDGE_PENDING_DISPATCH_LEGACY_ARCHIVE_TABLE: &str =
    "edge_pending_dispatch_legacy_owner_request_v1";
const TOOL_INVOCATION_LEDGER_REQUIRED_COLUMNS: &[&str] = &["identity_key"];
const TOOL_INVOCATION_LEDGER_REQUIRED_VARCHAR_WIDTHS: &[(&str, u64)] = &[("identity_key", 71)];

/// Sole owner of a short-lived MatrixOne bootstrap pool.
///
/// MatrixOne does not complete SQLx's MySQL shutdown handshake. Teardown
/// therefore waits for every checked-out connection to return, detaches each
/// socket from SQLx, drops it synchronously, and only then returns the global
/// connection reservation. Keeping this type private prevents bootstrap code
/// from cloning the pool beyond that ownership boundary.
struct BootstrapPool {
    pool: sqlx::Pool<MySql>,
    max_connections: u64,
    released: bool,
}

impl BootstrapPool {
    fn new(pool: sqlx::Pool<MySql>, max_connections: u64) -> Self {
        Self {
            pool,
            max_connections,
            released: false,
        }
    }

    fn pool(&self) -> &sqlx::Pool<MySql> {
        &self.pool
    }

    async fn release(mut self) -> Result<(), sqlx::Error> {
        while self.pool.size() > 0 {
            let connection = self.pool.acquire().await?;
            drop(connection.detach());
        }
        release_global_connections(self.max_connections);
        self.released = true;
        Ok(())
    }
}

impl Drop for BootstrapPool {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        if self.pool.size() == 0 {
            release_global_connections(self.max_connections);
            self.released = true;
        } else {
            tracing::error!(
                target: "astra_services::storage",
                live_connections = self.pool.size(),
                reserved_connections = self.max_connections,
                "bootstrap pool dropped with live connections; retaining global quota reservation"
            );
        }
    }
}

fn finish_bootstrap_operation<T>(
    operation: Result<T, sqlx::Error>,
    release: Result<(), sqlx::Error>,
) -> Result<T, sqlx::Error> {
    match (operation, release) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(operation_error), Err(release_error)) => Err(sqlx::Error::Protocol(format!(
            "bootstrap operation failed: {operation_error}; pool teardown also failed: {release_error}"
        ))),
    }
}

/// Standard column width for `agent_id` across all tables.
/// All `agent_id`, `edge_agent_id`, `holder_agent_id`, and `parent_agent_id`
/// columns MUST use this width for consistency and join compatibility.
pub const AGENT_ID_LEN: usize = 255;
/// Width for application event identifiers and references.
///
/// Journal ingestion uses `evt-` plus a full SHA-256 digest (68 chars), and
/// several runtime paths use UUID-like event ids. Keep the schema wider than
/// the current longest generated id so evidence/trace writes do not fail after
/// the run has already started.
pub const AGENT_EVENT_ID_LEN: usize = 128;
static CORE_SCHEMA_INIT_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
const CORE_SCHEMA_CONTRACT_COMPONENT: &str = "astra-core";
pub const CORE_SCHEMA_CONTRACT_VERSION: &str = "2026-09-04-v70";
const CORE_SCHEMA_CONTRACT_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS astra_schema_contracts (
    component VARCHAR(64) NOT NULL PRIMARY KEY,
    contract_version VARCHAR(64) NOT NULL,
    completed_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
)";
const CORE_SCHEMA_LEASE_TABLE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS astra_schema_bootstrap_leases (
    component VARCHAR(64) NOT NULL PRIMARY KEY,
    holder_id VARCHAR(64) NOT NULL,
    lease_expires_at DATETIME(6) NOT NULL,
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
)";
const CORE_SCHEMA_TABLE_CONTRACT_SQL: &str =
    "CREATE TABLE IF NOT EXISTS astra_schema_table_contracts (
    table_name VARCHAR(128) NOT NULL PRIMARY KEY,
    component VARCHAR(64) NOT NULL,
    owner VARCHAR(128) NOT NULL,
    contract_version VARCHAR(64) NOT NULL,
    ddl_sha256 CHAR(64) NOT NULL,
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    INDEX idx_schema_table_contract_component (component, contract_version, table_name)
)";
const AGENT_RUNS_TABLE: &str = "agent_runs";
const AGENT_RUNS_CREATE_SQL: &str = "CREATE TABLE IF NOT EXISTS agent_runs (
    run_id VARCHAR(64) NOT NULL,
    user_id VARCHAR(128) NOT NULL,
    session_id VARCHAR(64) NOT NULL,
    parent_run_id VARCHAR(64) NULL,
    root_run_id VARCHAR(64) NOT NULL,
    ancestor_path VARCHAR(2048) NOT NULL,
    depth INT NOT NULL DEFAULT 0,
    delegation_id VARCHAR(64) NULL,
    agent_id VARCHAR(255) NULL,
    retry_of VARCHAR(64) NULL,
    retry_scope VARCHAR(16) NOT NULL DEFAULT 'node',
    status VARCHAR(32) NOT NULL,
    execution_mode VARCHAR(32) NOT NULL DEFAULT 'web_agent',
    trigger_type VARCHAR(64) NULL,
    trigger_event_id VARCHAR(128) NULL,
    waiting_for VARCHAR(64) NULL,
    owner_pod_id VARCHAR(128) NULL,
    owner_lease_expires_at DATETIME(6) NULL,
    cancellation_requested_at DATETIME(6) NULL,
    run_generation BIGINT NOT NULL DEFAULT 0,
    last_event_idx BIGINT NOT NULL DEFAULT -1,
    checkpoint_version VARCHAR(32) NULL,
    checkpoint_json LONGTEXT NULL,
    error_code VARCHAR(128) NULL,
    error_message TEXT NULL,
    retry_count INT NOT NULL DEFAULT 0,
    total_prompt_tokens BIGINT NOT NULL DEFAULT 0,
    total_completion_tokens BIGINT NOT NULL DEFAULT 0,
    total_tool_calls BIGINT NOT NULL DEFAULT 0,
    request_id VARCHAR(64) NULL,
    trace_id VARCHAR(64) NULL,
    agent_binding_id VARCHAR(64) NULL,
    agent_binding_name VARCHAR(255) NULL,
    agent_binding_schema_version VARCHAR(32) NULL,
    model_offering_id VARCHAR(64) NULL,
    resolved_model_name VARCHAR(255) NULL,
    runtime_profile VARCHAR(64) NULL,
    start_request_fingerprint VARCHAR(64) NULL,
    work_id VARCHAR(64) NULL,
    work_branch_id VARCHAR(64) NULL,
    work_graph_revision BIGINT NULL,
    work_item_id VARCHAR(64) NULL,
    work_item_revision BIGINT NULL,
    work_item_attempt_id VARCHAR(64) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    CONSTRAINT chk_agent_runs_retry_scope CHECK (retry_scope IN ('node', 'subtree', 'siblings')),
    CONSTRAINT chk_agent_runs_work_binding CHECK (
        (work_id IS NULL AND work_branch_id IS NULL AND work_graph_revision IS NULL)
        OR
        (work_id IS NOT NULL AND work_branch_id IS NOT NULL AND work_graph_revision > 0)
    ),
    CONSTRAINT chk_agent_runs_work_item_binding CHECK (
        (work_item_id IS NULL AND work_item_revision IS NULL AND work_item_attempt_id IS NULL)
        OR
        (work_id IS NOT NULL AND work_item_id IS NOT NULL
         AND work_item_revision > 0 AND work_item_attempt_id IS NOT NULL)
    ),
    PRIMARY KEY (user_id, run_id),
    UNIQUE KEY uq_agent_runs_run_id (run_id),
    INDEX idx_agent_runs_user_updated_run (user_id, updated_at, run_id),
    INDEX idx_agent_runs_user_session_status_updated (user_id, session_id, status, updated_at),
    INDEX idx_agent_runs_owner_root_depth (user_id, root_run_id, depth, created_at),
    INDEX idx_agent_runs_owner_parent_status_updated (user_id, parent_run_id, status, updated_at),
    INDEX idx_agent_runs_owner_retry_of (user_id, retry_of),
    INDEX idx_agent_runs_owner_lease (owner_pod_id, owner_lease_expires_at),
    INDEX idx_agent_runs_binding (agent_binding_id, created_at),
    INDEX idx_agent_runs_model_offering (model_offering_id, created_at),
    INDEX idx_agent_runs_owner_work_branch_created (
        user_id, work_id, work_branch_id, created_at, run_id
    ),
    INDEX idx_agent_runs_owner_work_item_root_latest (
        user_id, work_id, work_branch_id, work_item_id, work_item_revision,
        parent_run_id, created_at, run_id
    )
)";
const AGENT_RUNS_RUNTIME_AUTHORITY_COLUMNS: &[&str] = &[
    "model_offering_id",
    "resolved_model_name",
    "start_request_fingerprint",
];
const AGENT_RUNS_WORK_BINDING_COLUMNS: &[&str] =
    &["work_id", "work_branch_id", "work_graph_revision"];
const AGENT_RUNS_WORK_ITEM_BINDING_COLUMNS: &[&str] =
    &["work_item_id", "work_item_revision", "work_item_attempt_id"];
const AGENT_RUNS_PRESERVED_COLUMNS: &[&str] = &[
    "run_id",
    "user_id",
    "session_id",
    "parent_run_id",
    "root_run_id",
    "ancestor_path",
    "depth",
    "delegation_id",
    "agent_id",
    "retry_of",
    "retry_scope",
    "status",
    "execution_mode",
    "trigger_type",
    "trigger_event_id",
    "waiting_for",
    "owner_pod_id",
    "owner_lease_expires_at",
    "cancellation_requested_at",
    "run_generation",
    "last_event_idx",
    "checkpoint_version",
    "checkpoint_json",
    "error_code",
    "error_message",
    "retry_count",
    "total_prompt_tokens",
    "total_completion_tokens",
    "total_tool_calls",
    "request_id",
    "trace_id",
    "agent_binding_id",
    "agent_binding_name",
    "agent_binding_schema_version",
    "runtime_profile",
    "created_at",
    "updated_at",
];
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreSchemaTableSpec {
    pub name: String,
    pub owner: String,
    pub ddl_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PersistedCoreSchemaTableClaim {
    name: String,
    component: String,
    owner: String,
    contract_version: String,
    ddl_sha256: String,
}

const INSERT_CORE_SCHEMA_TABLE_CLAIM_WITHOUT_TAKEOVER_SQL: &str =
    "INSERT IGNORE INTO astra_schema_table_contracts
     (table_name, component, owner, contract_version, ddl_sha256)
     VALUES (?, ?, ?, ?, ?)";

const UPDATE_OWNED_CORE_SCHEMA_TABLE_CLAIM_SQL: &str = "UPDATE astra_schema_table_contracts
     SET owner = ?, contract_version = ?, ddl_sha256 = ?, updated_at = NOW(6)
     WHERE table_name = ? AND component = ?";

/// Validate ownership and identify declarations retired by the new contract.
///
/// A contract-version upgrade is an ownership-aware reconciliation, not an
/// unqualified delete/reinsert cycle. The old path mapped every duplicate-key
/// error to "another owner" without reading the persisted owner first. An
/// explicit read plus per-key upsert distinguishes legitimate same-component
/// upgrades from real cross-component conflicts and remains safe under retry.
fn stale_core_schema_table_claims(
    existing: &[PersistedCoreSchemaTableClaim],
    declarations: &[CoreSchemaTableSpec],
) -> Result<Vec<String>, sqlx::Error> {
    let desired_names = declarations
        .iter()
        .map(|declaration| declaration.name.as_str())
        .collect::<BTreeSet<_>>();

    for claim in existing {
        if desired_names.contains(claim.name.as_str())
            && claim.component != CORE_SCHEMA_CONTRACT_COMPONENT
        {
            return Err(sqlx::Error::Protocol(format!(
                "schema table {} is already claimed by lifecycle component {} (owner={})",
                claim.name, claim.component, claim.owner
            )));
        }
    }

    Ok(existing
        .iter()
        .filter(|claim| {
            claim.component == CORE_SCHEMA_CONTRACT_COMPONENT
                && !desired_names.contains(claim.name.as_str())
        })
        .map(|claim| claim.name.clone())
        .collect())
}

fn validate_reconciled_core_schema_table_claim(
    claim: &PersistedCoreSchemaTableClaim,
    declaration: &CoreSchemaTableSpec,
) -> Result<(), sqlx::Error> {
    if claim.component != CORE_SCHEMA_CONTRACT_COMPONENT {
        return Err(sqlx::Error::Protocol(format!(
            "schema table {} is already claimed by lifecycle component {} (owner={})",
            claim.name, claim.component, claim.owner
        )));
    }
    if claim.owner != declaration.owner
        || claim.contract_version != CORE_SCHEMA_CONTRACT_VERSION
        || claim.ddl_sha256 != declaration.ddl_sha256
    {
        return Err(sqlx::Error::Protocol(format!(
            "schema table {} lifecycle contract did not converge after owner-scoped reconciliation",
            declaration.name
        )));
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct CoreSchemaDeclaration {
    owner: String,
    ddl_sha256: String,
    count: usize,
}

#[derive(Clone, Debug, Default)]
struct CoreSchemaAuthority(Arc<StdMutex<BTreeMap<String, CoreSchemaDeclaration>>>);

impl CoreSchemaAuthority {
    fn declare(&self, owner: &str, table_name: &str, sql: &str) {
        let ddl_sha256 = format!("{:x}", sha2::Sha256::digest(sql.trim().as_bytes()));
        let mut declarations = self.0.lock().unwrap_or_else(|error| error.into_inner());
        declarations
            .entry(table_name.to_string())
            .and_modify(|declaration| declaration.count = declaration.count.saturating_add(1))
            .or_insert_with(|| CoreSchemaDeclaration {
                owner: owner.to_string(),
                ddl_sha256,
                count: 1,
            });
    }

    fn declarations(&self) -> Result<Vec<CoreSchemaTableSpec>, sqlx::Error> {
        let declarations = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let conflicts = declarations
            .iter()
            .filter(|(_, declaration)| declaration.count != 1)
            .map(|(table, declaration)| format!("{table} (claims={})", declaration.count))
            .collect::<Vec<_>>();
        if !conflicts.is_empty() {
            return Err(sqlx::Error::Protocol(format!(
                "core schema has multiple lifecycle producers for tables: {}",
                conflicts.join(", ")
            )));
        }
        Ok(declarations
            .iter()
            .map(|(name, declaration)| CoreSchemaTableSpec {
                name: name.clone(),
                owner: declaration.owner.clone(),
                ddl_sha256: declaration.ddl_sha256.clone(),
            })
            .collect())
    }
}

macro_rules! core_schema_create {
    ($pool:expr, $table_name:literal, $ddl:expr $(,)?) => {{
        let ddl = $ddl;
        ($pool).authority.declare(($pool).owner, $table_name, ddl);
        query(ddl)
    }};
}

#[derive(Clone, Debug)]
struct CoreSchemaExecutor {
    pool: sqlx::Pool<MySql>,
    authority: CoreSchemaAuthority,
    owner: &'static str,
}

impl CoreSchemaExecutor {
    fn new(pool: sqlx::Pool<MySql>) -> Self {
        Self {
            pool,
            authority: CoreSchemaAuthority::default(),
            owner: "storage",
        }
    }

    fn owned_by(&self, owner: &'static str) -> Self {
        Self {
            pool: self.pool.clone(),
            authority: self.authority.clone(),
            owner,
        }
    }
}

impl Deref for CoreSchemaExecutor {
    type Target = sqlx::Pool<MySql>;

    fn deref(&self) -> &Self::Target {
        &self.pool
    }
}

impl<'c> Executor<'c> for &'c CoreSchemaExecutor {
    type Database = MySql;

    fn fetch_many<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> futures_util::stream::BoxStream<
        'e,
        Result<sqlx::Either<sqlx::mysql::MySqlQueryResult, sqlx::mysql::MySqlRow>, sqlx::Error>,
    >
    where
        'c: 'e,
        E: 'q + Execute<'q, Self::Database>,
    {
        (&self.pool).fetch_many(query)
    }

    fn fetch_optional<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> futures_util::future::BoxFuture<'e, Result<Option<sqlx::mysql::MySqlRow>, sqlx::Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Self::Database>,
    {
        (&self.pool).fetch_optional(query)
    }

    fn prepare_with<'e, 'q: 'e>(
        self,
        sql: &'q str,
        parameters: &'e [sqlx::mysql::MySqlTypeInfo],
    ) -> futures_util::future::BoxFuture<'e, Result<sqlx::mysql::MySqlStatement<'q>, sqlx::Error>>
    where
        'c: 'e,
    {
        (&self.pool).prepare_with(sql, parameters)
    }

    fn describe<'e, 'q: 'e>(
        self,
        sql: &'q str,
    ) -> futures_util::future::BoxFuture<'e, Result<sqlx::Describe<MySql>, sqlx::Error>>
    where
        'c: 'e,
    {
        (&self.pool).describe(sql)
    }
}

pub(crate) fn rows_affected_to_i64(rows: u64, context: &str) -> Result<i64, sqlx::Error> {
    i64::try_from(rows).map_err(|_| {
        sqlx::Error::Protocol(format!("{context}: rows_affected {rows} exceeds i64::MAX"))
    })
}

const AGENT_EVENTS_OWNER_SESSION_TURN_INDEX_DECL: &str =
    "INDEX idx_agent_events_owner_session_turn (user_id, session_id, turn_seq)";
const AGENT_EVENTS_OWNER_SESSION_TURN_INDEX_ALTER_SQL: &str = "ALTER TABLE agent_events ADD INDEX idx_agent_events_owner_session_turn (user_id, session_id, turn_seq)";

fn agent_events_create_sql() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS agent_events (
            event_id VARCHAR({AGENT_EVENT_ID_LEN}) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            agent_id VARCHAR(255) NULL,
            agent_version VARCHAR(32) NULL,
            event_type VARCHAR(64) NOT NULL,
            content LONGTEXT NULL,
            parent_event_id VARCHAR({AGENT_EVENT_ID_LEN}) NULL,
            causal_chain_id VARCHAR({AGENT_EVENT_ID_LEN}) NULL,
            run_id VARCHAR(64) NULL,
            parent_run_id VARCHAR(64) NULL,
            turn_id VARCHAR(64) NULL,
            turn_seq BIGINT NULL,
            round_index BIGINT NULL,
            tool_call_id VARCHAR(128) NULL,
            parent_agent_id VARCHAR(255) NULL,
            trace_kind VARCHAR(64) NULL,
            token_usage JSON NULL,
            llm_model_used VARCHAR(128) NULL,
            llm_params JSON NULL,
            metadata JSON NULL,
            skill_name VARCHAR(255) NULL,
            skill_version VARCHAR(64) NULL,
            reasoning_content LONGTEXT NULL,
            token_input  BIGINT NULL,
            token_output BIGINT NULL,
            token_total  BIGINT NULL,
            meta_tool_name VARCHAR(255) NULL,
            meta_duration_ms INT NULL,
            user_feedback_score BIGINT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, event_id),
            INDEX idx_agent_events_owner_session_created (user_id, session_id, created_at),
            INDEX idx_agent_events_owner_session_type_created (user_id, session_id, event_type, created_at),
            INDEX idx_agent_events_owner_session_model_created (user_id, session_id, llm_model_used, created_at DESC),
            INDEX idx_agent_events_owner_session_parent (user_id, session_id, parent_event_id),
            {AGENT_EVENTS_OWNER_SESSION_TURN_INDEX_DECL},
            INDEX idx_agent_events_user_created (user_id, created_at),
            INDEX idx_agent_events_owner_causal_chain_created (user_id, causal_chain_id, created_at, event_id),
            INDEX idx_agent_events_skill_created (skill_name, created_at),
            INDEX idx_agent_events_created_at (created_at),
            INDEX idx_agent_events_tool_name (meta_tool_name),
            INDEX idx_agent_events_trace (user_id, session_id, turn_id, created_at),
            INDEX idx_agent_events_run (user_id, session_id, run_id, created_at),
            INDEX idx_agent_events_parent_run (user_id, session_id, parent_run_id, created_at),
            INDEX idx_agent_events_tool_call (user_id, session_id, tool_call_id)
        )"
    )
}

const EVAL_CALIBRATION_ASSESSMENTS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS eval_calibration_assessments (
            calibration_id VARCHAR(64) NOT NULL,
            user_id        VARCHAR(128) NOT NULL,
            agent_id       VARCHAR(255),
            session_id     VARCHAR(64) NOT NULL,
            confidence     DECIMAL(5,4) NOT NULL,
            quality_score  DECIMAL(5,4) NOT NULL,
            created_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, calibration_id),
            INDEX idx_eval_calibration_user_created (user_id, created_at),
            INDEX idx_eval_calibration_user_agent_created (user_id, agent_id, created_at),
            INDEX idx_eval_calibration_session (user_id, session_id, created_at)
        )";

struct CoreSchemaDatabaseLease {
    pool: sqlx::Pool<MySql>,
    holder_id: String,
    stop_heartbeat: Option<tokio::sync::oneshot::Sender<()>>,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
}

impl CoreSchemaDatabaseLease {
    async fn acquire(pool: &sqlx::Pool<MySql>) -> Result<Self, sqlx::Error> {
        query(CORE_SCHEMA_LEASE_TABLE_SQL).execute(pool).await?;
        let holder_id = Uuid::new_v4().to_string();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            query(
                "INSERT INTO astra_schema_bootstrap_leases \
                 (component, holder_id, lease_expires_at, updated_at) \
                 VALUES (?, ?, DATE_ADD(NOW(6), INTERVAL 30 SECOND), NOW(6)) \
                 ON DUPLICATE KEY UPDATE \
                   updated_at = IF(lease_expires_at <= NOW(6), VALUES(updated_at), updated_at), \
                   holder_id = IF(lease_expires_at <= NOW(6), VALUES(holder_id), holder_id), \
                   lease_expires_at = IF(lease_expires_at <= NOW(6), VALUES(lease_expires_at), lease_expires_at)",
            )
            .bind(CORE_SCHEMA_CONTRACT_COMPONENT)
            .bind(&holder_id)
            .execute(pool)
            .await?;
            let current_holder: Option<String> = sqlx::query_scalar(
                "SELECT holder_id FROM astra_schema_bootstrap_leases WHERE component = ?",
            )
            .bind(CORE_SCHEMA_CONTRACT_COMPONENT)
            .fetch_optional(pool)
            .await?;
            if current_holder.as_deref() == Some(holder_id.as_str()) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(sqlx::Error::Protocol(
                    "timed out waiting for the current core schema bootstrap lease".to_string(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let heartbeat_pool = pool.clone();
        let heartbeat_holder = holder_id.clone();
        let (stop_heartbeat, mut stop_rx) = tokio::sync::oneshot::channel();
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                        let renewal = query(
                            "UPDATE astra_schema_bootstrap_leases \
                             SET lease_expires_at = DATE_ADD(NOW(6), INTERVAL 30 SECOND), \
                                 updated_at = NOW(6) \
                             WHERE component = ? AND holder_id = ? \
                               AND lease_expires_at > NOW(6)",
                        )
                        .bind(CORE_SCHEMA_CONTRACT_COMPONENT)
                        .bind(&heartbeat_holder)
                        .execute(&heartbeat_pool)
                        .await;
                        match renewal {
                            Ok(_) => {
                                let still_owned: Result<i64, sqlx::Error> = sqlx::query_scalar(
                                    "SELECT COUNT(*) FROM astra_schema_bootstrap_leases \
                                     WHERE component = ? AND holder_id = ? \
                                       AND lease_expires_at > NOW(6)",
                                )
                                .bind(CORE_SCHEMA_CONTRACT_COMPONENT)
                                .bind(&heartbeat_holder)
                                .fetch_one(&heartbeat_pool)
                                .await;
                                match still_owned {
                                    Ok(1) => continue,
                                    Ok(_) => tracing::error!(
                                        holder_id = %heartbeat_holder,
                                        "core schema bootstrap lease was lost before completion"
                                    ),
                                    Err(error) => tracing::error!(
                                        holder_id = %heartbeat_holder,
                                        error = %error,
                                        "core schema bootstrap lease ownership check failed"
                                    ),
                                }
                                break;
                            }
                            Err(error) => {
                                tracing::error!(
                                    holder_id = %heartbeat_holder,
                                    error = %error,
                                    "core schema bootstrap lease heartbeat failed"
                                );
                                break;
                            }
                        }
                    }
                }
            }
        });
        Ok(Self {
            pool: pool.clone(),
            holder_id,
            stop_heartbeat: Some(stop_heartbeat),
            heartbeat: Some(heartbeat),
        })
    }

    fn holder_id(&self) -> &str {
        &self.holder_id
    }

    async fn release(mut self) -> Result<(), sqlx::Error> {
        if let Some(stop_heartbeat) = self.stop_heartbeat.take() {
            let _ = stop_heartbeat.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.await.map_err(|error| {
                sqlx::Error::Protocol(format!(
                    "core schema bootstrap lease heartbeat task failed: {error}"
                ))
            })?;
        }
        query("DELETE FROM astra_schema_bootstrap_leases WHERE component = ? AND holder_id = ?")
            .bind(CORE_SCHEMA_CONTRACT_COMPONENT)
            .bind(&self.holder_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

impl Drop for CoreSchemaDatabaseLease {
    fn drop(&mut self) {
        if let Some(stop_heartbeat) = self.stop_heartbeat.take() {
            let _ = stop_heartbeat.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
    }
}

async fn core_schema_contract_is_current(pool: &sqlx::Pool<MySql>) -> Result<bool, sqlx::Error> {
    query(CORE_SCHEMA_CONTRACT_TABLE_SQL).execute(pool).await?;
    query(CORE_SCHEMA_TABLE_CONTRACT_SQL).execute(pool).await?;
    let persisted: Option<String> = sqlx::query_scalar(
        "SELECT contract_version FROM astra_schema_contracts WHERE component = ?",
    )
    .bind(CORE_SCHEMA_CONTRACT_COMPONENT)
    .fetch_optional(pool)
    .await?;
    match persisted {
        None => Ok(false),
        Some(version) if version == CORE_SCHEMA_CONTRACT_VERSION => Ok(true),
        Some(_) => Ok(false),
    }
}

pub async fn load_core_schema_table_contracts(
    pool: &sqlx::Pool<MySql>,
) -> Result<Vec<CoreSchemaTableSpec>, sqlx::Error> {
    query(
        "SELECT table_name, owner, ddl_sha256
         FROM astra_schema_table_contracts
         WHERE component = ? AND contract_version = ?
         ORDER BY table_name",
    )
    .bind(CORE_SCHEMA_CONTRACT_COMPONENT)
    .bind(CORE_SCHEMA_CONTRACT_VERSION)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| {
        Ok(CoreSchemaTableSpec {
            name: row.try_get("table_name")?,
            owner: row.try_get("owner")?,
            ddl_sha256: row.try_get("ddl_sha256")?,
        })
    })
    .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CoreSchemaCatalogState {
    Ready,
    Missing(Vec<String>),
    Invalid(String),
}

async fn inspect_core_schema_catalog(
    pool: &sqlx::Pool<MySql>,
    database: &str,
) -> Result<CoreSchemaCatalogState, sqlx::Error> {
    let rows = query("SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?")
        .bind(database)
        .fetch_all(pool)
        .await?;
    let existing = rows
        .into_iter()
        .map(|row| row.try_get::<String, _>("TABLE_NAME"))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let contracts = load_core_schema_table_contracts(pool).await?;
    if contracts.is_empty() {
        return Ok(CoreSchemaCatalogState::Invalid(
            "core schema table authority is empty for the current contract".to_string(),
        ));
    }
    let malformed = contracts
        .iter()
        .filter(|table| {
            table.name.trim().is_empty()
                || table.owner.trim().is_empty()
                || table.ddl_sha256.len() != 64
        })
        .map(|table| table.name.clone())
        .collect::<Vec<_>>();
    if !malformed.is_empty() {
        return Ok(CoreSchemaCatalogState::Invalid(format!(
            "core schema table authority contains malformed claims: {}",
            malformed.join(", ")
        )));
    }
    let missing = contracts
        .iter()
        .filter(|table| !existing.contains(&table.name))
        .map(|table| format!("{} (owner={})", table.name, table.owner))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(CoreSchemaCatalogState::Ready)
    } else {
        Ok(CoreSchemaCatalogState::Missing(missing))
    }
}

async fn verify_core_schema_catalog(
    pool: &sqlx::Pool<MySql>,
    database: &str,
) -> Result<(), sqlx::Error> {
    match inspect_core_schema_catalog(pool, database).await? {
        CoreSchemaCatalogState::Ready => Ok(()),
        CoreSchemaCatalogState::Missing(missing) => Err(sqlx::Error::Protocol(format!(
            "core schema catalog is incomplete after bootstrap: missing {}",
            missing.join(", ")
        ))),
        CoreSchemaCatalogState::Invalid(message) => Err(sqlx::Error::Protocol(message)),
    }
}

async fn publish_core_schema_table_contracts(
    pool: &sqlx::Pool<MySql>,
    declarations: &[CoreSchemaTableSpec],
) -> Result<(), sqlx::Error> {
    if declarations.is_empty() {
        return Err(sqlx::Error::Protocol(
            "core schema bootstrap produced no table declarations".to_string(),
        ));
    }
    let mut transaction = pool.begin().await?;
    let existing = query(
        "SELECT table_name, component, owner, contract_version, ddl_sha256
         FROM astra_schema_table_contracts",
    )
    .fetch_all(&mut *transaction)
    .await?
    .into_iter()
    .map(|row| {
        Ok(PersistedCoreSchemaTableClaim {
            name: row.try_get("table_name")?,
            component: row.try_get("component")?,
            owner: row.try_get("owner")?,
            contract_version: row.try_get("contract_version")?,
            ddl_sha256: row.try_get("ddl_sha256")?,
        })
    })
    .collect::<Result<Vec<_>, sqlx::Error>>()?;
    let stale_claims = stale_core_schema_table_claims(&existing, declarations)?;

    for declaration in declarations {
        // Insert without taking over an existing identity. A concurrent foreign
        // component may claim the table after the pre-read above, so a duplicate
        // must be a true no-op. Do not express that no-op as an update of the
        // primary key: some MySQL-protocol engines reject every primary-key
        // update, even assignment to its current value. The exact post-write
        // read below validates convergence and surfaces any suppressed insert
        // error as a contract mismatch rather than granting ownership.
        query(INSERT_CORE_SCHEMA_TABLE_CLAIM_WITHOUT_TAKEOVER_SQL)
            .bind(&declaration.name)
            .bind(CORE_SCHEMA_CONTRACT_COMPONENT)
            .bind(&declaration.owner)
            .bind(CORE_SCHEMA_CONTRACT_VERSION)
            .bind(&declaration.ddl_sha256)
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                sqlx::Error::Protocol(format!(
                    "failed to insert schema table {} lifecycle contract: {error}",
                    declaration.name
                ))
            })?;

        // Only the owning lifecycle component may mutate an existing claim.
        query(UPDATE_OWNED_CORE_SCHEMA_TABLE_CLAIM_SQL)
            .bind(&declaration.owner)
            .bind(CORE_SCHEMA_CONTRACT_VERSION)
            .bind(&declaration.ddl_sha256)
            .bind(&declaration.name)
            .bind(CORE_SCHEMA_CONTRACT_COMPONENT)
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                sqlx::Error::Protocol(format!(
                    "failed to update schema table {} lifecycle contract: {error}",
                    declaration.name
                ))
            })?;

        let row = query(
            "SELECT table_name, component, owner, contract_version, ddl_sha256
             FROM astra_schema_table_contracts
             WHERE table_name = ?",
        )
        .bind(&declaration.name)
        .fetch_one(&mut *transaction)
        .await?;
        let reconciled = PersistedCoreSchemaTableClaim {
            name: row.try_get("table_name")?,
            component: row.try_get("component")?,
            owner: row.try_get("owner")?,
            contract_version: row.try_get("contract_version")?,
            ddl_sha256: row.try_get("ddl_sha256")?,
        };
        validate_reconciled_core_schema_table_claim(&reconciled, declaration)?;
    }
    for table_name in stale_claims {
        query(
            "DELETE FROM astra_schema_table_contracts
             WHERE table_name = ? AND component = ?",
        )
        .bind(table_name)
        .bind(CORE_SCHEMA_CONTRACT_COMPONENT)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await
}

async fn mark_core_schema_contract_current(
    pool: &sqlx::Pool<MySql>,
    holder_id: &str,
) -> Result<(), sqlx::Error> {
    query("DELETE FROM astra_schema_contracts WHERE component = ?")
        .bind(CORE_SCHEMA_CONTRACT_COMPONENT)
        .execute(pool)
        .await?;
    let result = query(
        "INSERT INTO astra_schema_contracts (component, contract_version) \
         SELECT ?, ? FROM astra_schema_bootstrap_leases \
         WHERE component = ? AND holder_id = ? AND lease_expires_at > NOW(6)",
    )
    .bind(CORE_SCHEMA_CONTRACT_COMPONENT)
    .bind(CORE_SCHEMA_CONTRACT_VERSION)
    .bind(CORE_SCHEMA_CONTRACT_COMPONENT)
    .bind(holder_id)
    .execute(pool)
    .await?;
    if result.rows_affected() != 1 {
        return Err(sqlx::Error::Protocol(
            "core schema bootstrap lost its lease before publishing completion".to_string(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CoreSchemaVisibility {
    Visible,
    Lag(String),
}

async fn verify_core_schema_visible(
    pool: &sqlx::Pool<MySql>,
    database: &str,
) -> Result<CoreSchemaVisibility, sqlx::Error> {
    let persisted: Option<String> = sqlx::query_scalar(
        "SELECT contract_version FROM astra_schema_contracts WHERE component = ?",
    )
    .bind(CORE_SCHEMA_CONTRACT_COMPONENT)
    .fetch_optional(pool)
    .await?;
    if persisted.as_deref() != Some(CORE_SCHEMA_CONTRACT_VERSION) {
        return Ok(CoreSchemaVisibility::Lag(
            "core schema completion marker is not visible on a fresh connection".to_string(),
        ));
    }
    // The published table authority is the readiness boundary. A fixed probe
    // list inevitably drifts whenever a new runtime table is added and can
    // report ready while a fresh connection still cannot see that table.
    match inspect_core_schema_catalog(pool, database).await? {
        CoreSchemaCatalogState::Ready => Ok(CoreSchemaVisibility::Visible),
        CoreSchemaCatalogState::Missing(missing) => Ok(CoreSchemaVisibility::Lag(format!(
            "canonical tables are not visible: {}",
            missing.join(", ")
        ))),
        CoreSchemaCatalogState::Invalid(message) => Err(sqlx::Error::Protocol(message)),
    }
}

async fn wait_for_core_schema_visibility(
    settings: &MatrixOneSettings,
    fresh_database_bootstrap: bool,
) -> Result<(), sqlx::Error> {
    let mut verify_settings = settings.clone();
    verify_settings.db_pool_max_connections = 1;
    verify_settings.db_pool_min_connections = 0;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let connect_verify_pool = if fresh_database_bootstrap {
            connect_newly_created_matrixone_database(&verify_settings).await
        } else {
            connect_matrixone(&verify_settings).await
        };
        let verify_pool = BootstrapPool::new(connect_verify_pool?, 1);
        let visibility_result =
            verify_core_schema_visible(verify_pool.pool(), &settings.database).await;
        let visibility_result =
            finish_bootstrap_operation(visibility_result, verify_pool.release().await);
        match visibility_result {
            Ok(CoreSchemaVisibility::Visible) => return Ok(()),
            Ok(CoreSchemaVisibility::Lag(_)) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Ok(CoreSchemaVisibility::Lag(reason)) => {
                return Err(sqlx::Error::Protocol(format!(
                    "core schema contract was published but remained invisible to fresh connections: {reason}"
                )));
            }
            Err(error) => return Err(error),
        }
    }
}

fn unique_event_ids(event_ids: &[String]) -> Vec<&str> {
    let mut seen = HashSet::new();
    event_ids
        .iter()
        .map(String::as_str)
        .filter(|event_id| !event_id.trim().is_empty() && seen.insert(*event_id))
        .collect()
}

pub fn normalized_parent_event_ids(
    primary_parent_event_id: Option<&str>,
    parent_event_ids: Option<&[String]>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    if let Some(primary_parent_event_id) = primary_parent_event_id.map(str::trim)
        && !primary_parent_event_id.is_empty()
        && seen.insert(primary_parent_event_id.to_string())
    {
        out.push(primary_parent_event_id.to_string());
    }

    if let Some(parent_event_ids) = parent_event_ids {
        for parent_event_id in parent_event_ids {
            let parent_event_id = parent_event_id.trim();
            if parent_event_id.is_empty() {
                continue;
            }
            if seen.insert(parent_event_id.to_string()) {
                out.push(parent_event_id.to_string());
            }
        }
    }

    out
}

pub async fn agent_session_exists_for_user<'e, E>(
    executor: E,
    session_id: &str,
    user_id: &str,
) -> Result<bool, sqlx::Error>
where
    E: Executor<'e, Database = MySql>,
{
    let row =
        query("SELECT 1 AS owned FROM agent_sessions WHERE session_id = ? AND user_id = ? LIMIT 1")
            .bind(session_id)
            .bind(user_id)
            .fetch_optional(executor)
            .await?;
    Ok(row.is_some())
}

/// Admit one transaction that will append owner-scoped session child rows.
///
/// Existing sessions are locked before any child insert and deleting or
/// tombstoned identities fail closed. Callers that support offline/lazy roots
/// may create a missing parent, but only through the tombstone-gated canonical
/// upsert. The lock is held until the caller commits or rolls back.
pub async fn admit_session_event_write(
    tx: &mut sqlx::Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
    allow_lazy_create: bool,
) -> Result<(), sqlx::Error> {
    // The durable lifecycle fence is the first and shared lock for every
    // session-child write. A completed or pending delete is an authoritative
    // admission rejection, while storage and consistency failures retain
    // their original error identity.
    match lock_agent_session_write_fence(tx, session_id, user_id).await {
        Ok(()) => {}
        Err(sqlx::Error::Protocol(message))
            if message == "session has a durable deletion fence" =>
        {
            return Err(sqlx::Error::RowNotFound);
        }
        Err(error) => return Err(error),
    }
    let status: Option<String> = query_scalar(
        "SELECT status FROM agent_sessions
         WHERE user_id = ? AND session_id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(status) = status {
        if status == "deleting" {
            return Err(sqlx::Error::RowNotFound);
        }
        let tombstoned: Option<i32> = query_scalar(
            "SELECT 1 FROM session_deletion_tombstones
             WHERE user_id = ? AND session_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(user_id)
        .bind(session_id)
        .fetch_optional(&mut **tx)
        .await?;
        return if tombstoned.is_none() {
            Ok(())
        } else {
            Err(sqlx::Error::RowNotFound)
        };
    }
    if !allow_lazy_create {
        return Err(sqlx::Error::RowNotFound);
    }
    match add_agent_session_event_count_or_create(tx, session_id, user_id, 0, None).await {
        Ok(()) | Err(sqlx::Error::RowNotFound) => {}
        Err(error) => return Err(error),
    }
    let created: Option<i32> = query_scalar(
        "SELECT 1 FROM agent_sessions
         WHERE user_id = ? AND session_id = ? AND status <> 'deleting'
         LIMIT 1 FOR UPDATE",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await?;
    let tombstoned: Option<i32> = query_scalar(
        "SELECT 1 FROM session_deletion_tombstones
         WHERE user_id = ? AND session_id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await?;
    if created.is_some() && tombstoned.is_none() {
        Ok(())
    } else {
        Err(sqlx::Error::RowNotFound)
    }
}

/// Establish the only supported lock order for a transaction that mutates a
/// run and any session-scoped execution authority derived from it.
///
/// The caller supplies the session identity it already owns. We first fence
/// the active, non-tombstoned session, then the session execution slot, and
/// only then the exact run row. `allow_missing_run` is reserved for run
/// creation: it retains the session/slot fence while proving that an existing
/// exact run, when present, belongs to the same session.
pub async fn admit_session_scoped_run_write(
    tx: &mut sqlx::Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
    run_id: &str,
    allow_missing_run: bool,
) -> Result<bool, sqlx::Error> {
    admit_session_event_write(tx, session_id, user_id, false).await?;

    // Lock the derived slot before any run row. A missing slot is a valid
    // state; the SELECT still establishes the canonical access order for
    // engines that protect the key range on FOR UPDATE.
    let _: Option<String> = query_scalar(
        "SELECT run_id FROM agent_session_execution_slots
         WHERE user_id = ? AND session_id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await?;

    let run_exists: Option<i32> = query_scalar(
        "SELECT 1 FROM agent_runs
         WHERE user_id = ? AND session_id = ? AND run_id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await?;
    if run_exists.is_none() && !allow_missing_run {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(run_exists.is_some())
}

pub async fn agent_event_exists_for_user_session<'e, E>(
    executor: E,
    event_id: &str,
    session_id: &str,
    user_id: &str,
) -> Result<bool, sqlx::Error>
where
    E: Executor<'e, Database = MySql>,
{
    let row = query(
        "SELECT 1 AS owned FROM agent_events \
         WHERE event_id = ? AND session_id = ? AND user_id = ? LIMIT 1",
    )
    .bind(event_id)
    .bind(session_id)
    .bind(user_id)
    .fetch_optional(executor)
    .await?;
    Ok(row.is_some())
}

pub async fn bump_agent_session_event_count<'e, E>(
    executor: E,
    session_id: &str,
    user_id: &str,
    delta: i64,
    last_event_id: Option<&str>,
) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = MySql>,
{
    let result = if delta >= 0 {
        query(
            "UPDATE agent_sessions \
             SET event_count = event_count + ?, \
                 updated_at = IF(last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), updated_at), \
                 last_active_at = IF(last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), last_active_at), \
                 last_event_id = COALESCE(?, last_event_id) \
             WHERE session_id = ? AND user_id = ?",
        )
        .bind(delta)
        .bind(last_event_id)
        .bind(session_id)
        .bind(user_id)
        .execute(executor)
        .await?
    } else {
        let decrement = delta.saturating_abs();
        query(
            "UPDATE agent_sessions \
             SET event_count = CASE \
                     WHEN event_count >= ? THEN event_count - ? \
                     ELSE 0 \
                 END, \
                 updated_at = IF(last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), updated_at), \
                 last_active_at = IF(last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), last_active_at), \
                 last_event_id = COALESCE(?, last_event_id) \
             WHERE session_id = ? AND user_id = ?",
        )
        .bind(decrement)
        .bind(decrement)
        .bind(last_event_id)
        .bind(session_id)
        .bind(user_id)
        .execute(executor)
        .await?
    };
    if result.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

const ADD_AGENT_SESSION_EVENT_COUNT_OR_CREATE_SQL: &str = "INSERT INTO agent_sessions \
         (session_id, user_id, status, event_count, last_event_id, created_at, updated_at, last_active_at) \
         SELECT ?, ?, 'active', ?, ?, NOW(6), NOW(6), NOW(6) \
         FROM DUAL \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM session_deletion_tombstones \
             WHERE session_id = ? AND user_id = ? \
             LIMIT 1 \
         ) \
         ON DUPLICATE KEY UPDATE \
         event_count = event_count + VALUES(event_count), \
         last_event_id = COALESCE(VALUES(last_event_id), last_event_id), \
         updated_at = IF(last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), updated_at), \
         last_active_at = IF(last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), last_active_at)";

/// The state observed after locking a durable session lifecycle fence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentSessionWriteFenceState {
    Writable,
    PendingDelete,
    CompletedDelete,
    Missing,
}

/// Lock an existing durable lifecycle fence without creating one.
///
/// Maintenance uses this form to serialize with session-bound writers while
/// retaining the distinction between a delete still owned by reconciliation
/// and a completed durable tombstone.
pub async fn lock_existing_agent_session_write_fence(
    tx: &mut Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
) -> Result<AgentSessionWriteFenceState, sqlx::Error> {
    let row = query(
        "SELECT delete_requested_at, database_deleted_at \
         FROM agent_session_lifecycle_fences \
         WHERE user_id = ? AND session_id = ? FOR UPDATE",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(AgentSessionWriteFenceState::Missing);
    };
    let delete_requested_at: Option<chrono::NaiveDateTime> = row.try_get("delete_requested_at")?;
    let database_deleted_at: Option<chrono::NaiveDateTime> = row.try_get("database_deleted_at")?;
    Ok(match (delete_requested_at, database_deleted_at) {
        (None, None) => AgentSessionWriteFenceState::Writable,
        (Some(_), None) => AgentSessionWriteFenceState::PendingDelete,
        (_, Some(_)) => AgentSessionWriteFenceState::CompletedDelete,
    })
}

/// Lock a session fence for diagnostic expiry, claiming a fence-less session
/// only when its durable root no longer exists.
///
/// A historical prompt diagnostic can outlive both its session row and fence.
/// The claim is serialized with every session writer: if no root exists while
/// holding the fence, record a completed tombstone before deleting the
/// diagnostic so a later writer cannot recreate that session identity.
pub(crate) async fn lock_or_claim_orphaned_agent_session_write_fence(
    tx: &mut Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
) -> Result<AgentSessionWriteFenceState, sqlx::Error> {
    query(
        "INSERT IGNORE INTO agent_session_lifecycle_fences \
         (session_id, user_id, created_at, updated_at) \
         VALUES (?, ?, NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;

    match lock_existing_agent_session_write_fence(tx, session_id, user_id).await? {
        AgentSessionWriteFenceState::Writable => {
            let session_exists: Option<i32> = sqlx::query_scalar(
                "SELECT 1 FROM agent_sessions \
                 WHERE user_id = ? AND session_id = ? FOR UPDATE",
            )
            .bind(user_id)
            .bind(session_id)
            .fetch_optional(&mut **tx)
            .await?;
            if session_exists.is_some() {
                return Ok(AgentSessionWriteFenceState::Writable);
            }

            query(
                "UPDATE agent_session_lifecycle_fences \
                 SET delete_requested_at = COALESCE(delete_requested_at, NOW(6)), \
                     database_deleted_at = COALESCE(database_deleted_at, NOW(6)), \
                     updated_at = NOW(6) \
                 WHERE user_id = ? AND session_id = ?",
            )
            .bind(user_id)
            .bind(session_id)
            .execute(&mut **tx)
            .await?;
            lock_existing_agent_session_write_fence(tx, session_id, user_id).await
        }
        state => Ok(state),
    }
}

/// Acquire the durable lifecycle fence for one session-bound write.
///
/// The fence row is created before the session root and survives hard delete.
/// Every session-bound write must call this in the same transaction. A delete
/// request locks the same row before removing data, so an already-queued
/// writer either commits before the delete or observes the tombstone and rolls
/// its whole transaction back.
pub async fn lock_agent_session_write_fence(
    tx: &mut Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
) -> Result<(), sqlx::Error> {
    query(
        "INSERT IGNORE INTO agent_session_lifecycle_fences \
         (session_id, user_id, created_at, updated_at) \
         VALUES (?, ?, NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;

    match lock_existing_agent_session_write_fence(tx, session_id, user_id).await? {
        AgentSessionWriteFenceState::Writable => Ok(()),
        AgentSessionWriteFenceState::PendingDelete
        | AgentSessionWriteFenceState::CompletedDelete => Err(sqlx::Error::Protocol(
            "session has a durable deletion fence".into(),
        )),
        AgentSessionWriteFenceState::Missing => Err(sqlx::Error::Protocol(
            "session lifecycle fence disappeared before it could be locked".into(),
        )),
    }
}

pub async fn add_agent_session_event_count_or_create(
    tx: &mut Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
    delta: i64,
    last_event_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    if delta < 0 {
        return Err(sqlx::Error::Protocol(
            "add_agent_session_event_count_or_create requires a non-negative delta".into(),
        ));
    }

    lock_agent_session_write_fence(tx, session_id, user_id).await?;

    let result = query(ADD_AGENT_SESSION_EVENT_COUNT_OR_CREATE_SQL)
        .bind(session_id)
        .bind(user_id)
        .bind(delta)
        .bind(last_event_id)
        .bind(session_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    if result.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

pub async fn touch_agent_session_activity<'e, E>(
    executor: E,
    session_id: &str,
    user_id: &str,
    last_event_id: Option<&str>,
) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = MySql> + Copy,
{
    let result = query(
        "UPDATE agent_sessions \
         SET updated_at = IF(last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), updated_at), \
             last_active_at = IF(last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), last_active_at), \
             last_event_id = COALESCE(?, last_event_id) \
         WHERE session_id = ? AND user_id = ?",
    )
    .bind(last_event_id)
    .bind(session_id)
    .bind(user_id)
    .execute(executor)
    .await?;
    if result.rows_affected() == 0 {
        let exists = agent_session_exists_for_user(executor, session_id, user_id).await?;
        if !exists {
            return Err(sqlx::Error::RowNotFound);
        }
    }
    Ok(())
}

pub async fn insert_agent_event_edges<'e, E>(
    executor: E,
    user_id: &str,
    session_id: &str,
    child_event_id: &str,
    primary_parent_event_id: Option<&str>,
    parent_event_ids: &[String],
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = MySql>,
{
    let normalized = normalized_parent_event_ids(primary_parent_event_id, Some(parent_event_ids));
    if normalized.is_empty() {
        return Ok(());
    }

    let mut builder = QueryBuilder::<MySql>::new(
        "INSERT INTO agent_event_edges \
         (user_id, session_id, child_event_id, parent_event_id, relation_kind, parent_order) ",
    );
    builder.push_values(
        normalized.iter().enumerate(),
        |mut row, (idx, parent_event_id)| {
            row.push_bind(user_id)
                .push_bind(session_id)
                .push_bind(child_event_id)
                .push_bind(parent_event_id)
                .push_bind(CAUSAL_EDGE_KIND)
                .push_bind(i32::try_from(idx).unwrap_or(i32::MAX));
        },
    );
    builder.push(
        " ON DUPLICATE KEY UPDATE \
         session_id = VALUES(session_id), \
         parent_order = VALUES(parent_order)",
    );
    builder.build().execute(executor).await?;
    Ok(())
}

pub async fn load_agent_event_parent_ids<'e, E>(
    executor: E,
    user_id: &str,
    event_ids: &[String],
) -> Result<HashMap<String, Vec<String>>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = MySql>,
{
    let mut out = HashMap::new();
    let event_ids = unique_event_ids(event_ids);
    if event_ids.is_empty() {
        return Ok(out);
    }

    let mut builder = QueryBuilder::<MySql>::new(
        "SELECT child_event_id, parent_event_id \
         FROM agent_event_edges WHERE user_id = ",
    );
    builder.push_bind(user_id);
    builder.push(" AND relation_kind = ");
    builder.push_bind(CAUSAL_EDGE_KIND);
    builder.push(" AND child_event_id IN (");
    let mut separated = builder.separated(", ");
    for event_id in &event_ids {
        separated.push_bind(*event_id);
    }
    separated.push_unseparated(")");
    builder.push(" ORDER BY child_event_id ASC, parent_order ASC, parent_event_id ASC");

    let rows = builder.build().fetch_all(executor).await?;
    for row in rows {
        let child_event_id: String = row.try_get("child_event_id")?;
        let parent_event_id: String = row.try_get("parent_event_id")?;
        out.entry(child_event_id).or_default().push(parent_event_id);
    }
    Ok(out)
}

pub async fn delete_agent_event_edges_for_owned_event_ids<'e, E>(
    executor: E,
    owned_event_ids: &[(String, String)],
) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = MySql>,
{
    let owned_event_ids: Vec<(String, String)> = owned_event_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if owned_event_ids.is_empty() {
        return Ok(0);
    }

    let mut builder = QueryBuilder::<MySql>::new(
        "DELETE FROM agent_event_edges WHERE (user_id, child_event_id) IN (",
    );
    {
        let mut child_keys = builder.separated(", ");
        for (user_id, event_id) in &owned_event_ids {
            child_keys
                .push_unseparated("(")
                .push_bind(user_id)
                .push_unseparated(", ")
                .push_bind(event_id)
                .push_unseparated(")");
        }
        child_keys.push_unseparated(")");
    }
    builder.push(" OR (user_id, parent_event_id) IN (");
    {
        let mut parent_keys = builder.separated(", ");
        for (user_id, event_id) in &owned_event_ids {
            parent_keys
                .push_unseparated("(")
                .push_bind(user_id)
                .push_unseparated(", ")
                .push_bind(event_id)
                .push_unseparated(")");
        }
        parent_keys.push_unseparated(")");
    }

    let result = builder.build().execute(executor).await?;
    Ok(result.rows_affected())
}

/// When `ASTRA_AUTO_CREATE_DATABASE=1`, connect to `bootstrap_catalog` and
/// run `CREATE DATABASE IF NOT EXISTS` for [`MatrixOneSettings::database`] before normal DDL.
async fn ensure_matrixone_database_exists(
    settings: &MatrixOneSettings,
    bootstrap_catalog: &str,
) -> Result<(), sqlx::Error> {
    use std::error::Error;

    crate::snapshot_sql::validate_sql_identifier(&settings.database, "matrixone database")
        .map_err(|e| {
            sqlx::Error::Configuration(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e,
            )) as Box<dyn Error + Send + Sync>)
        })?;
    crate::snapshot_sql::validate_sql_identifier(bootstrap_catalog, "matrixone bootstrap catalog")
        .map_err(|e| {
            sqlx::Error::Configuration(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e,
            )) as Box<dyn Error + Send + Sync>)
        })?;
    let mut admin_settings = settings.clone();
    admin_settings.database = bootstrap_catalog.to_string();
    admin_settings.db_pool_max_connections = 1;
    admin_settings.db_pool_min_connections = 0;
    let admin_pool = BootstrapPool::new(connect_matrixone(&admin_settings).await?, 1);
    let ddl = format!(
        "CREATE DATABASE IF NOT EXISTS {}",
        crate::snapshot_sql::quote_mysql_identifier(&settings.database)
    );
    let create_result = query(&ddl).execute(admin_pool.pool()).await.map(|_| ());
    finish_bootstrap_operation(create_result, admin_pool.release().await)
}

const FRESH_DATABASE_VISIBILITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const FRESH_DATABASE_VISIBILITY_RETRY_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(100);

/// MatrixOne can acknowledge `CREATE DATABASE` before a subsequent new
/// connection can select that database.  Restrict recovery to this exact
/// vendor error: a fresh-database bootstrap must not turn unrelated startup
/// failures into retries.
fn is_fresh_database_visibility_error(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(database) = error else {
        return false;
    };
    database
        .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
        .is_some_and(|error| is_fresh_database_visibility_error_code(error.number()))
}

fn is_fresh_database_visibility_error_code(number: u16) -> bool {
    number == 1049
}

/// Connect to the target database after this process has just created it.
///
/// This is deliberately separate from ordinary connection setup: retrying is
/// valid only for the bounded metadata-visibility window after an explicit
/// auto-create request.  Returning the last exact error preserves operator
/// diagnostics if MatrixOne does not converge in time.
async fn connect_newly_created_matrixone_database(
    settings: &MatrixOneSettings,
) -> Result<sqlx::Pool<MySql>, sqlx::Error> {
    let deadline = tokio::time::Instant::now() + FRESH_DATABASE_VISIBILITY_TIMEOUT;
    loop {
        match connect_matrixone(settings).await {
            Ok(pool) => {
                // SQLx can construct a pool before it has selected the target
                // catalog. Force one bounded operation here so MatrixOne's
                // post-CREATE 1049 is handled by this fresh-only recovery
                // path rather than escaping later from schema bootstrap.
                let ready = sqlx::query_scalar::<_, i64>("SELECT 1")
                    .fetch_one(&pool)
                    .await
                    .map(|_| ());
                match ready {
                    Ok(()) => return Ok(pool),
                    Err(error) => {
                        let bootstrap_pool =
                            BootstrapPool::new(pool, settings.db_pool_max_connections as u64);
                        let error = finish_bootstrap_operation::<()>(
                            Err(error),
                            bootstrap_pool.release().await,
                        )
                        .expect_err("failed fresh database probe must remain an error");
                        if is_fresh_database_visibility_error(&error)
                            && tokio::time::Instant::now() < deadline
                        {
                            tokio::time::sleep(FRESH_DATABASE_VISIBILITY_RETRY_INTERVAL).await;
                            continue;
                        }
                        return Err(error);
                    }
                }
            }
            Err(error)
                if is_fresh_database_visibility_error(&error)
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(FRESH_DATABASE_VISIBILITY_RETRY_INTERVAL).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn validate_schema_identifier(raw: &str, kind: &str) -> Result<(), sqlx::Error> {
    use std::error::Error;

    crate::snapshot_sql::validate_sql_identifier(raw, kind).map_err(|e| {
        sqlx::Error::Configuration(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            e,
        )) as Box<dyn Error + Send + Sync>)
    })
}

const USER_IDENTITY_COLUMN_NAMES: [&str; 6] = [
    "user_id",
    "owner_user_id",
    "scope_user_id",
    "created_by",
    "updated_by",
    "username",
];

fn identity_column_widening_ddl(
    table: &str,
    column: &str,
    is_nullable: bool,
) -> Result<String, sqlx::Error> {
    validate_schema_identifier(table, "matrixone table")?;
    validate_schema_identifier(column, "matrixone column")?;
    let nullability = if is_nullable { "NULL" } else { "NOT NULL" };
    Ok(format!(
        "ALTER TABLE {} MODIFY COLUMN {} VARCHAR({USER_ID_MAX_LEN}) {nullability}",
        crate::snapshot_sql::quote_mysql_identifier(table),
        crate::snapshot_sql::quote_mysql_identifier(column),
    ))
}

/// Widens legacy identity columns to the repository-wide principal contract.
///
/// This is an explicit, idempotent schema migration. It never truncates data,
/// changes nullability, or rewrites UUID identifier columns.
async fn migrate_user_identity_column_widths(
    pool: &sqlx::Pool<MySql>,
    database: &str,
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    let rows = query(
        "SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH, \
                IS_NULLABLE, COLUMN_DEFAULT \
         FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = ? \
           AND COLUMN_NAME IN \
               ('user_id', 'owner_user_id', 'scope_user_id', 'created_by', 'updated_by', 'username') \
         ORDER BY TABLE_NAME, COLUMN_NAME",
    )
    .bind(database)
    .fetch_all(pool)
    .await?;

    for row in rows {
        let table: String = row.try_get("TABLE_NAME")?;
        let column: String = row.try_get("COLUMN_NAME")?;
        let data_type: String = row.try_get("DATA_TYPE")?;
        let width: Option<i64> = row.try_get("CHARACTER_MAXIMUM_LENGTH")?;
        let is_nullable: String = row.try_get("IS_NULLABLE")?;
        let default_value: Option<String> = row.try_get("COLUMN_DEFAULT")?;

        if !USER_IDENTITY_COLUMN_NAMES.contains(&column.as_str()) {
            return Err(sqlx::Error::Protocol(format!(
                "identity column migration selected unexpected column {table}.{column}"
            )));
        }
        if !data_type.eq_ignore_ascii_case("varchar") {
            return Err(sqlx::Error::Protocol(format!(
                "identity column {table}.{column} must be VARCHAR, found {data_type}"
            )));
        }
        let width = width.ok_or_else(|| {
            sqlx::Error::Protocol(format!(
                "identity column {table}.{column} has no bounded VARCHAR width"
            ))
        })?;
        if width < 0 {
            return Err(sqlx::Error::Protocol(format!(
                "identity column {table}.{column} has invalid width {width}"
            )));
        }
        if width >= USER_ID_MAX_LEN as i64 {
            continue;
        }
        if default_value.is_some() {
            return Err(sqlx::Error::Protocol(format!(
                "identity column {table}.{column} has a default value that must be preserved by an explicit migration"
            )));
        }
        let is_nullable = match is_nullable.as_str() {
            "YES" => true,
            "NO" => false,
            value => {
                return Err(sqlx::Error::Protocol(format!(
                    "identity column {table}.{column} has invalid IS_NULLABLE value {value}"
                )));
            }
        };
        let ddl = identity_column_widening_ddl(&table, &column, is_nullable)?;
        query(&ddl).execute(pool).await?;
        tracing::info!(
            table,
            column,
            previous_width = width,
            new_width = USER_ID_MAX_LEN,
            "widened legacy user identity column"
        );
    }

    Ok(())
}

async fn add_column_if_missing(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    column: &str,
    ddl: &str,
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    validate_schema_identifier(column, "matrixone column")?;

    let exists = query(
        "SELECT 1 FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = ? LIMIT 1",
    )
    .bind(database)
    .bind(table)
    .bind(column)
    .fetch_optional(pool)
    .await?
    .is_some();
    if exists {
        return Ok(());
    }

    query(ddl).execute(pool).await?;
    Ok(())
}

async fn add_index_if_missing(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    index: &str,
    ddl: &str,
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    validate_schema_identifier(index, "matrixone index")?;

    let exists = query(
        "SELECT 1 FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME = ? LIMIT 1",
    )
    .bind(database)
    .bind(table)
    .bind(index)
    .fetch_optional(pool)
    .await?
    .is_some();
    if exists {
        return Ok(());
    }

    query(ddl).execute(pool).await?;
    Ok(())
}

async fn existing_table_columns(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
) -> Result<BTreeSet<String>, sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    query(
        "SELECT COLUMN_NAME FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
    )
    .bind(database)
    .bind(table)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| row.try_get::<String, _>("COLUMN_NAME"))
    .collect::<Result<BTreeSet<_>, _>>()
}

async fn table_exists(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
) -> Result<bool, sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    query(
        "SELECT 1 FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? LIMIT 1",
    )
    .bind(database)
    .bind(table)
    .fetch_optional(pool)
    .await
    .map(|row| row.is_some())
}

fn agent_runs_canonical_columns() -> BTreeSet<String> {
    AGENT_RUNS_PRESERVED_COLUMNS
        .iter()
        .chain(AGENT_RUNS_RUNTIME_AUTHORITY_COLUMNS)
        .chain(AGENT_RUNS_WORK_BINDING_COLUMNS)
        .chain(AGENT_RUNS_WORK_ITEM_BINDING_COLUMNS)
        .map(|column| (*column).to_string())
        .collect()
}

async fn verify_work_canonical_schema(
    pool: &sqlx::Pool<MySql>,
    database: &str,
) -> Result<(), sqlx::Error> {
    if table_exists(pool, database, "work_item_attempt_settlements").await? {
        return Err(sqlx::Error::Protocol(
            "obsolete table work_item_attempt_settlements is not accepted by the canonical Work schema; use a fresh-schema cutover"
                .to_string(),
        ));
    }

    let required_columns: &[(&str, &[&str])] = &[
        (
            "work_check_runs",
            &["work_item_id", "work_item_revision", "work_item_attempt_id"],
        ),
        (
            "work_patch_materialization_operations",
            &[
                "executor_token",
                "executor_lease_expires_at",
                "recovery_after",
                "apply_invocation_ref",
                "observed_subject_revision",
                "apply_outcome",
                "failure_code",
                "verification_evidence_hash",
                "verification_outcome",
            ],
        ),
        (
            "work_patch_commit_operations",
            &[
                "active_target_branch_id",
                "provider_ref",
                "policy_decision_ref",
                "executor_token",
                "executor_lease_expires_at",
                "recovery_after",
                "commit_invocation_ref",
                "index_reconciled",
                "commit_author_name",
                "commit_author_email",
            ],
        ),
        ("work_items", &["last_revision"]),
        ("work_item_revisions", &["parent_revision"]),
        (
            "work_proposals",
            &[
                "item_change_count",
                "dependency_change_count",
                "criterion_count",
                "result_work_revision",
                "result_criteria_set_revision",
            ],
        ),
        (
            "work_branches",
            &["deletion_operation_id", "deletion_requested_at"],
        ),
        ("work_events", &["payload_hash"]),
        (
            "work_branch_control_operations",
            &[
                "forced_authorization_id",
                "handoff_id",
                "executor_token",
                "executor_lease_until",
            ],
        ),
        (
            "work_branch_creation_operations",
            &[
                "session_fork_id",
                "executor_token",
                "executor_lease_expires_at",
            ],
        ),
        ("work_criterion_sets", &["member_count"]),
        ("work_graph_revisions", &["item_count", "edge_count"]),
    ];
    let mut mismatches = Vec::new();
    for &(table, required) in required_columns {
        let observed = existing_table_columns(pool, database, table).await?;
        let missing = required
            .iter()
            .filter(|column| !observed.contains(**column))
            .copied()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            mismatches.push(format!("{table} missing ({})", missing.join(", ")));
        }
    }

    let required_indexes: &[(&str, &str, &[&str])] = &[
        (
            "work_branches",
            "idx_work_branches_owner_archive",
            &["owner_id", "work_id", "archived_at", "branch_id"],
        ),
        (
            "work_branches",
            "idx_work_branches_deletion",
            &["owner_id", "deletion_operation_id", "deletion_requested_at"],
        ),
        (
            "work_patch_materialization_operations",
            "idx_work_patch_materialization_recovery",
            &["operation_state", "recovery_after", "operation_id"],
        ),
        (
            "work_patch_materialization_operations",
            "idx_work_patch_materialization_source_history",
            &[
                "owner_id",
                "work_id",
                "target_branch_id",
                "source_branch_id",
                "created_at",
                "operation_id",
            ],
        ),
        (
            "work_branch_control_operations",
            "idx_work_branch_control_pending",
            &["operation_state", "created_at", "operation_id"],
        ),
        (
            "works",
            "idx_works_owner_created",
            &["owner_id", "created_at", "work_id"],
        ),
        (
            "work_check_runs",
            "idx_work_check_runs_item_attempt_time",
            &[
                "owner_id",
                "work_id",
                "branch_id",
                "work_item_id",
                "work_item_revision",
                "work_item_attempt_id",
                "produced_at",
                "check_run_id",
            ],
        ),
        (
            "work_check_runs",
            "idx_work_check_runs_branch_criterion_time",
            &[
                "owner_id",
                "work_id",
                "branch_id",
                "criterion_id",
                "criterion_revision",
                "produced_at",
                "check_run_id",
            ],
        ),
    ];
    for &(table, index, expected) in required_indexes {
        let observed = existing_index_columns(pool, database, table, index).await?;
        if !observed
            .iter()
            .map(String::as_str)
            .eq(expected.iter().copied())
        {
            mismatches.push(format!(
                "{table}.{index}=({}), expected ({})",
                observed.join(", "),
                expected.join(", ")
            ));
        }
    }

    if mismatches.is_empty() {
        return Ok(());
    }
    Err(sqlx::Error::Protocol(format!(
        "Work tables do not match the canonical schema; use a fresh-schema cutover: {}",
        mismatches.join("; ")
    )))
}

async fn verify_agent_runs_canonical_schema(
    pool: &sqlx::Pool<MySql>,
    database: &str,
) -> Result<(), sqlx::Error> {
    let observed_columns = existing_table_columns(pool, database, AGENT_RUNS_TABLE)
        .await?
        .into_iter()
        .filter(|column| !column.starts_with("__mo_"))
        .collect::<BTreeSet<_>>();
    let expected_columns = agent_runs_canonical_columns();
    let missing_columns = expected_columns
        .difference(&observed_columns)
        .cloned()
        .collect::<Vec<_>>();
    let unexpected_columns = observed_columns
        .difference(&expected_columns)
        .cloned()
        .collect::<Vec<_>>();

    let expected_indexes: &[(&str, &[&str])] = &[
        ("PRIMARY", &["user_id", "run_id"]),
        ("uq_agent_runs_run_id", &["run_id"]),
        (
            "idx_agent_runs_user_updated_run",
            &["user_id", "updated_at", "run_id"],
        ),
        (
            "idx_agent_runs_user_session_status_updated",
            &["user_id", "session_id", "status", "updated_at"],
        ),
        (
            "idx_agent_runs_owner_root_depth",
            &["user_id", "root_run_id", "depth", "created_at"],
        ),
        (
            "idx_agent_runs_owner_parent_status_updated",
            &["user_id", "parent_run_id", "status", "updated_at"],
        ),
        ("idx_agent_runs_owner_retry_of", &["user_id", "retry_of"]),
        (
            "idx_agent_runs_owner_lease",
            &["owner_pod_id", "owner_lease_expires_at"],
        ),
        (
            "idx_agent_runs_binding",
            &["agent_binding_id", "created_at"],
        ),
        (
            "idx_agent_runs_model_offering",
            &["model_offering_id", "created_at"],
        ),
        (
            "idx_agent_runs_owner_work_branch_created",
            &[
                "user_id",
                "work_id",
                "work_branch_id",
                "created_at",
                "run_id",
            ],
        ),
        (
            "idx_agent_runs_owner_work_item_root_latest",
            &[
                "user_id",
                "work_id",
                "work_branch_id",
                "work_item_id",
                "work_item_revision",
                "parent_run_id",
                "created_at",
                "run_id",
            ],
        ),
    ];
    let mut index_mismatches = Vec::new();
    for &(name, expected_columns) in expected_indexes {
        let observed = existing_index_columns(pool, database, AGENT_RUNS_TABLE, name).await?;
        if !observed
            .iter()
            .map(String::as_str)
            .eq(expected_columns.iter().copied())
        {
            index_mismatches.push(format!(
                "{name}=({}), expected ({})",
                observed.join(", "),
                expected_columns.join(", ")
            ));
        }
    }

    let observed_index_names = query(
        "SELECT DISTINCT INDEX_NAME FROM information_schema.STATISTICS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
    )
    .bind(database)
    .bind(AGENT_RUNS_TABLE)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| row.try_get::<String, _>("INDEX_NAME"))
    .collect::<Result<BTreeSet<_>, _>>()?;
    let expected_index_names = expected_indexes
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect::<BTreeSet<_>>();
    let unexpected_indexes = observed_index_names
        .difference(&expected_index_names)
        .cloned()
        .collect::<Vec<_>>();

    if missing_columns.is_empty()
        && unexpected_columns.is_empty()
        && index_mismatches.is_empty()
        && unexpected_indexes.is_empty()
    {
        return Ok(());
    }
    Err(sqlx::Error::Protocol(format!(
        "agent_runs does not match the canonical runtime schema; use a fresh-schema cutover (missing columns: {}; unexpected columns: {}; index mismatches: {}; unexpected indexes: {})",
        missing_columns.join(", "),
        unexpected_columns.join(", "),
        index_mismatches.join("; "),
        unexpected_indexes.join(", ")
    )))
}

async fn fail_if_obsolete_shape(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    required_columns: &[&str],
    obsolete_columns: &[&str],
    obsolete_indexes: &[&str],
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    for column in required_columns {
        validate_schema_identifier(column, "matrixone column")?;
    }
    for column in obsolete_columns {
        validate_schema_identifier(column, "matrixone column")?;
    }
    for index in obsolete_indexes {
        validate_schema_identifier(index, "matrixone index")?;
    }

    let columns = existing_table_columns(pool, database, table).await?;
    if columns.is_empty() {
        return Ok(());
    }

    let index_rows = query(
        "SELECT DISTINCT INDEX_NAME FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
    )
    .bind(database)
    .bind(table)
    .fetch_all(pool)
    .await?;
    let indexes = index_rows
        .into_iter()
        .map(|row| row.try_get::<String, _>("INDEX_NAME"))
        .collect::<Result<BTreeSet<_>, _>>()?;

    let mut reasons = Vec::new();
    for column in required_columns {
        if !columns.contains(*column) {
            reasons.push(format!("missing column {column}"));
        }
    }
    for column in obsolete_columns {
        if columns.contains(*column) {
            reasons.push(format!("obsolete column {column}"));
        }
    }
    for index in obsolete_indexes {
        if indexes.contains(*index) {
            reasons.push(format!("obsolete index {index}"));
        }
    }
    if reasons.is_empty() {
        return Ok(());
    }

    Err(sqlx::Error::Protocol(format!(
        "obsolete core schema table {table} requires manual migration before startup: {}",
        reasons.join(", ")
    )))
}

fn is_legacy_edge_pending_dispatch_shape(
    columns: &BTreeSet<String>,
    primary_key: &[String],
) -> bool {
    EDGE_PENDING_DISPATCH_LEGACY_COLUMNS
        .iter()
        .all(|column| columns.contains(*column))
        && EDGE_PENDING_DISPATCH_IDENTITY_COLUMNS
            .iter()
            .skip(1)
            .take(3)
            .all(|column| !columns.contains(*column))
        && primary_key
            .iter()
            .map(String::as_str)
            .eq(EDGE_PENDING_DISPATCH_LEGACY_PRIMARY_KEY.iter().copied())
}

async fn migrate_legacy_edge_pending_dispatch_if_needed(
    pool: &sqlx::Pool<MySql>,
    database: &str,
) -> Result<(), sqlx::Error> {
    let columns = existing_table_columns(pool, database, "edge_pending_dispatch").await?;
    if columns.is_empty() {
        return Ok(());
    }
    let primary_key =
        existing_index_columns(pool, database, "edge_pending_dispatch", "PRIMARY").await?;
    if !is_legacy_edge_pending_dispatch_shape(&columns, &primary_key) {
        return Ok(());
    }
    if table_exists(pool, database, EDGE_PENDING_DISPATCH_LEGACY_ARCHIVE_TABLE).await? {
        return Err(sqlx::Error::Protocol(format!(
            "legacy edge_pending_dispatch migration archive {EDGE_PENDING_DISPATCH_LEGACY_ARCHIVE_TABLE} already exists; inspect the previous migration before startup"
        )));
    }

    let active_row = query(
        "SELECT 1 AS active_row FROM edge_pending_dispatch \
         WHERE status IN ('pending', 'dispatched') LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    if active_row.is_some() {
        return Err(sqlx::Error::Protocol(
            "legacy edge_pending_dispatch contains active rows without session/run/turn identity; drain them with the pre-turn-scoped Astra release before upgrade"
                .to_string(),
        ));
    }
    query(&format!(
        "RENAME TABLE {} TO {}",
        crate::snapshot_sql::quote_mysql_identifier("edge_pending_dispatch"),
        crate::snapshot_sql::quote_mysql_identifier(EDGE_PENDING_DISPATCH_LEGACY_ARCHIVE_TABLE),
    ))
    .execute(pool)
    .await?;
    tracing::info!(
        legacy_table = "edge_pending_dispatch",
        archive_table = EDGE_PENDING_DISPATCH_LEGACY_ARCHIVE_TABLE,
        "archived terminal legacy edge dispatch rows before turn-scoped schema creation"
    );
    Ok(())
}

async fn fail_if_required_columns_missing_or_nullable(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    required_not_null_columns: &[&str],
) -> Result<(), sqlx::Error> {
    let requirements = required_not_null_columns
        .iter()
        .map(|column| (*column, ColumnNullability::NotNull))
        .collect::<Vec<_>>();
    fail_if_required_column_nullability_mismatches(pool, database, table, &requirements).await
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObservedColumnShape {
    data_type: String,
    character_maximum_length: Option<i64>,
    nullable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObservedIndexShape {
    columns: Vec<String>,
    non_unique: bool,
}

fn inference_provider_attempt_schema_mismatches(
    columns: &BTreeMap<String, ObservedColumnShape>,
    indexes: &BTreeMap<String, ObservedIndexShape>,
) -> Vec<String> {
    let mut reasons = Vec::new();
    for (name, expected_type, expected_width) in [
        ("admission_token", "char", Some(32_i64)),
        ("provider_protocol", "varchar", Some(32_i64)),
        ("provider_wire_hash", "char", Some(64_i64)),
        ("provider_wire_bytes", "bigint", None),
        ("usage_status", "varchar", Some(32_i64)),
    ] {
        let Some(column) = columns.get(name) else {
            reasons.push(format!("missing NOT NULL column {name}"));
            continue;
        };
        if column.nullable {
            reasons.push(format!("nullable column {name}"));
        }
        if !column.data_type.eq_ignore_ascii_case(expected_type) {
            reasons.push(format!(
                "column {name} has type {}, expected {expected_type}",
                column.data_type
            ));
        }
        if let Some(expected_width) = expected_width
            && column.character_maximum_length != Some(expected_width)
        {
            reasons.push(format!(
                "column {name} has width {:?}, expected {expected_width}",
                column.character_maximum_length
            ));
        }
    }

    match columns.get("context_expired_at") {
        Some(column) if column.nullable && column.data_type.eq_ignore_ascii_case("datetime") => {}
        Some(column) if !column.nullable => {
            reasons.push("non-nullable column context_expired_at".to_string());
        }
        Some(column) => reasons.push(format!(
            "column context_expired_at has type {}, expected datetime",
            column.data_type
        )),
        None => reasons.push("missing nullable column context_expired_at".to_string()),
    }

    for (name, expected_type, expected_width) in [
        ("canonical_transition_id", "char", Some(64_i64)),
        ("canonical_parent_transition_id", "char", Some(64_i64)),
        ("canonical_transition_hash", "char", Some(64_i64)),
    ] {
        let Some(column) = columns.get(name) else {
            reasons.push(format!("missing nullable column {name}"));
            continue;
        };
        if !column.nullable {
            reasons.push(format!("non-nullable column {name}"));
        }
        if !column.data_type.eq_ignore_ascii_case(expected_type) {
            reasons.push(format!(
                "column {name} has type {}, expected {expected_type}",
                column.data_type
            ));
        }
        if let Some(expected_width) = expected_width
            && column.character_maximum_length != Some(expected_width)
        {
            reasons.push(format!(
                "column {name} has width {:?}, expected {expected_width}",
                column.character_maximum_length
            ));
        }
    }

    for (name, expected_columns) in [
        ("PRIMARY", &["user_id", "attempt_id"][..]),
        (
            "uq_inference_provider_attempt",
            &["user_id", "invocation_id", "attempt_index"][..],
        ),
    ] {
        let Some(index) = indexes.get(name) else {
            reasons.push(format!("missing unique constraint {name}"));
            continue;
        };
        if index.non_unique {
            reasons.push(format!("constraint {name} is not unique"));
        }
        if !index
            .columns
            .iter()
            .map(String::as_str)
            .eq(expected_columns.iter().copied())
        {
            reasons.push(format!(
                "constraint {name} has columns ({}), expected ({})",
                index.columns.join(", "),
                expected_columns.join(", ")
            ));
        }
    }
    reasons
}

fn inference_canonical_transition_head_schema_mismatches(
    columns: &BTreeMap<String, ObservedColumnShape>,
    indexes: &BTreeMap<String, ObservedIndexShape>,
) -> Vec<String> {
    let mut reasons = Vec::new();
    for (name, expected_type, expected_width) in [
        ("user_id", "varchar", Some(128_i64)),
        ("session_id", "varchar", Some(64_i64)),
        ("turn_index", "bigint", None),
        ("head_transition_id", "char", Some(64_i64)),
        ("head_attempt_id", "varchar", Some(64_i64)),
        ("head_result_count", "bigint", None),
        ("head_result_root_hash", "char", Some(64_i64)),
        ("chain_length", "bigint", None),
        ("chain_payload_bytes", "bigint", None),
        ("updated_at", "datetime", None),
    ] {
        let Some(column) = columns.get(name) else {
            reasons.push(format!("missing NOT NULL column {name}"));
            continue;
        };
        if column.nullable {
            reasons.push(format!("nullable column {name}"));
        }
        if !column.data_type.eq_ignore_ascii_case(expected_type) {
            reasons.push(format!(
                "column {name} has type {}, expected {expected_type}",
                column.data_type
            ));
        }
        if let Some(expected_width) = expected_width
            && column.character_maximum_length != Some(expected_width)
        {
            reasons.push(format!(
                "column {name} has width {:?}, expected {expected_width}",
                column.character_maximum_length
            ));
        }
    }
    for (name, expected_columns) in [
        ("PRIMARY", &["user_id", "session_id", "turn_index"][..]),
        (
            "uq_inference_canonical_head_attempt",
            &["user_id", "head_attempt_id"][..],
        ),
    ] {
        let Some(index) = indexes.get(name) else {
            reasons.push(format!("missing unique constraint {name}"));
            continue;
        };
        if index.non_unique {
            reasons.push(format!("constraint {name} is not unique"));
        }
        if !index
            .columns
            .iter()
            .map(String::as_str)
            .eq(expected_columns.iter().copied())
        {
            reasons.push(format!(
                "constraint {name} has columns ({}), expected ({})",
                index.columns.join(", "),
                expected_columns.join(", ")
            ));
        }
    }
    reasons
}

fn inference_canonical_transition_wal_schema_mismatches(
    columns: &BTreeMap<String, ObservedColumnShape>,
    indexes: &BTreeMap<String, ObservedIndexShape>,
) -> Vec<String> {
    let mut reasons = Vec::new();
    for (name, expected_type, expected_width, nullable) in [
        ("user_id", "varchar", Some(128_i64), false),
        ("session_id", "varchar", Some(64_i64), false),
        ("turn_index", "bigint", None, false),
        ("round_index", "bigint", None, false),
        ("logical_attempt", "bigint", None, false),
        ("physical_attempt", "bigint", None, false),
        ("transition_id", "char", Some(64_i64), false),
        ("parent_transition_id", "char", Some(64_i64), true),
        ("attempt_id", "varchar", Some(64_i64), false),
        ("payload_hash", "char", Some(64_i64), false),
        ("payload_bytes", "bigint", None, false),
        ("predecessor_count", "bigint", None, false),
        ("predecessor_root_hash", "char", Some(64_i64), false),
        ("result_count", "bigint", None, false),
        ("result_root_hash", "char", Some(64_i64), false),
        ("recovery_mode", "varchar", Some(32_i64), false),
        ("created_at", "datetime", None, false),
    ] {
        let Some(column) = columns.get(name) else {
            reasons.push(format!("missing column {name}"));
            continue;
        };
        if column.nullable != nullable {
            reasons.push(format!(
                "column {name} has nullable={}, expected {nullable}",
                column.nullable
            ));
        }
        if !column.data_type.eq_ignore_ascii_case(expected_type) {
            reasons.push(format!(
                "column {name} has type {}, expected {expected_type}",
                column.data_type
            ));
        }
        if let Some(expected_width) = expected_width
            && column.character_maximum_length != Some(expected_width)
        {
            reasons.push(format!(
                "column {name} has width {:?}, expected {expected_width}",
                column.character_maximum_length
            ));
        }
    }
    match columns.get("payload_json") {
        Some(column)
            if !column.nullable
                && (column.data_type.eq_ignore_ascii_case("text")
                    || column.data_type.eq_ignore_ascii_case("longtext")) => {}
        Some(column) if column.nullable => {
            reasons.push("nullable column payload_json".to_string());
        }
        Some(column) => reasons.push(format!(
            "column payload_json has type {}, expected text-compatible storage",
            column.data_type
        )),
        None => reasons.push("missing NOT NULL column payload_json".to_string()),
    }
    for (name, expected_columns, expected_non_unique) in [
        (
            "PRIMARY",
            &["user_id", "session_id", "turn_index", "transition_id"][..],
            false,
        ),
        (
            "uq_inference_canonical_wal_attempt",
            &["user_id", "attempt_id"][..],
            false,
        ),
        (
            "idx_inference_canonical_wal_parent",
            &[
                "user_id",
                "session_id",
                "turn_index",
                "parent_transition_id",
            ][..],
            true,
        ),
    ] {
        let Some(index) = indexes.get(name) else {
            reasons.push(format!("missing index {name}"));
            continue;
        };
        if index.non_unique != expected_non_unique {
            reasons.push(format!(
                "index {name} has non_unique={}, expected {expected_non_unique}",
                index.non_unique
            ));
        }
        if !index
            .columns
            .iter()
            .map(String::as_str)
            .eq(expected_columns.iter().copied())
        {
            reasons.push(format!(
                "index {name} has columns ({}), expected ({})",
                index.columns.join(", "),
                expected_columns.join(", ")
            ));
        }
    }
    reasons
}

fn inference_invocation_schema_mismatches(
    columns: &BTreeMap<String, ObservedColumnShape>,
) -> Vec<String> {
    let mut reasons = Vec::new();
    for (name, expected_type, expected_width) in [
        ("admission_token", "char", Some(32_i64)),
        ("owner_token", "char", Some(32_i64)),
        ("owner_generation", "bigint", None),
        ("owner_lease_expires_at", "datetime", None),
        ("usage_status", "varchar", Some(32_i64)),
        ("provider_delivery_state", "varchar", Some(32_i64)),
    ] {
        let Some(column) = columns.get(name) else {
            reasons.push(format!("missing NOT NULL column {name}"));
            continue;
        };
        if column.nullable {
            reasons.push(format!("nullable column {name}"));
        }
        if !column.data_type.eq_ignore_ascii_case(expected_type) {
            reasons.push(format!(
                "column {name} has type {}, expected {expected_type}",
                column.data_type
            ));
        }
        if let Some(expected_width) = expected_width
            && column.character_maximum_length != Some(expected_width)
        {
            reasons.push(format!(
                "column {name} has width {:?}, expected {expected_width}",
                column.character_maximum_length
            ));
        }
    }
    reasons
}

async fn verify_inference_invocation_schema_contract(
    pool: &sqlx::Pool<MySql>,
    database: &str,
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    let table = "inference_invocations";
    let rows = query(
        "SELECT COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH, IS_NULLABLE
         FROM information_schema.COLUMNS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
           AND COLUMN_NAME IN ('admission_token', 'owner_token', 'owner_generation',
                               'owner_lease_expires_at', 'usage_status',
                               'provider_delivery_state')",
    )
    .bind(database)
    .bind(table)
    .fetch_all(pool)
    .await?;
    let mut columns = BTreeMap::new();
    for row in rows {
        let name: String = row.try_get("COLUMN_NAME")?;
        let nullable = match row.try_get::<String, _>("IS_NULLABLE")?.as_str() {
            "YES" => true,
            "NO" => false,
            value => {
                return Err(sqlx::Error::Protocol(format!(
                    "schema column {table}.{name} has invalid IS_NULLABLE value {value}"
                )));
            }
        };
        columns.insert(
            name,
            ObservedColumnShape {
                data_type: row.try_get("DATA_TYPE")?,
                character_maximum_length: row.try_get("CHARACTER_MAXIMUM_LENGTH")?,
                nullable,
            },
        );
    }
    let reasons = inference_invocation_schema_mismatches(&columns);
    if reasons.is_empty() {
        return Ok(());
    }
    Err(sqlx::Error::Protocol(format!(
        "obsolete core schema table {table} requires manual migration before startup: {}",
        reasons.join(", ")
    )))
}

async fn verify_inference_provider_attempt_schema_contract(
    pool: &sqlx::Pool<MySql>,
    database: &str,
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    let table = "inference_provider_attempts";
    let column_rows = query(
        "SELECT COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH, IS_NULLABLE
         FROM information_schema.COLUMNS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
    )
    .bind(database)
    .bind(table)
    .fetch_all(pool)
    .await?;
    let mut columns = BTreeMap::new();
    for row in column_rows {
        let name: String = row.try_get("COLUMN_NAME")?;
        let nullable = match row.try_get::<String, _>("IS_NULLABLE")?.as_str() {
            "YES" => true,
            "NO" => false,
            value => {
                return Err(sqlx::Error::Protocol(format!(
                    "schema column {table}.{name} has invalid IS_NULLABLE value {value}"
                )));
            }
        };
        columns.insert(
            name,
            ObservedColumnShape {
                data_type: row.try_get("DATA_TYPE")?,
                character_maximum_length: row.try_get("CHARACTER_MAXIMUM_LENGTH")?,
                nullable,
            },
        );
    }

    let index_rows = query(
        "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME
         FROM information_schema.STATISTICS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
           AND INDEX_NAME IN ('PRIMARY', 'uq_inference_provider_attempt')
         ORDER BY INDEX_NAME, SEQ_IN_INDEX",
    )
    .bind(database)
    .bind(table)
    .fetch_all(pool)
    .await?;
    let mut indexes = BTreeMap::<String, ObservedIndexShape>::new();
    for row in index_rows {
        let name: String = row.try_get("INDEX_NAME")?;
        let non_unique = match row.try_get::<i64, _>("NON_UNIQUE")? {
            0 => false,
            1 => true,
            value => {
                return Err(sqlx::Error::Protocol(format!(
                    "schema constraint {table}.{name} has invalid NON_UNIQUE value {value}"
                )));
            }
        };
        let column: String = row.try_get("COLUMN_NAME")?;
        let index = indexes
            .entry(name.clone())
            .or_insert_with(|| ObservedIndexShape {
                columns: Vec::new(),
                non_unique,
            });
        if index.non_unique != non_unique {
            return Err(sqlx::Error::Protocol(format!(
                "schema constraint {table}.{name} reports inconsistent uniqueness"
            )));
        }
        index.columns.push(column);
    }

    let reasons = inference_provider_attempt_schema_mismatches(&columns, &indexes);
    if reasons.is_empty() {
        return Ok(());
    }
    Err(sqlx::Error::Protocol(format!(
        "obsolete core schema table {table} requires manual migration before startup: {}",
        reasons.join(", ")
    )))
}

async fn verify_inference_canonical_transition_head_schema_contract(
    pool: &sqlx::Pool<MySql>,
    database: &str,
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    let table = "inference_canonical_transition_heads";
    let column_rows = query(
        "SELECT COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH, IS_NULLABLE
         FROM information_schema.COLUMNS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
    )
    .bind(database)
    .bind(table)
    .fetch_all(pool)
    .await?;
    let mut columns = BTreeMap::new();
    for row in column_rows {
        let name: String = row.try_get("COLUMN_NAME")?;
        let nullable = match row.try_get::<String, _>("IS_NULLABLE")?.as_str() {
            "YES" => true,
            "NO" => false,
            value => {
                return Err(sqlx::Error::Protocol(format!(
                    "schema column {table}.{name} has invalid IS_NULLABLE value {value}"
                )));
            }
        };
        columns.insert(
            name,
            ObservedColumnShape {
                data_type: row.try_get("DATA_TYPE")?,
                character_maximum_length: row.try_get("CHARACTER_MAXIMUM_LENGTH")?,
                nullable,
            },
        );
    }
    let index_rows = query(
        "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME
         FROM information_schema.STATISTICS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
           AND INDEX_NAME IN ('PRIMARY', 'uq_inference_canonical_head_attempt')
         ORDER BY INDEX_NAME, SEQ_IN_INDEX",
    )
    .bind(database)
    .bind(table)
    .fetch_all(pool)
    .await?;
    let mut indexes = BTreeMap::<String, ObservedIndexShape>::new();
    for row in index_rows {
        let name: String = row.try_get("INDEX_NAME")?;
        let non_unique = match row.try_get::<i64, _>("NON_UNIQUE")? {
            0 => false,
            1 => true,
            value => {
                return Err(sqlx::Error::Protocol(format!(
                    "schema constraint {table}.{name} has invalid NON_UNIQUE value {value}"
                )));
            }
        };
        let column: String = row.try_get("COLUMN_NAME")?;
        let index = indexes
            .entry(name.clone())
            .or_insert_with(|| ObservedIndexShape {
                columns: Vec::new(),
                non_unique,
            });
        if index.non_unique != non_unique {
            return Err(sqlx::Error::Protocol(format!(
                "schema constraint {table}.{name} reports inconsistent uniqueness"
            )));
        }
        index.columns.push(column);
    }
    let reasons = inference_canonical_transition_head_schema_mismatches(&columns, &indexes);
    if reasons.is_empty() {
        return Ok(());
    }
    Err(sqlx::Error::Protocol(format!(
        "obsolete core schema table {table} requires manual migration before startup: {}",
        reasons.join(", ")
    )))
}

async fn verify_inference_canonical_transition_wal_schema_contract(
    pool: &sqlx::Pool<MySql>,
    database: &str,
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    let table = "inference_canonical_transition_wal";
    let column_rows = query(
        "SELECT COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH, IS_NULLABLE
         FROM information_schema.COLUMNS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
    )
    .bind(database)
    .bind(table)
    .fetch_all(pool)
    .await?;
    let mut columns = BTreeMap::new();
    for row in column_rows {
        let name: String = row.try_get("COLUMN_NAME")?;
        let nullable = match row.try_get::<String, _>("IS_NULLABLE")?.as_str() {
            "YES" => true,
            "NO" => false,
            value => {
                return Err(sqlx::Error::Protocol(format!(
                    "schema column {table}.{name} has invalid IS_NULLABLE value {value}"
                )));
            }
        };
        columns.insert(
            name,
            ObservedColumnShape {
                data_type: row.try_get("DATA_TYPE")?,
                character_maximum_length: row.try_get("CHARACTER_MAXIMUM_LENGTH")?,
                nullable,
            },
        );
    }
    let index_rows = query(
        "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME
         FROM information_schema.STATISTICS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
           AND INDEX_NAME IN ('PRIMARY', 'uq_inference_canonical_wal_attempt',
                              'idx_inference_canonical_wal_parent')
         ORDER BY INDEX_NAME, SEQ_IN_INDEX",
    )
    .bind(database)
    .bind(table)
    .fetch_all(pool)
    .await?;
    let mut indexes = BTreeMap::<String, ObservedIndexShape>::new();
    for row in index_rows {
        let name: String = row.try_get("INDEX_NAME")?;
        let non_unique = match row.try_get::<i64, _>("NON_UNIQUE")? {
            0 => false,
            1 => true,
            value => {
                return Err(sqlx::Error::Protocol(format!(
                    "schema constraint {table}.{name} has invalid NON_UNIQUE value {value}"
                )));
            }
        };
        let column: String = row.try_get("COLUMN_NAME")?;
        let index = indexes
            .entry(name.clone())
            .or_insert_with(|| ObservedIndexShape {
                columns: Vec::new(),
                non_unique,
            });
        if index.non_unique != non_unique {
            return Err(sqlx::Error::Protocol(format!(
                "schema constraint {table}.{name} reports inconsistent uniqueness"
            )));
        }
        index.columns.push(column);
    }
    let reasons = inference_canonical_transition_wal_schema_mismatches(&columns, &indexes);
    if reasons.is_empty() {
        return Ok(());
    }
    Err(sqlx::Error::Protocol(format!(
        "obsolete core schema table {table} requires manual migration before startup: {}",
        reasons.join(", ")
    )))
}

async fn fail_if_required_columns_missing_or_not_nullable(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    required_nullable_columns: &[&str],
) -> Result<(), sqlx::Error> {
    let requirements = required_nullable_columns
        .iter()
        .map(|column| (*column, ColumnNullability::Nullable))
        .collect::<Vec<_>>();
    fail_if_required_column_nullability_mismatches(pool, database, table, &requirements).await
}

#[derive(Clone, Copy)]
enum ColumnNullability {
    NotNull,
    Nullable,
}

async fn fail_if_required_column_nullability_mismatches(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    requirements: &[(&str, ColumnNullability)],
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    for (column, _) in requirements {
        validate_schema_identifier(column, "matrixone column")?;
    }

    let rows = query(
        "SELECT COLUMN_NAME, IS_NULLABLE FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
    )
    .bind(database)
    .bind(table)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }

    let mut columns = BTreeMap::new();
    for row in rows {
        let column: String = row.try_get("COLUMN_NAME")?;
        let is_nullable: String = row.try_get("IS_NULLABLE")?;
        columns.insert(column, is_nullable == "YES");
    }
    let mut reasons = Vec::new();
    for (column, required) in requirements {
        match (columns.get(*column), required) {
            (None, ColumnNullability::NotNull) => {
                reasons.push(format!("missing NOT NULL column {column}"));
            }
            (None, ColumnNullability::Nullable) => {
                reasons.push(format!("missing nullable column {column}"));
            }
            (Some(true), ColumnNullability::NotNull) => {
                reasons.push(format!("nullable owner column {column}"));
            }
            (Some(false), ColumnNullability::Nullable) => {
                reasons.push(format!("non-nullable column {column}"));
            }
            (Some(false), ColumnNullability::NotNull)
            | (Some(true), ColumnNullability::Nullable) => {}
        }
    }
    if reasons.is_empty() {
        return Ok(());
    }
    Err(sqlx::Error::Protocol(format!(
        "obsolete core schema table {table} requires manual migration before startup: {}",
        reasons.join(", ")
    )))
}

async fn fail_if_varchar_columns_shorter_than(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    required_widths: &[(&str, u64)],
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    for (column, _) in required_widths {
        validate_schema_identifier(column, "matrixone column")?;
    }

    let rows = query(
        "SELECT COLUMN_NAME, CHARACTER_MAXIMUM_LENGTH FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
    )
    .bind(database)
    .bind(table)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }

    let required_widths = required_widths.iter().copied().collect::<HashMap<_, _>>();
    let mut reasons = Vec::new();
    for row in rows {
        let column: String = row.try_get("COLUMN_NAME")?;
        let Some(required_width) = required_widths.get(column.as_str()).copied() else {
            continue;
        };
        match row.try_get::<Option<i64>, _>("CHARACTER_MAXIMUM_LENGTH")? {
            Some(actual_width) if actual_width < 0 => reasons.push(format!(
                "column {column} has invalid negative width {actual_width}"
            )),
            Some(actual_width) => {
                let actual_width = u64::try_from(actual_width).map_err(|_| {
                    sqlx::Error::Protocol(format!(
                        "column {column} width {actual_width} cannot be represented as u64"
                    ))
                })?;
                if actual_width < required_width {
                    reasons.push(format!(
                        "column {column} width {actual_width} below {required_width}"
                    ));
                }
            }
            None => reasons.push(format!("column {column} is not a bounded varchar")),
        }
    }

    if reasons.is_empty() {
        return Ok(());
    }

    Err(sqlx::Error::Protocol(format!(
        "obsolete core schema table {table} requires manual migration before startup: {}",
        reasons.join(", ")
    )))
}

async fn existing_index_columns(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    index: &str,
) -> Result<Vec<String>, sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    validate_schema_identifier(index, "matrixone index")?;

    let rows = query(
        "SELECT COLUMN_NAME FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME = ? \
         ORDER BY SEQ_IN_INDEX",
    )
    .bind(database)
    .bind(table)
    .bind(index)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| row.try_get::<String, _>("COLUMN_NAME"))
        .collect::<Result<Vec<_>, _>>()
}

async fn drop_index_if_present(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    index: &str,
) -> Result<(), sqlx::Error> {
    let columns = existing_index_columns(pool, database, table, index).await?;
    if columns.is_empty() {
        return Ok(());
    }
    let ddl = format!(
        "ALTER TABLE {} DROP INDEX {}",
        crate::snapshot_sql::quote_mysql_identifier(table),
        crate::snapshot_sql::quote_mysql_identifier(index)
    );
    query(&ddl).execute(pool).await?;
    Ok(())
}

async fn ensure_index_shape(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    index: &str,
    expected_columns: &[&str],
    ddl: &str,
) -> Result<(), sqlx::Error> {
    for column in expected_columns {
        validate_schema_identifier(column, "matrixone column")?;
    }
    let existing = existing_index_columns(pool, database, table, index).await?;
    if existing
        .iter()
        .map(String::as_str)
        .eq(expected_columns.iter().copied())
    {
        return Ok(());
    }
    if !existing.is_empty() {
        return Err(sqlx::Error::Protocol(format!(
            "index shape mismatch for {table}.{index}: existing ({}) != expected ({})",
            existing.join(", "),
            expected_columns.join(", ")
        )));
    }
    query(ddl).execute(pool).await?;
    Ok(())
}

async fn ensure_primary_key_shape(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    expected_columns: &[&str],
    ddl: &str,
) -> Result<(), sqlx::Error> {
    for column in expected_columns {
        validate_schema_identifier(column, "matrixone column")?;
    }
    let existing = existing_index_columns(pool, database, table, "PRIMARY").await?;
    if existing
        .iter()
        .map(String::as_str)
        .eq(expected_columns.iter().copied())
    {
        return Ok(());
    }
    if !existing.is_empty() {
        return Err(sqlx::Error::Protocol(format!(
            "primary key shape mismatch for {table}: existing ({}) != expected ({})",
            existing.join(", "),
            expected_columns.join(", ")
        )));
    }
    query(ddl).execute(pool).await?;
    Ok(())
}

pub async fn ensure_core_schema(
    settings: &MatrixOneSettings,
    bootstrap_catalog: &str,
) -> Result<(), sqlx::Error> {
    // Tests and startup paths can race on schema bootstrap inside the same process.
    // Serialize schema setup so version markers and DDL stay idempotent.
    let _init_guard = CORE_SCHEMA_INIT_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;

    let auto_create_database = std::env::var("ASTRA_AUTO_CREATE_DATABASE")
        .map(|v| v == "1")
        .unwrap_or(false);
    if auto_create_database {
        ensure_matrixone_database_exists(settings, bootstrap_catalog).await?;
    }
    let mut schema_settings = settings.clone();
    schema_settings.db_pool_max_connections = 1;
    schema_settings.db_pool_min_connections = 0;
    let connect_schema_pool = if auto_create_database {
        connect_newly_created_matrixone_database(&schema_settings).await
    } else {
        connect_matrixone(&schema_settings).await
    };
    let pool = BootstrapPool::new(connect_schema_pool?, 1);
    let connect_lease_pool = if auto_create_database {
        connect_newly_created_matrixone_database(&schema_settings).await
    } else {
        connect_matrixone(&schema_settings).await
    };
    let lease_pool = match connect_lease_pool {
        Ok(lease_pool) => BootstrapPool::new(lease_pool, 1),
        Err(error) => {
            return finish_bootstrap_operation(Err(error), pool.release().await);
        }
    };
    let database_lease = match CoreSchemaDatabaseLease::acquire(lease_pool.pool()).await {
        Ok(lease) => lease,
        Err(error) => {
            let lease_release = lease_pool.release().await;
            let pool_release = pool.release().await;
            return finish_bootstrap_operation(
                finish_bootstrap_operation(Err(error), lease_release),
                pool_release,
            );
        }
    };
    let schema_result =
        ensure_core_schema_while_leased(settings, pool.pool().clone(), database_lease.holder_id())
            .await;
    let release_result = database_lease.release().await;
    let lease_pool_release = lease_pool.release().await;
    let pool_release = pool.release().await;
    let bootstrap_result = match (schema_result, release_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(schema_error), Ok(())) => Err(schema_error),
        (Ok(()), Err(release_error)) => Err(release_error),
        (Err(schema_error), Err(release_error)) => Err(sqlx::Error::Protocol(format!(
            "core schema bootstrap failed: {schema_error}; bootstrap lease release also failed: {release_error}"
        ))),
    };
    let bootstrap_result = finish_bootstrap_operation(bootstrap_result, lease_pool_release);
    let bootstrap_result = finish_bootstrap_operation(bootstrap_result, pool_release);
    match bootstrap_result {
        Ok(()) => wait_for_core_schema_visibility(settings, auto_create_database).await,
        Err(error) => Err(error),
    }
}

async fn backfill_conversation_manifest_segments(
    pool: &sqlx::Pool<MySql>,
) -> Result<(), sqlx::Error> {
    const BACKFILL_BATCH: i64 = 128;
    loop {
        let rows = query(
            "SELECT isolation_domain, owner_user_id, session_id, branch_id,
                    manifest_root, CAST(manifest_json AS CHAR) AS manifest_json
               FROM conversation_manifest_nodes node
              WHERE NOT EXISTS (
                        SELECT 1 FROM conversation_manifest_segments segment
                         WHERE segment.isolation_domain = node.isolation_domain
                           AND segment.owner_user_id = node.owner_user_id
                           AND segment.session_id = node.session_id
                           AND segment.branch_id = node.branch_id
                           AND segment.manifest_root = node.manifest_root
                    )
              ORDER BY node.created_at ASC, node.manifest_root ASC
              LIMIT ?",
        )
        .bind(BACKFILL_BATCH)
        .fetch_all(pool)
        .await?;
        if rows.is_empty() {
            return Ok(());
        }

        for row in rows {
            let isolation_domain: String = row.try_get("isolation_domain")?;
            let owner_user_id: String = row.try_get("owner_user_id")?;
            let session_id: String = row.try_get("session_id")?;
            let branch_id: String = row.try_get("branch_id")?;
            let manifest_root: String = row.try_get("manifest_root")?;
            let manifest_json: String = row.try_get("manifest_json")?;
            let manifest: astra_turn_types::ContextManifestNodeV1 =
                serde_json::from_str(&manifest_json).map_err(|source| {
                    sqlx::Error::Protocol(format!(
                        "decode manifest segment backfill row {manifest_root}: {source}"
                    ))
                })?;
            manifest.validate().map_err(|source| {
                sqlx::Error::Protocol(format!(
                    "validate manifest segment backfill row {manifest_root}: {source}"
                ))
            })?;
            if manifest.key.isolation_domain != isolation_domain
                || manifest.key.owner_user_id != owner_user_id
                || manifest.key.session_id != session_id
                || manifest.key.branch_id != branch_id
                || manifest.manifest_root != manifest_root
            {
                return Err(sqlx::Error::Protocol(format!(
                    "manifest segment backfill identity mismatch for {manifest_root}"
                )));
            }

            // One manifest's reference index is installed atomically. If
            // bootstrap is interrupted, the next leased run sees either all
            // positions or none and can safely retry.
            let mut tx = pool.begin().await?;
            let mut builder = QueryBuilder::<MySql>::new(
                "INSERT IGNORE INTO conversation_manifest_segments
                 (isolation_domain, owner_user_id, session_id, branch_id,
                  manifest_root, segment_position, segment_hash) ",
            );
            builder.push_values(
                manifest.appended_segments.iter().enumerate(),
                |mut values, (position, segment)| {
                    values
                        .push_bind(&isolation_domain)
                        .push_bind(&owner_user_id)
                        .push_bind(&session_id)
                        .push_bind(&branch_id)
                        .push_bind(&manifest_root)
                        .push_bind(i64::try_from(position).unwrap_or(i64::MAX))
                        .push_bind(&segment.segment_hash);
                },
            );
            builder.build().execute(&mut *tx).await?;
            tx.commit().await?;
        }
    }
}

async fn ensure_core_schema_while_leased(
    settings: &MatrixOneSettings,
    pool: sqlx::Pool<MySql>,
    holder_id: &str,
) -> Result<(), sqlx::Error> {
    if core_schema_contract_is_current(&pool).await? {
        verify_core_schema_catalog(&pool, &settings.database).await?;
        verify_inference_invocation_schema_contract(&pool, &settings.database).await?;
        verify_inference_provider_attempt_schema_contract(&pool, &settings.database).await?;
        verify_inference_canonical_transition_head_schema_contract(&pool, &settings.database)
            .await?;
        return verify_inference_canonical_transition_wal_schema_contract(
            &pool,
            &settings.database,
        )
        .await;
    }

    // Existing deployments may still have UUID-sized identity columns. Widen
    // them before table-specific shape checks run so every persistence path
    // observes the same principal contract during startup.
    migrate_user_identity_column_widths(&pool, &settings.database).await?;

    // The executor observes the exact CREATE TABLE statements that bootstrap
    // executes. This makes DDL the declaration and the ownership catalog its
    // generated contract, rather than maintaining a second list of names.
    let pool = CoreSchemaExecutor::new(pool);
    for (table_name, ddl) in [
        ("astra_schema_contracts", CORE_SCHEMA_CONTRACT_TABLE_SQL),
        ("astra_schema_bootstrap_leases", CORE_SCHEMA_LEASE_TABLE_SQL),
        (
            "astra_schema_table_contracts",
            CORE_SCHEMA_TABLE_CONTRACT_SQL,
        ),
    ] {
        pool.authority.declare("storage", table_name, ddl);
    }

    // Auth
    core_schema_create!(
        pool,
        "auth_users",
        "CREATE TABLE IF NOT EXISTS auth_users (
            user_id VARCHAR(128) PRIMARY KEY,
            username VARCHAR(128) NOT NULL UNIQUE,
            email VARCHAR(255) NOT NULL UNIQUE,
            password_hash VARCHAR(255) NOT NULL,
            display_name VARCHAR(100) NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            last_login_at DATETIME(6) NULL
        )",
    )
    .execute(&pool)
    .await?;
    core_schema_create!(
        pool,
        "auth_roles",
        "CREATE TABLE IF NOT EXISTS auth_roles (
            role_id VARCHAR(64) PRIMARY KEY,
            role_name VARCHAR(50) NOT NULL UNIQUE,
            description VARCHAR(255) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "auth_user_roles",
        "CREATE TABLE IF NOT EXISTS auth_user_roles (
            user_id VARCHAR(128) NOT NULL,
            role_id VARCHAR(64) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, role_id),
            INDEX idx_auth_user_roles_role_id (role_id)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "auth_user_roles",
        &["user_id", "role_id"],
        &["id"],
        &["uq_auth_user_roles_user_role"],
    )
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "auth_user_roles",
        "idx_auth_user_roles_user_id",
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "auth_user_roles",
        &["user_id", "role_id"],
        "ALTER TABLE auth_user_roles ADD PRIMARY KEY (user_id, role_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "auth_user_roles",
        "idx_auth_user_roles_role_id",
        &["role_id"],
        "ALTER TABLE auth_user_roles ADD INDEX idx_auth_user_roles_role_id (role_id)",
    )
    .await?;
    core_schema_create!(
        pool,
        "auth_refresh_tokens",
        "CREATE TABLE IF NOT EXISTS auth_refresh_tokens (
            token_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NULL,
            token_hash VARCHAR(255) NOT NULL,
            token_prefix VARCHAR(16) NULL,
            expires_at DATETIME(6) NOT NULL,
            is_revoked SMALLINT NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_auth_refresh_tokens_hash (token_hash),
            INDEX idx_auth_refresh_tokens_session (session_id),
            INDEX idx_auth_refresh_tokens_user_expires (user_id, expires_at),
            INDEX idx_auth_refresh_tokens_expires_at (expires_at),
            INDEX idx_auth_refresh_tokens_prefix (token_prefix)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "auth_reauthentication_proofs",
        "CREATE TABLE IF NOT EXISTS auth_reauthentication_proofs (
            proof_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            purpose VARCHAR(32) NOT NULL,
            proof_hash CHAR(64) NOT NULL,
            expires_at DATETIME(6) NOT NULL,
            consumed_at DATETIME(6) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, proof_id),
            UNIQUE KEY uq_auth_reauthentication_proof_hash (proof_hash),
            INDEX idx_auth_reauthentication_user_expiry (user_id, expires_at),
            INDEX idx_auth_reauthentication_expiry (expires_at, consumed_at)
        )",
    )
    .execute(&pool)
    .await?;

    add_column_if_missing(
        &pool,
        &settings.database,
        "auth_refresh_tokens",
        "session_id",
        "ALTER TABLE auth_refresh_tokens ADD COLUMN session_id VARCHAR(64) NULL",
    )
    .await?;
    add_index_if_missing(
        &pool,
        &settings.database,
        "auth_refresh_tokens",
        "idx_auth_refresh_tokens_session",
        "ALTER TABLE auth_refresh_tokens ADD INDEX idx_auth_refresh_tokens_session (session_id)",
    )
    .await?;

    core_schema_create!(
        pool,
        "auth_tokens",
        "CREATE TABLE IF NOT EXISTS auth_tokens (
            token_id VARCHAR(64) PRIMARY KEY,
            type VARCHAR(50) NOT NULL,
            provider VARCHAR(50) NOT NULL,
            encrypted_value TEXT NULL,
            secret_ref VARCHAR(255) NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            scope_user_id VARCHAR(128) NULL,
            scope_repo VARCHAR(255) NULL,
            metadata JSON NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_auth_tokens_active (is_active),
            INDEX idx_auth_tokens_scope_user (scope_user_id),
            INDEX idx_auth_tokens_scope_repo (scope_repo)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(pool, "auth_provider_request_replay",
        "CREATE TABLE IF NOT EXISTS auth_provider_request_replay (
            provider VARCHAR(64) NOT NULL,
            request_authorization_id VARCHAR(512) NOT NULL,
            external_subject VARCHAR(255) NOT NULL,
            request_id VARCHAR(255) NOT NULL,
            expires_at_unix BIGINT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (provider, request_authorization_id),
            INDEX idx_auth_provider_request_replay_expires (expires_at_unix, provider, request_authorization_id),
            INDEX idx_auth_provider_request_replay_request (provider, request_id)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(pool, "auth_audit_logs",
        "CREATE TABLE IF NOT EXISTS auth_audit_logs (
            log_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            action VARCHAR(50) NOT NULL,
            resource_type VARCHAR(50) NULL,
            resource_id VARCHAR(64) NULL,
            details JSON NULL,
            ip_address VARCHAR(45) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_auth_audit_logs_user_created (user_id, created_at),
            INDEX idx_auth_audit_logs_user_resource_created (user_id, resource_type, resource_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    // Sessions / events core
    core_schema_create!(
        pool,
        "agent_sessions",
        "CREATE TABLE IF NOT EXISTS agent_sessions (
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            agent_id VARCHAR(255) NULL,
            title VARCHAR(255) NULL,
            status VARCHAR(20) NOT NULL DEFAULT 'active',
            event_count BIGINT NOT NULL DEFAULT 0,
            last_event_id VARCHAR(128) NULL,
            summary_status VARCHAR(20) NULL,
            summary_job_id VARCHAR(64) NULL,
            vector_db_snapshot_id VARCHAR(64) NULL,
            metadata JSON NULL,
            project_id VARCHAR(128) NULL,
            project_retention_policy VARCHAR(32) NOT NULL DEFAULT 'session',
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            ended_at DATETIME(6) NULL,
            delete_requested_at DATETIME(6) NULL,
            last_active_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            active_plan_id VARCHAR(64) NULL,
            config_version_id VARCHAR(24) NULL,
            PRIMARY KEY (user_id, session_id),
            INDEX idx_agent_sessions_user_status_updated (user_id, status, updated_at),
            INDEX idx_agent_sessions_delete_requested_owner
                (delete_requested_at, user_id, session_id),
            INDEX idx_agent_sessions_user_last_active (user_id, last_active_at),
            INDEX idx_agent_sessions_agent_status (agent_id, status),
            INDEX idx_agent_sessions_active_plan_id (active_plan_id),
            INDEX idx_agent_sessions_config_version (config_version_id),
            INDEX idx_sessions_project (user_id, project_id, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "agent_sessions",
        "delete_requested_at",
        "ALTER TABLE agent_sessions ADD COLUMN delete_requested_at DATETIME(6) NULL",
    )
    .await?;
    let legacy_delete_intents_backfilled = query(
        "UPDATE agent_sessions
         SET delete_requested_at = COALESCE(ended_at, updated_at, created_at)
         WHERE status = 'deleting' AND delete_requested_at IS NULL",
    )
    .execute(&pool)
    .await?
    .rows_affected();
    if legacy_delete_intents_backfilled > 0 {
        tracing::info!(
            legacy_delete_intents_backfilled,
            "backfilled immutable timestamps for legacy session delete intents"
        );
    }
    drop_index_if_present(
        &pool,
        &settings.database,
        "agent_sessions",
        "idx_agent_sessions_status_updated_owner",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "agent_sessions",
        "idx_agent_sessions_delete_requested_owner",
        &["delete_requested_at", "user_id", "session_id"],
        "ALTER TABLE agent_sessions ADD INDEX idx_agent_sessions_delete_requested_owner (delete_requested_at, user_id, session_id)",
    )
    .await?;
    core_schema_create!(
        pool,
        "agent_session_lifecycle_fences",
        "CREATE TABLE IF NOT EXISTS agent_session_lifecycle_fences (
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            delete_requested_at DATETIME(6) NULL,
            database_deleted_at DATETIME(6) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id),
            INDEX idx_agent_session_fences_pending_delete
                (database_deleted_at, delete_requested_at, user_id, session_id)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "agent_session_lifecycle_fences",
        &["user_id", "session_id"],
        "ALTER TABLE agent_session_lifecycle_fences ADD PRIMARY KEY (user_id, session_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "agent_session_lifecycle_fences",
        "idx_agent_session_fences_pending_delete",
        &[
            "database_deleted_at",
            "delete_requested_at",
            "user_id",
            "session_id",
        ],
        "ALTER TABLE agent_session_lifecycle_fences ADD INDEX idx_agent_session_fences_pending_delete (database_deleted_at, delete_requested_at, user_id, session_id)",
    )
    .await?;
    let lifecycle_fences_backfilled = query(
        "INSERT IGNORE INTO agent_session_lifecycle_fences
         (session_id, user_id, delete_requested_at, created_at, updated_at)
         SELECT session_id, user_id, delete_requested_at, created_at, updated_at
         FROM agent_sessions",
    )
    .execute(&pool)
    .await?
    .rows_affected();
    if lifecycle_fences_backfilled > 0 {
        tracing::info!(
            lifecycle_fences_backfilled,
            "backfilled durable lifecycle fences for existing sessions"
        );
    }
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "agent_sessions",
        &["user_id", "session_id"],
        "ALTER TABLE agent_sessions ADD PRIMARY KEY (user_id, session_id)",
    )
    .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "agent_sessions",
        &[("last_event_id", AGENT_EVENT_ID_LEN as u64)],
    )
    .await?;
    core_schema_create!(
        pool,
        "session_deletion_tombstones",
        "CREATE TABLE IF NOT EXISTS session_deletion_tombstones (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            deleted_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id),
            INDEX idx_session_deletion_tombstones_deleted (deleted_at)
        )",
    )
    .execute(&pool)
    .await?;

    // Work is the canonical product root. Bootstrap creates the current
    // schema; an older shape is rejected below instead of being rewritten.
    let work_schema = pool.owned_by("work");
    for &(table_name, ddl) in crate::work::WORK_SCHEMA_TABLES {
        work_schema
            .authority
            .declare(work_schema.owner, table_name, ddl);
        query(ddl).execute(&work_schema).await?;
    }
    verify_work_canonical_schema(&pool, &settings.database).await?;

    let agent_events_sql = agent_events_create_sql();
    pool.authority
        .declare("storage", "agent_events", &agent_events_sql);
    query(&agent_events_sql).execute(&pool).await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "agent_events",
        &[
            ("event_id", AGENT_EVENT_ID_LEN as u64),
            ("parent_event_id", AGENT_EVENT_ID_LEN as u64),
            ("causal_chain_id", AGENT_EVENT_ID_LEN as u64),
        ],
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "agent_events",
        &["user_id", "event_id"],
        "ALTER TABLE agent_events ADD PRIMARY KEY (user_id, event_id)",
    )
    .await?;
    for removed_index in [
        "idx_agent_events_session_created",
        "idx_agent_events_session_type_created",
        "idx_agent_events_session_model_created",
        "idx_agent_events_session_parent",
        "idx_agent_events_causal_chain_id",
    ] {
        drop_index_if_present(&pool, &settings.database, "agent_events", removed_index).await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_agent_events_owner_session_created",
            &["user_id", "session_id", "created_at"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_owner_session_created (user_id, session_id, created_at)",
        ),
        (
            "idx_agent_events_owner_session_type_created",
            &["user_id", "session_id", "event_type", "created_at"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_owner_session_type_created (user_id, session_id, event_type, created_at)",
        ),
        (
            "idx_agent_events_owner_session_model_created",
            &["user_id", "session_id", "llm_model_used", "created_at"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_owner_session_model_created (user_id, session_id, llm_model_used, created_at DESC)",
        ),
        (
            "idx_agent_events_owner_session_parent",
            &["user_id", "session_id", "parent_event_id"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_owner_session_parent (user_id, session_id, parent_event_id)",
        ),
        (
            "idx_agent_events_owner_causal_chain_created",
            &["user_id", "causal_chain_id", "created_at", "event_id"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_owner_causal_chain_created (user_id, causal_chain_id, created_at, event_id)",
        ),
        (
            "idx_agent_events_trace",
            &["user_id", "session_id", "turn_id", "created_at"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_trace (user_id, session_id, turn_id, created_at)",
        ),
        (
            "idx_agent_events_run",
            &["user_id", "session_id", "run_id", "created_at"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_run (user_id, session_id, run_id, created_at)",
        ),
        (
            "idx_agent_events_parent_run",
            &["user_id", "session_id", "parent_run_id", "created_at"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_parent_run (user_id, session_id, parent_run_id, created_at)",
        ),
        (
            "idx_agent_events_tool_call",
            &["user_id", "session_id", "tool_call_id"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_tool_call (user_id, session_id, tool_call_id)",
        ),
        (
            "idx_agent_events_owner_session_turn",
            &["user_id", "session_id", "turn_seq"][..],
            AGENT_EVENTS_OWNER_SESSION_TURN_INDEX_ALTER_SQL,
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "agent_events",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }
    let workspace_schema = pool.owned_by("workspace_records");
    for (table_name, ddl) in [
        (
            "workspace_records",
            crate::workspace_records::WORKSPACE_RECORDS_CREATE_SQL,
        ),
        (
            "workspace_cleanup_debts",
            crate::workspace_records::WORKSPACE_CLEANUP_DEBTS_CREATE_SQL,
        ),
    ] {
        workspace_schema
            .authority
            .declare(workspace_schema.owner, table_name, ddl);
        query(ddl).execute(&workspace_schema).await?;
    }
    crate::workspace_records::verify_workspace_record_tables(&workspace_schema).await?;

    // ── Durable web-agent run state (Phase 1 / G15 + G19) ────────────────
    core_schema_create!(pool, "agent_runs", AGENT_RUNS_CREATE_SQL)
        .execute(&pool)
        .await?;
    verify_agent_runs_canonical_schema(&pool, &settings.database).await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "agent_runs",
        &[("trigger_event_id", AGENT_EVENT_ID_LEN as u64)],
    )
    .await?;

    core_schema_create!(
        pool,
        "agent_session_execution_slots",
        "CREATE TABLE IF NOT EXISTS agent_session_execution_slots (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64) NOT NULL,
            acquired_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id),
            INDEX idx_session_execution_slots_run (user_id, run_id),
            INDEX idx_session_execution_slots_updated (updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "agent_session_execution_slots",
        &["user_id", "session_id"],
        "ALTER TABLE agent_session_execution_slots ADD PRIMARY KEY (user_id, session_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "agent_session_execution_slots",
        "idx_session_execution_slots_run",
        &["user_id", "run_id"],
        "ALTER TABLE agent_session_execution_slots ADD INDEX idx_session_execution_slots_run (user_id, run_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "agent_session_execution_slots",
        "idx_session_execution_slots_updated",
        &["updated_at"],
        "ALTER TABLE agent_session_execution_slots ADD INDEX idx_session_execution_slots_updated (updated_at)",
    )
    .await?;

    core_schema_create!(
        pool,
        "session_context_heads",
        "CREATE TABLE IF NOT EXISTS session_context_heads (
            isolation_domain VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            branch_id VARCHAR(128) NOT NULL,
            head_json LONGTEXT NULL,
            canonical_root_hash CHAR(64) NULL,
            latest_manifest_root CHAR(64) NULL,
            total_canonical_bytes BIGINT NOT NULL DEFAULT 0,
            total_message_count BIGINT NOT NULL DEFAULT 0,
            completed_turn BIGINT NOT NULL DEFAULT 0,
            journal_event_seq BIGINT NOT NULL DEFAULT 0,
            conversation_seq BIGINT NOT NULL DEFAULT 0,
            projection_schema INT NOT NULL DEFAULT 0,
            compaction_generation BIGINT NOT NULL DEFAULT 0,
            writer_epoch BIGINT NOT NULL DEFAULT 0,
            authorization_epoch BIGINT NOT NULL DEFAULT 0,
            device_trust_epoch BIGINT NOT NULL DEFAULT 0,
            permission_epoch BIGINT NOT NULL DEFAULT 0,
            active_writer_json LONGTEXT NULL,
            active_writer_expires_at_ms BIGINT NULL,
            active_reservation_json LONGTEXT NULL,
            active_reservation_expires_at_ms BIGINT NULL,
            last_commit_json LONGTEXT NULL,
            fork_base_json LONGTEXT NULL,
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (isolation_domain, owner_user_id, session_id, branch_id),
            INDEX idx_session_context_heads_owner_session (owner_user_id, session_id, branch_id),
            INDEX idx_session_context_heads_writer_expiry (active_writer_expires_at_ms)
        )",
    )
    .execute(&pool)
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "session_context_heads",
        "fork_base_json",
        "ALTER TABLE session_context_heads ADD COLUMN fork_base_json LONGTEXT NULL",
    )
    .await?;

    core_schema_create!(
        pool,
        "conversation_segments",
        "CREATE TABLE IF NOT EXISTS conversation_segments (
            isolation_domain VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            segment_hash CHAR(64) NOT NULL,
            canonical_root_hash CHAR(64) NOT NULL,
            canonical_bytes BIGINT NOT NULL,
            message_count BIGINT NOT NULL,
            segment_json LONGTEXT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (isolation_domain, owner_user_id, segment_hash),
            INDEX idx_conversation_segments_created (created_at)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "session_forks",
        "CREATE TABLE IF NOT EXISTS session_forks (
            isolation_domain VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            fork_id VARCHAR(128) NOT NULL,
            parent_session_id VARCHAR(128) NOT NULL,
            parent_branch_id VARCHAR(128) NOT NULL,
            child_session_id VARCHAR(128) NOT NULL,
            child_branch_id VARCHAR(128) NOT NULL,
            idempotency_hash CHAR(64) NOT NULL,
            request_hash CHAR(64) NOT NULL,
            state VARCHAR(32) NOT NULL,
            manifest_json LONGTEXT NOT NULL,
            activated_at_ms BIGINT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (isolation_domain, owner_user_id, fork_id),
            UNIQUE KEY uq_session_forks_child
                (isolation_domain, owner_user_id, child_session_id, child_branch_id),
            UNIQUE KEY uq_session_forks_parent_idempotency
                (isolation_domain, owner_user_id, parent_session_id, parent_branch_id, idempotency_hash),
            INDEX idx_session_forks_parent_state
                (isolation_domain, owner_user_id, parent_session_id, parent_branch_id, state),
            INDEX idx_session_forks_child_state
                (isolation_domain, owner_user_id, child_session_id, child_branch_id, state)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "session_fork_events",
        "CREATE TABLE IF NOT EXISTS session_fork_events (
            isolation_domain VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            fork_id VARCHAR(128) NOT NULL,
            transition_seq BIGINT NOT NULL,
            parent_session_id VARCHAR(128) NOT NULL,
            child_session_id VARCHAR(128) NOT NULL,
            from_state VARCHAR(32) NULL,
            to_state VARCHAR(32) NOT NULL,
            event_json LONGTEXT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (isolation_domain, owner_user_id, fork_id, transition_seq),
            INDEX idx_session_fork_events_parent
                (isolation_domain, owner_user_id, parent_session_id, created_at),
            INDEX idx_session_fork_events_child
                (isolation_domain, owner_user_id, child_session_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "conversation_manifest_pins",
        "CREATE TABLE IF NOT EXISTS conversation_manifest_pins (
            isolation_domain VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            pin_id VARCHAR(128) NOT NULL,
            parent_session_id VARCHAR(128) NOT NULL,
            parent_branch_id VARCHAR(128) NOT NULL,
            manifest_root CHAR(64) NOT NULL,
            pin_state VARCHAR(32) NOT NULL,
            grace_expires_at_ms BIGINT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (isolation_domain, owner_user_id, pin_id),
            INDEX idx_manifest_pins_parent
                (isolation_domain, owner_user_id, parent_session_id, parent_branch_id, pin_state),
            INDEX idx_manifest_pins_grace
                (pin_state, grace_expires_at_ms)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "conversation_manifest_nodes",
        "CREATE TABLE IF NOT EXISTS conversation_manifest_nodes (
            isolation_domain VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            branch_id VARCHAR(128) NOT NULL,
            manifest_root CHAR(64) NOT NULL,
            parent_manifest_root CHAR(64) NULL,
            completed_turn BIGINT NOT NULL,
            conversation_seq BIGINT NOT NULL,
            compaction_generation BIGINT NOT NULL DEFAULT 0,
            canonical_segment_bytes BIGINT NOT NULL,
            total_canonical_bytes BIGINT NOT NULL,
            total_message_count BIGINT NOT NULL,
            reachable TINYINT NOT NULL DEFAULT 0,
            manifest_json LONGTEXT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (isolation_domain, owner_user_id, session_id, branch_id, manifest_root),
            INDEX idx_context_manifest_parent (isolation_domain, owner_user_id, session_id, branch_id, parent_manifest_root),
            INDEX idx_context_manifest_sequence (isolation_domain, owner_user_id, session_id, branch_id, conversation_seq),
            INDEX idx_context_manifest_reachable_sequence (isolation_domain, owner_user_id, session_id, branch_id, reachable, conversation_seq),
            INDEX idx_context_manifest_generation_sequence (isolation_domain, owner_user_id, session_id, branch_id, compaction_generation, reachable, conversation_seq)
        )",
    )
    .execute(&pool)
    .await?;
    for (column, ddl) in [
        (
            "total_canonical_bytes",
            "ALTER TABLE conversation_manifest_nodes ADD COLUMN total_canonical_bytes BIGINT NOT NULL DEFAULT 0",
        ),
        (
            "total_message_count",
            "ALTER TABLE conversation_manifest_nodes ADD COLUMN total_message_count BIGINT NOT NULL DEFAULT 0",
        ),
        (
            "reachable",
            "ALTER TABLE conversation_manifest_nodes ADD COLUMN reachable TINYINT NOT NULL DEFAULT 0",
        ),
        (
            "compaction_generation",
            "ALTER TABLE conversation_manifest_nodes ADD COLUMN compaction_generation BIGINT NOT NULL DEFAULT 0",
        ),
    ] {
        add_column_if_missing(
            &pool,
            &settings.database,
            "conversation_manifest_nodes",
            column,
            ddl,
        )
        .await?;
    }
    ensure_index_shape(
        &pool,
        &settings.database,
        "conversation_manifest_nodes",
        "idx_context_manifest_reachable_sequence",
        &[
            "isolation_domain",
            "owner_user_id",
            "session_id",
            "branch_id",
            "reachable",
            "conversation_seq",
        ],
        "ALTER TABLE conversation_manifest_nodes ADD INDEX idx_context_manifest_reachable_sequence (isolation_domain, owner_user_id, session_id, branch_id, reachable, conversation_seq)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "conversation_manifest_nodes",
        "idx_context_manifest_generation_sequence",
        &[
            "isolation_domain",
            "owner_user_id",
            "session_id",
            "branch_id",
            "compaction_generation",
            "reachable",
            "conversation_seq",
        ],
        "ALTER TABLE conversation_manifest_nodes ADD INDEX idx_context_manifest_generation_sequence (isolation_domain, owner_user_id, session_id, branch_id, compaction_generation, reachable, conversation_seq)",
    )
    .await?;
    query(
        "UPDATE conversation_manifest_nodes n
         INNER JOIN session_context_heads h
           ON h.isolation_domain = n.isolation_domain
          AND h.owner_user_id = n.owner_user_id
          AND h.session_id = n.session_id
          AND h.branch_id = n.branch_id
          AND h.latest_manifest_root = n.manifest_root
         SET n.reachable = 1
         WHERE n.reachable = 0",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "conversation_manifest_segments",
        "CREATE TABLE IF NOT EXISTS conversation_manifest_segments (
            isolation_domain VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            branch_id VARCHAR(128) NOT NULL,
            manifest_root CHAR(64) NOT NULL,
            segment_position BIGINT NOT NULL,
            segment_hash CHAR(64) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (
                isolation_domain, owner_user_id, session_id, branch_id,
                manifest_root, segment_position
            ),
            INDEX idx_manifest_segments_hash (
                isolation_domain, owner_user_id, segment_hash, manifest_root
            ),
            INDEX idx_manifest_segments_session (
                owner_user_id, session_id, branch_id, manifest_root
            ),
            INDEX idx_manifest_segments_created (created_at)
        )",
    )
    .execute(&pool)
    .await?;
    backfill_conversation_manifest_segments(&pool).await?;

    core_schema_create!(
        pool,
        "session_context_operation_receipts",
        "CREATE TABLE IF NOT EXISTS session_context_operation_receipts (
            isolation_domain VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            branch_id VARCHAR(128) NOT NULL,
            operation_kind VARCHAR(32) NOT NULL,
            idempotency_hash CHAR(64) NOT NULL,
            request_hash CHAR(64) NOT NULL,
            receipt_json LONGTEXT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (isolation_domain, owner_user_id, session_id, branch_id, operation_kind, idempotency_hash),
            INDEX idx_session_context_receipts_created (created_at)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "session_context_authority_events",
        "CREATE TABLE IF NOT EXISTS session_context_authority_events (
            isolation_domain VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            event_id CHAR(36) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            branch_id VARCHAR(128) NOT NULL,
            operation_kind VARCHAR(32) NOT NULL,
            outcome VARCHAR(32) NOT NULL,
            writer_epoch BIGINT NOT NULL,
            actor_id VARCHAR(512) NULL,
            device_id VARCHAR(512) NULL,
            lease_id CHAR(36) NULL,
            reservation_id CHAR(36) NULL,
            expected_root CHAR(64) NULL,
            observed_root CHAR(64) NULL,
            authorization_epoch BIGINT NOT NULL,
            device_trust_epoch BIGINT NOT NULL,
            permission_epoch BIGINT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (isolation_domain, owner_user_id, event_id),
            INDEX idx_context_authority_session_created
                (isolation_domain, owner_user_id, session_id, branch_id, created_at),
            INDEX idx_context_authority_outcome_created
                (operation_kind, outcome, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "session_attachment_quarantines",
        "CREATE TABLE IF NOT EXISTS session_attachment_quarantines (
            isolation_domain VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            branch_id VARCHAR(128) NOT NULL,
            quarantine_id CHAR(36) NOT NULL,
            idempotency_hash CHAR(64) NOT NULL,
            request_hash CHAR(64) NOT NULL,
            observed_manifest_root CHAR(64) NOT NULL,
            current_manifest_root CHAR(64) NULL,
            reason VARCHAR(64) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (isolation_domain, owner_user_id, session_id, branch_id, quarantine_id),
            UNIQUE KEY uq_session_attachment_quarantine_idempotency
                (isolation_domain, owner_user_id, session_id, branch_id, idempotency_hash),
            INDEX idx_session_attachment_quarantine_created
                (isolation_domain, owner_user_id, session_id, branch_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "session_handoff_slots",
        "CREATE TABLE IF NOT EXISTS session_handoff_slots (
            isolation_domain VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            branch_id VARCHAR(128) NOT NULL,
            active_handoff_id CHAR(36) NULL,
            next_attachment_epoch BIGINT NOT NULL DEFAULT 0,
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (isolation_domain, owner_user_id, session_id, branch_id)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "session_attachments",
        "CREATE TABLE IF NOT EXISTS session_attachments (
            isolation_domain VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            branch_id VARCHAR(128) NOT NULL,
            attachment_id CHAR(36) NOT NULL,
            attachment_epoch BIGINT NOT NULL,
            idempotency_hash CHAR(64) NOT NULL,
            request_hash CHAR(64) NOT NULL,
            actor_id VARCHAR(512) NOT NULL,
            mode VARCHAR(32) NOT NULL,
            placement VARCHAR(32) NOT NULL,
            observed_manifest_root CHAR(64) NULL,
            attachment_json LONGTEXT NOT NULL,
            expires_at_ms BIGINT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (isolation_domain, owner_user_id, session_id, branch_id, attachment_id),
            UNIQUE KEY uq_session_attachment_epoch
                (isolation_domain, owner_user_id, session_id, branch_id, attachment_epoch),
            UNIQUE KEY uq_session_attachment_idempotency
                (isolation_domain, owner_user_id, session_id, branch_id, idempotency_hash),
            INDEX idx_session_attachments_expiry (expires_at_ms)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "session_handoffs",
        "CREATE TABLE IF NOT EXISTS session_handoffs (
            isolation_domain VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            branch_id VARCHAR(128) NOT NULL,
            handoff_id CHAR(36) NOT NULL,
            idempotency_hash CHAR(64) NOT NULL,
            request_hash CHAR(64) NOT NULL,
            state VARCHAR(32) NOT NULL,
            mode VARCHAR(32) NOT NULL,
            transition_seq BIGINT NOT NULL,
            deadline_ms BIGINT NOT NULL,
            record_json LONGTEXT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (isolation_domain, owner_user_id, session_id, branch_id, handoff_id),
            UNIQUE KEY uq_session_handoff_idempotency
                (isolation_domain, owner_user_id, session_id, branch_id, idempotency_hash),
            INDEX idx_session_handoffs_state_deadline (state, deadline_ms)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "session_handoff_events",
        "CREATE TABLE IF NOT EXISTS session_handoff_events (
            isolation_domain VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            branch_id VARCHAR(128) NOT NULL,
            handoff_id CHAR(36) NOT NULL,
            transition_seq BIGINT NOT NULL,
            request_hash CHAR(64) NOT NULL,
            from_state VARCHAR(32) NULL,
            to_state VARCHAR(32) NOT NULL,
            event_json LONGTEXT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (isolation_domain, owner_user_id, session_id, branch_id, handoff_id, transition_seq),
            INDEX idx_session_handoff_events_created
                (isolation_domain, owner_user_id, session_id, branch_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "session_weighted_admission_gates",
        "CREATE TABLE IF NOT EXISTS session_weighted_admission_gates (
            scope_name VARCHAR(64) NOT NULL,
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (scope_name)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "session_weighted_admission_reservations",
        "CREATE TABLE IF NOT EXISTS session_weighted_admission_reservations (
            scope_name VARCHAR(64) NOT NULL,
            reservation_id CHAR(36) NOT NULL,
            isolation_domain VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            branch_id VARCHAR(128) NOT NULL,
            idempotency_hash CHAR(64) NOT NULL,
            resident_bytes BIGINT NOT NULL,
            context_tokens BIGINT NOT NULL,
            provider_slots BIGINT NOT NULL,
            cpu_units BIGINT NOT NULL,
            io_bytes BIGINT NOT NULL,
            expires_at DATETIME(6) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (scope_name, reservation_id),
            UNIQUE KEY uq_weighted_admission_idempotency
                (scope_name, isolation_domain, owner_user_id, idempotency_hash),
            INDEX idx_weighted_admission_expiry (scope_name, expires_at),
            INDEX idx_weighted_admission_owner_expiry
                (scope_name, isolation_domain, owner_user_id, expires_at)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(pool, "agent_run_events",
        "CREATE TABLE IF NOT EXISTS agent_run_events (
            id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64) NOT NULL,
            event_idx BIGINT NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            event_type VARCHAR(64) NOT NULL,
            event_id VARCHAR(128) NOT NULL,
            agent_id VARCHAR(255) NULL,
            subject_run_id VARCHAR(64) NULL,
            interaction_request_id VARCHAR(128) NULL,
            idempotency_key VARCHAR(128) NULL,
            event_hash VARCHAR(64) NOT NULL,
            producer_pod_id VARCHAR(128) NULL,
            payload_json LONGTEXT NOT NULL,
            request_id VARCHAR(64) NULL,
            trace_id VARCHAR(64) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, id),
            UNIQUE KEY uq_run_event_idx (user_id, run_id, event_idx),
            UNIQUE KEY uq_run_event_idempotency (user_id, run_id, idempotency_key),
            INDEX idx_agent_run_events_control_type_idx (user_id, run_id, event_type, event_idx),
            INDEX idx_agent_run_events_owner_session_run_idx (user_id, session_id, run_id, event_idx),
            INDEX idx_agent_run_events_owner_session_subject (user_id, session_id, event_type, subject_run_id, event_idx),
            INDEX idx_agent_run_events_interaction (user_id, run_id, interaction_request_id, event_type, event_idx),
            INDEX idx_agent_run_events_user_created (user_id, created_at),
            INDEX idx_agent_run_events_event_id (event_id)
        )",
    )
    .execute(&pool)
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "agent_run_events",
        "subject_run_id",
        "ALTER TABLE agent_run_events ADD COLUMN subject_run_id VARCHAR(64) NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "agent_run_events",
        "interaction_request_id",
        "ALTER TABLE agent_run_events ADD COLUMN interaction_request_id VARCHAR(128) NULL",
    )
    .await?;
    for (index, expected_columns, ddl) in [
        (
            "uq_run_event_idx",
            &["user_id", "run_id", "event_idx"][..],
            "ALTER TABLE agent_run_events ADD UNIQUE KEY uq_run_event_idx (user_id, run_id, event_idx)",
        ),
        (
            "uq_run_event_idempotency",
            &["user_id", "run_id", "idempotency_key"][..],
            "ALTER TABLE agent_run_events ADD UNIQUE KEY uq_run_event_idempotency (user_id, run_id, idempotency_key)",
        ),
        (
            "idx_agent_run_events_control_type_idx",
            &["user_id", "run_id", "event_type", "event_idx"][..],
            "ALTER TABLE agent_run_events ADD INDEX idx_agent_run_events_control_type_idx (user_id, run_id, event_type, event_idx)",
        ),
        (
            "idx_agent_run_events_owner_session_run_idx",
            &["user_id", "session_id", "run_id", "event_idx"][..],
            "ALTER TABLE agent_run_events ADD INDEX idx_agent_run_events_owner_session_run_idx (user_id, session_id, run_id, event_idx)",
        ),
        (
            "idx_agent_run_events_owner_session_subject",
            &[
                "user_id",
                "session_id",
                "event_type",
                "subject_run_id",
                "event_idx",
            ][..],
            "ALTER TABLE agent_run_events ADD INDEX idx_agent_run_events_owner_session_subject (user_id, session_id, event_type, subject_run_id, event_idx)",
        ),
        (
            "idx_agent_run_events_interaction",
            &[
                "user_id",
                "run_id",
                "interaction_request_id",
                "event_type",
                "event_idx",
            ][..],
            "ALTER TABLE agent_run_events ADD INDEX idx_agent_run_events_interaction (user_id, run_id, interaction_request_id, event_type, event_idx)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "agent_run_events",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }
    for removed_index in [
        "idx_agent_run_events_run_created",
        "idx_agent_run_events_session_created",
    ] {
        drop_index_if_present(&pool, &settings.database, "agent_run_events", removed_index).await?;
    }
    core_schema_create!(pool, "run_checkpoints",
        "CREATE TABLE IF NOT EXISTS run_checkpoints (
            checkpoint_id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            node_seq BIGINT NOT NULL DEFAULT 0,
            checkpoint_kind VARCHAR(32) NOT NULL,
            checkpoint_version VARCHAR(32) NOT NULL,
            idempotency_key VARCHAR(191) NOT NULL,
            checkpoint_json LONGTEXT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, checkpoint_id),
            UNIQUE KEY uniq_run_checkpoint_idem (user_id, run_id, checkpoint_kind, idempotency_key),
            INDEX idx_run_checkpoints_user_run_created (user_id, run_id, created_at),
            INDEX idx_run_checkpoints_session_kind_created (user_id, session_id, checkpoint_kind, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "run_checkpoints",
        &["user_id", "checkpoint_id"],
        "ALTER TABLE run_checkpoints ADD PRIMARY KEY (user_id, checkpoint_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "run_checkpoints",
        "uniq_run_checkpoint_idem",
        &["user_id", "run_id", "checkpoint_kind", "idempotency_key"],
        "ALTER TABLE run_checkpoints ADD UNIQUE KEY uniq_run_checkpoint_idem (user_id, run_id, checkpoint_kind, idempotency_key)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "run_checkpoints",
        "idx_run_checkpoints_user_run_created",
        &["user_id", "run_id", "created_at"],
        "ALTER TABLE run_checkpoints ADD INDEX idx_run_checkpoints_user_run_created (user_id, run_id, created_at)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "run_checkpoints",
        "idx_run_checkpoints_session_kind_created",
        &["user_id", "session_id", "checkpoint_kind", "created_at"],
        "ALTER TABLE run_checkpoints ADD INDEX idx_run_checkpoints_session_kind_created (user_id, session_id, checkpoint_kind, created_at)",
    )
    .await?;

    core_schema_create!(pool, "run_display_projections",
        "CREATE TABLE IF NOT EXISTS run_display_projections (
            run_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            status VARCHAR(32) NOT NULL,
            waiting_for VARCHAR(64) NULL,
            error_message TEXT NULL,
            projection_event_idx BIGINT NOT NULL DEFAULT -1,
            latest_event_type VARCHAR(64) NULL,
            latest_checkpoint_id VARCHAR(64) NULL,
            latest_checkpoint_kind VARCHAR(32) NULL,
            latest_checkpoint_version VARCHAR(32) NULL,
            total_prompt_tokens BIGINT NOT NULL DEFAULT 0,
            total_completion_tokens BIGINT NOT NULL DEFAULT 0,
            total_tool_calls BIGINT NOT NULL DEFAULT 0,
            projection_hash VARCHAR(64) NOT NULL,
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, run_id),
            INDEX idx_run_display_projections_owner_session_updated (user_id, session_id, updated_at),
            INDEX idx_run_display_projections_user_updated (user_id, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "run_display_projections",
        &["user_id", "run_id"],
        "ALTER TABLE run_display_projections ADD PRIMARY KEY (user_id, run_id)",
    )
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "run_display_projections",
        "idx_run_display_projections_session_updated",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "run_display_projections",
        "idx_run_display_projections_owner_session_updated",
        &["user_id", "session_id", "updated_at"],
        "ALTER TABLE run_display_projections ADD INDEX idx_run_display_projections_owner_session_updated (user_id, session_id, updated_at)",
    )
    .await?;

    core_schema_create!(pool, "session_tool_output_batches",
        "CREATE TABLE IF NOT EXISTS session_tool_output_batches (
            batch_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            output_count INT NOT NULL,
            payload_bytes BIGINT NOT NULL,
            status VARCHAR(32) NOT NULL DEFAULT 'committed',
            request_id VARCHAR(64) NULL,
            trace_id VARCHAR(64) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, batch_id),
            INDEX idx_tool_output_batches_user_run_created (user_id, run_id, created_at, batch_id),
            INDEX idx_tool_output_batches_user_session_created (user_id, session_id, created_at, batch_id)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in [
        "idx_tool_output_batches_session",
        "idx_tool_output_batches_run_status",
        "idx_tool_output_batches_run_created",
        "idx_tool_output_batches_session_created",
    ] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "session_tool_output_batches",
            removed_index,
        )
        .await?;
    }
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_tool_output_batches",
        &["user_id", "session_id", "batch_id"],
        "ALTER TABLE session_tool_output_batches ADD PRIMARY KEY (user_id, session_id, batch_id)",
    )
    .await?;
    for (index, expected_columns, ddl) in [
        (
            "idx_tool_output_batches_user_run_created",
            &["user_id", "run_id", "created_at", "batch_id"][..],
            "ALTER TABLE session_tool_output_batches ADD INDEX idx_tool_output_batches_user_run_created (user_id, run_id, created_at, batch_id)",
        ),
        (
            "idx_tool_output_batches_user_session_created",
            &["user_id", "session_id", "created_at", "batch_id"][..],
            "ALTER TABLE session_tool_output_batches ADD INDEX idx_tool_output_batches_user_session_created (user_id, session_id, created_at, batch_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_tool_output_batches",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    core_schema_create!(pool, "session_tool_outputs",
        "CREATE TABLE IF NOT EXISTS session_tool_outputs (
            output_id VARCHAR(64) NOT NULL,
            batch_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            output_idx INT NOT NULL,
            parent_output_id VARCHAR(64) NULL,
            tool_call_id VARCHAR(128) NULL,
            tool_name VARCHAR(128) NOT NULL,
            output_json LONGTEXT NOT NULL,
            payload_bytes BIGINT NOT NULL,
            preview_text LONGTEXT NULL,
            preview_status VARCHAR(32) NOT NULL DEFAULT 'template',
            artifact_ref VARCHAR(255) NULL,
            content_hash VARCHAR(128) NULL,
            normalize_version VARCHAR(32) NOT NULL DEFAULT 'raw_v1',
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, output_id),
            UNIQUE KEY uq_tool_outputs_batch_idx (user_id, session_id, batch_id, output_idx),
            INDEX idx_tool_outputs_user_run_created (user_id, run_id, created_at, output_id),
            INDEX idx_tool_outputs_user_session_created (user_id, session_id, created_at, output_id),
            INDEX idx_tool_outputs_parent (user_id, parent_output_id),
            INDEX idx_tool_outputs_artifact_ref (user_id, artifact_ref)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in [
        "idx_tool_outputs_tool_created",
        "idx_tool_outputs_session_tool_score",
        "idx_tool_outputs_status_created",
        "idx_tool_outputs_batch",
        "idx_tool_outputs_run_created",
        "idx_tool_outputs_session_created",
    ] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "session_tool_outputs",
            removed_index,
        )
        .await?;
    }
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_tool_outputs",
        &["user_id", "session_id", "output_id"],
        "ALTER TABLE session_tool_outputs ADD PRIMARY KEY (user_id, session_id, output_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_tool_outputs",
        "uq_tool_outputs_batch_idx",
        &["user_id", "session_id", "batch_id", "output_idx"],
        "ALTER TABLE session_tool_outputs ADD UNIQUE KEY uq_tool_outputs_batch_idx (user_id, session_id, batch_id, output_idx)",
    )
    .await?;
    for (index, expected_columns, ddl) in [
        (
            "idx_tool_outputs_user_run_created",
            &["user_id", "run_id", "created_at", "output_id"][..],
            "ALTER TABLE session_tool_outputs ADD INDEX idx_tool_outputs_user_run_created (user_id, run_id, created_at, output_id)",
        ),
        (
            "idx_tool_outputs_user_session_created",
            &["user_id", "session_id", "created_at", "output_id"][..],
            "ALTER TABLE session_tool_outputs ADD INDEX idx_tool_outputs_user_session_created (user_id, session_id, created_at, output_id)",
        ),
        (
            "idx_tool_outputs_parent",
            &["user_id", "parent_output_id"][..],
            "ALTER TABLE session_tool_outputs ADD INDEX idx_tool_outputs_parent (user_id, parent_output_id)",
        ),
        (
            "idx_tool_outputs_artifact_ref",
            &["user_id", "artifact_ref"][..],
            "ALTER TABLE session_tool_outputs ADD INDEX idx_tool_outputs_artifact_ref (user_id, artifact_ref)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_tool_outputs",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    // `tool_exactly_once_results` is retired and is not a runtime authority.
    // Startup deliberately leaves any historical table untouched: removing
    // side-effect evidence is an explicit administrative lifecycle action,
    // not schema bootstrap. No compatibility read or migration is performed.

    core_schema_create!(
        pool,
        "tool_invocation_ledger",
        "CREATE TABLE IF NOT EXISTS tool_invocation_ledger (
            user_id             VARCHAR(128) NOT NULL,
            session_id          VARCHAR(128) NOT NULL,
            run_id              VARCHAR(128) NOT NULL,
            turn_chain_id       VARCHAR(128) NOT NULL,
            invocation_id       VARCHAR(128) NOT NULL,
            identity_key        VARCHAR(71) NOT NULL,
            fingerprint_json    JSON NOT NULL,
            decision_json       JSON NOT NULL,
            state               VARCHAR(32) NOT NULL,
            dispatch_certainty  VARCHAR(32) NOT NULL,
            attempt_count       BIGINT UNSIGNED NOT NULL DEFAULT 0,
            dispatch_owner      VARCHAR(64) NULL,
            dispatch_lease_expires_at DATETIME(6) NULL,
            outcome_json        JSON NULL,
            completion_source_json JSON NULL,
            created_at          DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at          DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, run_id, turn_chain_id, invocation_id),
            INDEX idx_tool_invocation_updated (updated_at),
            INDEX idx_tool_invocation_run_compaction
                (user_id, session_id, run_id, state, identity_key),
            INDEX idx_tool_invocation_session_state
                (user_id, session_id, state, identity_key)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_required_columns_missing_or_nullable(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        TOOL_INVOCATION_LEDGER_REQUIRED_COLUMNS,
    )
    .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        TOOL_INVOCATION_LEDGER_REQUIRED_VARCHAR_WIDTHS,
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        "idx_tool_invocation_run_compaction",
        &["user_id", "session_id", "run_id", "state", "identity_key"],
        "ALTER TABLE tool_invocation_ledger ADD INDEX idx_tool_invocation_run_compaction (user_id, session_id, run_id, state, identity_key)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        "idx_tool_invocation_session_state",
        &["user_id", "session_id", "state", "identity_key"],
        "ALTER TABLE tool_invocation_ledger ADD INDEX idx_tool_invocation_session_state (user_id, session_id, state, identity_key)",
    )
    .await?;
    core_schema_create!(
        pool,
        "tool_invocation_archive_chunks",
        "CREATE TABLE IF NOT EXISTS tool_invocation_archive_chunks (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64) NOT NULL,
            chunk_index BIGINT UNSIGNED NOT NULL,
            artifact_id VARCHAR(64) NOT NULL,
            first_identity_key VARCHAR(71) NOT NULL,
            last_identity_key VARCHAR(71) NOT NULL,
            record_count BIGINT UNSIGNED NOT NULL,
            encoded_bytes BIGINT UNSIGNED NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, run_id, chunk_index),
            UNIQUE KEY uq_tool_invocation_archive_artifact
                (user_id, session_id, artifact_id),
            INDEX idx_tool_invocation_archive_lookup
                (user_id, session_id, run_id, first_identity_key, last_identity_key)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "tool_invocation_archive_chunks",
        &["user_id", "session_id", "run_id", "chunk_index"],
        "ALTER TABLE tool_invocation_archive_chunks ADD PRIMARY KEY (user_id, session_id, run_id, chunk_index)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "tool_invocation_archive_chunks",
        "idx_tool_invocation_archive_lookup",
        &[
            "user_id",
            "session_id",
            "run_id",
            "first_identity_key",
            "last_identity_key",
        ],
        "ALTER TABLE tool_invocation_archive_chunks ADD INDEX idx_tool_invocation_archive_lookup (user_id, session_id, run_id, first_identity_key, last_identity_key)",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        "decision_json",
        "ALTER TABLE tool_invocation_ledger ADD COLUMN decision_json JSON NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        "dispatch_owner",
        "ALTER TABLE tool_invocation_ledger ADD COLUMN dispatch_owner VARCHAR(64) NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        "dispatch_lease_expires_at",
        "ALTER TABLE tool_invocation_ledger ADD COLUMN dispatch_lease_expires_at DATETIME(6) NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        "outcome_json",
        "ALTER TABLE tool_invocation_ledger ADD COLUMN outcome_json JSON NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        "completion_source_json",
        "ALTER TABLE tool_invocation_ledger ADD COLUMN completion_source_json JSON NULL",
    )
    .await?;

    core_schema_create!(
        pool,
        "semantic_read_observation_budgets",
        "CREATE TABLE IF NOT EXISTS semantic_read_observation_budgets (
            user_id             VARCHAR(128) NOT NULL,
            session_id          VARCHAR(128) NOT NULL,
            created_at          DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at          DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "semantic_read_observations",
        "CREATE TABLE IF NOT EXISTS semantic_read_observations (
            user_id             VARCHAR(128) NOT NULL,
            session_id          VARCHAR(128) NOT NULL,
            key_id              VARCHAR(71) NOT NULL,
            key_json            JSON NOT NULL,
            state               VARCHAR(16) NOT NULL,
            fill_owner          VARCHAR(64) NULL,
            fill_lease_expires_at DATETIME(6) NULL,
            observation_json    JSON NULL,
            observation_bytes   BIGINT UNSIGNED NULL,
            created_at          DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at          DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            last_accessed_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, key_id),
            INDEX idx_semantic_read_observations_session_state_access
                (user_id, session_id, state, last_accessed_at)
        )",
    )
    .execute(&pool)
    .await?;

    // ── Web transcript hydration + device lease state (Phase 2 / G13+G19+G25) ──
    core_schema_create!(
        pool,
        "session_transcript_items",
        "CREATE TABLE IF NOT EXISTS session_transcript_items (
            session_id VARCHAR(64) NOT NULL,
            item_seq BIGINT NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            run_id VARCHAR(64) NULL,
            role VARCHAR(32) NOT NULL,
            content LONGTEXT NOT NULL,
            payload_json LONGTEXT NULL,
            source_event_id VARCHAR(128) NULL,
            source_event_idx BIGINT NULL,
            content_hash VARCHAR(128) NOT NULL,
            canonical_completed_turn BIGINT NULL,
            canonical_conversation_seq BIGINT NULL,
            canonical_root_hash VARCHAR(64) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, item_seq),
            INDEX idx_transcript_owner_run_event (user_id, run_id, source_event_idx),
            INDEX idx_transcript_owner_session_source_event (user_id, session_id, source_event_id),
            INDEX idx_transcript_owner_session_commit_item
                (user_id, session_id, canonical_completed_turn, item_seq)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_transcript_items",
        &["user_id", "session_id", "item_seq"],
        "ALTER TABLE session_transcript_items ADD PRIMARY KEY (user_id, session_id, item_seq)",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "session_transcript_items",
        "payload_json",
        "ALTER TABLE session_transcript_items ADD COLUMN payload_json LONGTEXT NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "session_transcript_items",
        "canonical_completed_turn",
        "ALTER TABLE session_transcript_items ADD COLUMN canonical_completed_turn BIGINT NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "session_transcript_items",
        "canonical_conversation_seq",
        "ALTER TABLE session_transcript_items ADD COLUMN canonical_conversation_seq BIGINT NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "session_transcript_items",
        "canonical_root_hash",
        "ALTER TABLE session_transcript_items ADD COLUMN canonical_root_hash VARCHAR(64) NULL",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_transcript_items",
        "idx_transcript_owner_run_event",
        &["user_id", "run_id", "source_event_idx"],
        "ALTER TABLE session_transcript_items ADD INDEX idx_transcript_owner_run_event (user_id, run_id, source_event_idx)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_transcript_items",
        "idx_transcript_owner_session_commit_item",
        &[
            "user_id",
            "session_id",
            "canonical_completed_turn",
            "item_seq",
        ],
        "ALTER TABLE session_transcript_items ADD INDEX idx_transcript_owner_session_commit_item (user_id, session_id, canonical_completed_turn, item_seq)",
    )
    .await?;
    core_schema_create!(
        pool,
        "session_transcript_projection_heads",
        "CREATE TABLE IF NOT EXISTS session_transcript_projection_heads (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            completed_turn BIGINT NOT NULL,
            journal_event_seq BIGINT NOT NULL,
            conversation_seq BIGINT NOT NULL,
            canonical_root_hash VARCHAR(64) NOT NULL,
            projection_schema BIGINT NOT NULL,
            compaction_generation BIGINT NOT NULL,
            config_version_id VARCHAR(128) NULL,
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_transcript_items",
        "idx_transcript_owner_session_source_event",
        &["user_id", "session_id", "source_event_id"],
        "ALTER TABLE session_transcript_items ADD INDEX idx_transcript_owner_session_source_event (user_id, session_id, source_event_id)",
    )
    .await?;
    core_schema_create!(
        pool,
        "transcript_pages",
        "CREATE TABLE IF NOT EXISTS transcript_pages (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            page_seq BIGINT NOT NULL,
            start_item_seq BIGINT NOT NULL,
            end_item_seq BIGINT NOT NULL,
            item_count INT NOT NULL,
            page_hash VARCHAR(128) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, page_seq),
            INDEX idx_transcript_pages_owner_session_end (user_id, session_id, end_item_seq),
            INDEX idx_transcript_pages_owner_session_updated (user_id, session_id, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "transcript_pages",
        &["user_id", "session_id", "page_seq"],
        "ALTER TABLE transcript_pages ADD PRIMARY KEY (user_id, session_id, page_seq)",
    )
    .await?;
    for (index, expected_columns, ddl) in [
        (
            "idx_transcript_pages_owner_session_end",
            &["user_id", "session_id", "end_item_seq"][..],
            "ALTER TABLE transcript_pages ADD INDEX idx_transcript_pages_owner_session_end (user_id, session_id, end_item_seq)",
        ),
        (
            "idx_transcript_pages_owner_session_updated",
            &["user_id", "session_id", "updated_at"][..],
            "ALTER TABLE transcript_pages ADD INDEX idx_transcript_pages_owner_session_updated (user_id, session_id, updated_at)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "transcript_pages",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }
    core_schema_create!(pool, "prompt_request_records",
        "CREATE TABLE IF NOT EXISTS prompt_request_records (
            request_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            run_id VARCHAR(64) NULL,
            turn BIGINT NOT NULL,
            round BIGINT NOT NULL,
            attempt BIGINT NOT NULL,
            source VARCHAR(64) NOT NULL,
            model VARCHAR(128) NOT NULL,
            provider VARCHAR(64) NOT NULL,
            max_output_tokens BIGINT NULL,
            message_count BIGINT NOT NULL,
            tool_count BIGINT NOT NULL,
            previous_request_id VARCHAR(64) NULL,
            request_hash VARCHAR(64) NOT NULL,
            summary_json LONGTEXT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            created_at_unix_ms BIGINT NULL,
            PRIMARY KEY (user_id, request_id),
            UNIQUE KEY uq_prompt_request_attempt (user_id, session_id, turn, round, source, attempt),
            INDEX idx_prompt_requests_owner_session_created (user_id, session_id, created_at, turn, round, attempt),
            INDEX idx_prompt_requests_owner_run_created (user_id, run_id, created_at, turn, round, attempt),
            INDEX idx_prompt_requests_owner_previous (user_id, session_id, previous_request_id),
            INDEX idx_prompt_requests_retention_ms (created_at_unix_ms, user_id, request_id, session_id)
        )",
    )
    .execute(&pool)
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "prompt_request_records",
        "created_at_unix_ms",
        "ALTER TABLE prompt_request_records ADD COLUMN created_at_unix_ms BIGINT NULL",
    )
    .await?;
    query(
        "UPDATE prompt_request_records
         SET created_at_unix_ms = UNIX_TIMESTAMP(created_at) * 1000
         WHERE created_at_unix_ms IS NULL",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "prompt_request_records",
        &["user_id", "request_id"],
        "ALTER TABLE prompt_request_records ADD PRIMARY KEY (user_id, request_id)",
    )
    .await?;
    for removed_index in [
        "idx_prompt_requests_session_created",
        "idx_prompt_requests_run_created",
    ] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "prompt_request_records",
            removed_index,
        )
        .await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_prompt_requests_owner_session_created",
            &[
                "user_id",
                "session_id",
                "created_at",
                "turn",
                "round",
                "attempt",
            ][..],
            "ALTER TABLE prompt_request_records ADD INDEX idx_prompt_requests_owner_session_created (user_id, session_id, created_at, turn, round, attempt)",
        ),
        (
            "idx_prompt_requests_owner_run_created",
            &[
                "user_id",
                "run_id",
                "created_at",
                "turn",
                "round",
                "attempt",
            ][..],
            "ALTER TABLE prompt_request_records ADD INDEX idx_prompt_requests_owner_run_created (user_id, run_id, created_at, turn, round, attempt)",
        ),
        (
            "idx_prompt_requests_owner_previous",
            &["user_id", "session_id", "previous_request_id"][..],
            "ALTER TABLE prompt_request_records ADD INDEX idx_prompt_requests_owner_previous (user_id, session_id, previous_request_id)",
        ),
        (
            "idx_prompt_requests_retention_ms",
            &["created_at_unix_ms", "user_id", "request_id", "session_id"][..],
            "ALTER TABLE prompt_request_records ADD INDEX idx_prompt_requests_retention_ms (created_at_unix_ms, user_id, request_id, session_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "prompt_request_records",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }
    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "prompt_deltas",
        &[
            "user_id",
            "session_id",
            "request_id",
            "delta_seq",
            "logical_key",
            "chunk_kind",
            "position",
            "op",
            "chunk_id",
            "chunk_hash",
            "previous_chunk_hash",
            "created_at",
        ],
        &["delta_id", "payload_json"],
        &[
            "uq_prompt_delta_request_seq",
            "idx_prompt_deltas_request_position",
        ],
    )
    .await?;
    core_schema_create!(pool, "prompt_deltas",
        "CREATE TABLE IF NOT EXISTS prompt_deltas (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            request_id VARCHAR(64) NOT NULL,
            delta_seq INT NOT NULL,
            logical_key VARCHAR(191) NOT NULL,
            chunk_kind VARCHAR(32) NOT NULL,
            position INT NOT NULL,
            op VARCHAR(16) NOT NULL,
            reuse_count INT NULL,
            chunk_id VARCHAR(80) NULL,
            chunk_hash VARCHAR(64) NULL,
            previous_chunk_hash VARCHAR(64) NULL,
            chunk_tokens BIGINT NULL,
            chunk_bytes BIGINT NULL,
            previous_chunk_tokens BIGINT NULL,
            previous_chunk_bytes BIGINT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, request_id, delta_seq),
            INDEX idx_prompt_deltas_owner_request_position (user_id, session_id, request_id, position, delta_seq)
        )",
    )
    .execute(&pool)
    .await?;
    for (column, ddl) in [
        (
            "reuse_count",
            "ALTER TABLE prompt_deltas ADD COLUMN reuse_count INT NULL",
        ),
        (
            "chunk_tokens",
            "ALTER TABLE prompt_deltas ADD COLUMN chunk_tokens BIGINT NULL",
        ),
        (
            "chunk_bytes",
            "ALTER TABLE prompt_deltas ADD COLUMN chunk_bytes BIGINT NULL",
        ),
        (
            "previous_chunk_tokens",
            "ALTER TABLE prompt_deltas ADD COLUMN previous_chunk_tokens BIGINT NULL",
        ),
        (
            "previous_chunk_bytes",
            "ALTER TABLE prompt_deltas ADD COLUMN previous_chunk_bytes BIGINT NULL",
        ),
    ] {
        add_column_if_missing(&pool, &settings.database, "prompt_deltas", column, ddl).await?;
    }
    core_schema_create!(
        pool,
        "session_state_revisions",
        "CREATE TABLE IF NOT EXISTS session_state_revisions (
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            monotonic_id BIGINT NOT NULL DEFAULT 0,
            revision_hash VARCHAR(96) NOT NULL,
            device_fingerprint VARCHAR(128) NOT NULL,
            transcript_high_watermark BIGINT NOT NULL DEFAULT 0,
            run_event_high_watermark BIGINT NOT NULL DEFAULT 0,
            state_projection_hash VARCHAR(96) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id),
            INDEX idx_state_revisions_user_updated (user_id, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_state_revisions",
        &["user_id", "session_id"],
        "ALTER TABLE session_state_revisions ADD PRIMARY KEY (user_id, session_id)",
    )
    .await?;
    core_schema_create!(
        pool,
        "session_device_leases",
        "CREATE TABLE IF NOT EXISTS session_device_leases (
            lease_id VARCHAR(128) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            device_id VARCHAR(128) NOT NULL,
            device_fingerprint VARCHAR(128) NOT NULL,
            device_key_hash CHAR(64) NOT NULL,
            trust_level VARCHAR(32) NOT NULL DEFAULT 'new_device',
            status VARCHAR(32) NOT NULL DEFAULT 'active',
            last_monotonic_id BIGINT NOT NULL DEFAULT 0,
            expires_at DATETIME(6) NOT NULL,
            revoked_at DATETIME(6) NULL,
            request_id VARCHAR(128) NULL,
            trace_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, lease_id),
            UNIQUE KEY uq_session_device (user_id, session_id, device_id),
            INDEX idx_device_leases_user_session (user_id, session_id, status, updated_at),
            INDEX idx_device_leases_fingerprint (user_id, device_fingerprint, status),
            INDEX idx_device_leases_expiry (status, expires_at)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "session_device_leases",
        &[
            "lease_id",
            "user_id",
            "session_id",
            "device_id",
            "device_fingerprint",
            "device_key_hash",
            "trust_level",
            "status",
            "last_monotonic_id",
            "expires_at",
        ],
        &[],
        &[],
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_device_leases",
        &["user_id", "lease_id"],
        "ALTER TABLE session_device_leases ADD PRIMARY KEY (user_id, lease_id)",
    )
    .await?;
    core_schema_create!(
        pool,
        "session_device_challenges",
        "CREATE TABLE IF NOT EXISTS session_device_challenges (
            challenge_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            device_id VARCHAR(128) NOT NULL,
            device_fingerprint VARCHAR(128) NOT NULL,
            purpose VARCHAR(32) NOT NULL,
            challenge_digest CHAR(64) NOT NULL,
            expires_at DATETIME(6) NOT NULL,
            consumed_at DATETIME(6) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, challenge_id),
            INDEX idx_device_challenges_owner_device
                (user_id, session_id, device_id, created_at),
            INDEX idx_device_challenges_expiry (expires_at, consumed_at)
        )",
    )
    .execute(&pool)
    .await?;
    core_schema_create!(pool, "session_device_lease_events",
        "CREATE TABLE IF NOT EXISTS session_device_lease_events (
            lease_event_id VARCHAR(128) NOT NULL,
            lease_id VARCHAR(128) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            device_id VARCHAR(128) NOT NULL,
            device_fingerprint VARCHAR(128) NOT NULL,
            event_type VARCHAR(64) NOT NULL,
            reason VARCHAR(64) NOT NULL,
            ended_at_server DATETIME(6) NOT NULL,
            request_id VARCHAR(128) NULL,
            trace_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, lease_event_id),
            INDEX idx_lease_events_user_created (user_id, created_at),
            INDEX idx_lease_events_owner_session_device (user_id, session_id, device_id, created_at),
            INDEX idx_lease_events_type_created (event_type, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "session_device_lease_events",
        "idx_lease_events_session_device",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_device_lease_events",
        "idx_lease_events_owner_session_device",
        &["user_id", "session_id", "device_id", "created_at"],
        "ALTER TABLE session_device_lease_events ADD INDEX idx_lease_events_owner_session_device (user_id, session_id, device_id, created_at)",
    )
    .await?;

    // ── Sweeper leader election (prevents duplicate background work in
    // multi-pod deployments). One row per sweeper type; pods CAS-update
    // the lease every TTL/2 seconds. Only the lease holder runs work.
    // Table created idempotently via IF NOT EXISTS — no DROP, no data loss.
    core_schema_create!(
        pool,
        "sweeper_leases",
        "CREATE TABLE IF NOT EXISTS sweeper_leases (
            sweeper_name VARCHAR(128) PRIMARY KEY,
            owner_pod_id VARCHAR(256) NOT NULL,
            expires_at DATETIME(6) NOT NULL,
            version INT UNSIGNED NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        )",
    )
    .execute(&pool)
    .await?;

    // Durable keyset cursors keep bounded maintenance jobs fair across pod
    // changes and process restarts. The cursor is progress, never authority:
    // every mutation performed by a sweeper remains independently idempotent.
    core_schema_create!(
        pool,
        "maintenance_sweep_cursors",
        "CREATE TABLE IF NOT EXISTS maintenance_sweep_cursors (
            sweep_name VARCHAR(128) PRIMARY KEY,
            cursor_updated_at DATETIME(6) NOT NULL,
            cursor_user_id VARCHAR(128) NOT NULL,
            cursor_run_id VARCHAR(64) NOT NULL,
            scan_generation BIGINT UNSIGNED NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        )",
    )
    .execute(&pool)
    .await?;

    // ── Context manifest v1 (Phase 3 / G1+G3+G10+G26+G27) ───────────────
    core_schema_create!(pool, "context_manifests",
        "CREATE TABLE IF NOT EXISTS context_manifests (
            manifest_id VARCHAR(128) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            run_id VARCHAR(128) NULL,
            turn_id VARCHAR(128) NOT NULL,
            model_provider VARCHAR(64) NOT NULL,
            model_name VARCHAR(128) NOT NULL,
            context_window_tokens INT NOT NULL,
            max_output_tokens INT NOT NULL,
            total_estimated_tokens INT NOT NULL,
            stable_prefix_hash VARCHAR(128) NULL,
            prompt_cache_key VARCHAR(255) NULL,
            compaction_version VARCHAR(64) NULL,
            policy_version VARCHAR(64) NOT NULL,
            tokenizer_id VARCHAR(128) NULL,
            budget_template_id VARCHAR(64) NULL,
            turn_intent VARCHAR(64) NULL,
            reason VARCHAR(64) NOT NULL,
            dropped_count INT NOT NULL DEFAULT 0,
            manifest_json LONGTEXT NOT NULL,
            request_id VARCHAR(128) NULL,
            trace_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_ctx_manifest_owner_session_created (user_id, session_id, created_at, manifest_id),
            INDEX idx_ctx_manifest_owner_session_run_created (user_id, session_id, run_id, created_at, manifest_id),
            INDEX idx_ctx_manifest_user_created (user_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in ["idx_ctx_manifest_session_turn", "idx_ctx_manifest_run"] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "context_manifests",
            removed_index,
        )
        .await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_ctx_manifest_owner_session_created",
            &["user_id", "session_id", "created_at", "manifest_id"][..],
            "ALTER TABLE context_manifests ADD INDEX idx_ctx_manifest_owner_session_created (user_id, session_id, created_at, manifest_id)",
        ),
        (
            "idx_ctx_manifest_owner_session_run_created",
            &[
                "user_id",
                "session_id",
                "run_id",
                "created_at",
                "manifest_id",
            ][..],
            "ALTER TABLE context_manifests ADD INDEX idx_ctx_manifest_owner_session_run_created (user_id, session_id, run_id, created_at, manifest_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "context_manifests",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }
    core_schema_create!(
        pool,
        "context_manifest_items",
        "CREATE TABLE IF NOT EXISTS context_manifest_items (
            manifest_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            item_order INT NOT NULL,
            zone VARCHAR(64) NOT NULL,
            source_table VARCHAR(64) NOT NULL,
            source_id VARCHAR(128) NOT NULL,
            source_hash VARCHAR(128) NULL,
            included SMALLINT NOT NULL,
            token_estimate INT NOT NULL DEFAULT 0,
            budget_tokens INT NOT NULL DEFAULT 0,
            reason VARCHAR(128) NOT NULL,
            render_mode VARCHAR(64) NOT NULL,
            raw_ref VARCHAR(255) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (manifest_id, item_order),
            INDEX idx_manifest_items_source (source_table, source_id),
            INDEX idx_manifest_items_manifest_zone (manifest_id, zone, included),
            INDEX idx_manifest_items_raw_ref (raw_ref)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "context_manifest_items",
        &["manifest_id", "item_order"],
        &["id"],
        &["uq_manifest_item_order"],
    )
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "context_manifest_items",
        "idx_manifest_items_session_zone",
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "context_manifest_items",
        &["manifest_id", "item_order"],
        "ALTER TABLE context_manifest_items ADD PRIMARY KEY (manifest_id, item_order)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "context_manifest_items",
        "idx_manifest_items_manifest_zone",
        &["manifest_id", "zone", "included"],
        "ALTER TABLE context_manifest_items ADD INDEX idx_manifest_items_manifest_zone (manifest_id, zone, included)",
    )
    .await?;

    core_schema_create!(
        pool,
        "preview_template_registry",
        "CREATE TABLE IF NOT EXISTS preview_template_registry (
            tool_name VARCHAR(128) NOT NULL,
            version VARCHAR(64) NOT NULL,
            status VARCHAR(32) NOT NULL DEFAULT 'active',
            max_preview_bytes INT NOT NULL DEFAULT 400,
            default_chunk_type VARCHAR(64) NOT NULL DEFAULT 'tool_output_preview',
            first_class_columns_json LONGTEXT NOT NULL,
            fts_field_weights_json LONGTEXT NOT NULL,
            normalize_version VARCHAR(32) NOT NULL DEFAULT 'v1',
            schema_json LONGTEXT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (tool_name, version),
            INDEX idx_preview_templates_status (tool_name, status, updated_at)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "raw_ref_scheme_registry",
        "CREATE TABLE IF NOT EXISTS raw_ref_scheme_registry (
            scheme VARCHAR(64) PRIMARY KEY,
            resolver_name VARCHAR(128) NOT NULL,
            backing_store VARCHAR(64) NOT NULL,
            access_check VARCHAR(64) NOT NULL,
            canonical_example VARCHAR(255) NOT NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        )",
    )
    .execute(&pool)
    .await?;

    for (scheme, resolver, backing, access_check, example) in [
        (
            "artifact",
            "artifact_resolver",
            "matrixone",
            "session_artifact_acl",
            "artifact://session/artifact_id@sha256:...",
        ),
        (
            "s3",
            "s3_resolver",
            "object_store",
            "presigned_url_acl",
            "s3://bucket/key@sha256:...",
        ),
        (
            "conversation_log",
            "conversation_log_resolver",
            "matrixone",
            "session_owner",
            "conversation_log://session/item_seq@sha256:...",
        ),
        (
            "object_store",
            "object_store_resolver",
            "object_store",
            "object_acl",
            "object_store://namespace/key@sha256:...",
        ),
        (
            "cold_storage",
            "cold_storage_resolver",
            "cold_storage",
            "archive_acl",
            "cold_storage://archive/key@sha256:...",
        ),
        (
            "blob",
            "blob_resolver",
            "matrixone",
            "blob_acl",
            "blob://table/blob_id@sha256:...",
        ),
        (
            "tool_output",
            "tool_output_resolver",
            "matrixone",
            "session_owner",
            "tool_output://session/output_id@sha256:...",
        ),
        (
            "chunk",
            "history_chunk_resolver",
            "matrixone",
            "session_or_user_scope",
            "chunk://session/chunk_id@sha256:...",
        ),
        (
            "state_item",
            "state_item_resolver",
            "matrixone",
            "session_or_user_scope",
            "state_item://session/item_id@sha256:...",
        ),
    ] {
        query(
            "INSERT IGNORE INTO raw_ref_scheme_registry
             (scheme, resolver_name, backing_store, access_check, canonical_example, is_active, created_at)
             VALUES (?, ?, ?, ?, ?, 1, NOW(6))",
        )
        .bind(scheme)
        .bind(resolver)
        .bind(backing)
        .bind(access_check)
        .bind(example)
        .execute(&pool)
        .await?;
    }

    for (tool_name, max_preview_bytes, normalize_version) in
        crate::context_manifest::BASELINE_PREVIEW_TEMPLATES
    {
        let fts_field_weights =
            crate::context_manifest::preview_template_fts_field_weights(normalize_version);
        query(
            "INSERT IGNORE INTO preview_template_registry
             (tool_name, version, status, max_preview_bytes, default_chunk_type,
              first_class_columns_json, fts_field_weights_json, normalize_version, schema_json,
              created_at, updated_at)
             VALUES (?, 'v1', 'active', ?, 'tool_output_preview', '[]', ?, ?, '{}', NOW(6), NOW(6))",
        )
        .bind(tool_name)
        .bind(i64::from(*max_preview_bytes))
        .bind(fts_field_weights)
        .bind(normalize_version)
        .execute(&pool)
        .await?;

        query(
            "UPDATE preview_template_registry
             SET fts_field_weights_json = ?, updated_at = NOW(6)
             WHERE tool_name = ? AND version = 'v1' AND fts_field_weights_json = '{}'",
        )
        .bind(fts_field_weights)
        .bind(tool_name)
        .execute(&pool)
        .await?;
    }

    // ── State projection v1 (Phase 4 / G2+G4+G5+G6+G14+G16+G20) ────────
    core_schema_create!(pool, "session_state_items",
        "CREATE TABLE IF NOT EXISTS session_state_items (
            item_id VARCHAR(128) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            scope VARCHAR(32) NOT NULL DEFAULT 'session',
            category VARCHAR(64) NOT NULL,
            item_key VARCHAR(255) NOT NULL,
            status VARCHAR(32) NOT NULL DEFAULT 'active',
            priority INT NOT NULL DEFAULT 0,
            source VARCHAR(64) NOT NULL,
            provenance_event_id VARCHAR(128) NULL,
            run_id VARCHAR(128) NULL,
            title VARCHAR(255) NULL,
            summary_text TEXT NULL,
            payload_json LONGTEXT NULL,
            payload_hash VARCHAR(128) NULL,
            token_estimate INT NOT NULL DEFAULT 0,
            version BIGINT NOT NULL DEFAULT 1,
            origin_session_id VARCHAR(128) NULL,
            origin_chunk_id VARCHAR(128) NULL,
            origin_state_item_id VARCHAR(128) NULL,
            expires_at DATETIME(6) NULL,
            request_id VARCHAR(128) NULL,
            trace_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_session_state_items_scope CHECK (scope IN ('session', 'user', 'project', 'workspace')),
            UNIQUE KEY uq_state_current (user_id, session_id, scope, category, item_key),
            INDEX idx_state_owner_session_status_category (user_id, session_id, status, category),
            INDEX idx_state_user_category (user_id, category, status, updated_at),
            INDEX idx_state_user_scope_category (user_id, scope, category, status, priority),
            INDEX idx_state_owner_origin_session (user_id, origin_session_id, category, status),
            INDEX idx_state_expires (expires_at),
            INDEX idx_state_provenance (provenance_event_id)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in ["idx_state_session_category", "idx_state_origin_session"] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "session_state_items",
            removed_index,
        )
        .await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_state_owner_session_status_category",
            &["user_id", "session_id", "status", "category"][..],
            "ALTER TABLE session_state_items ADD INDEX idx_state_owner_session_status_category (user_id, session_id, status, category)",
        ),
        (
            "idx_state_user_scope_category",
            &["user_id", "scope", "category", "status", "priority"][..],
            "ALTER TABLE session_state_items ADD INDEX idx_state_user_scope_category (user_id, scope, category, status, priority)",
        ),
        (
            "idx_state_owner_origin_session",
            &["user_id", "origin_session_id", "category", "status"][..],
            "ALTER TABLE session_state_items ADD INDEX idx_state_owner_origin_session (user_id, origin_session_id, category, status)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_state_items",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }
    core_schema_create!(pool, "session_state_item_events",
        "CREATE TABLE IF NOT EXISTS session_state_item_events (
            event_id VARCHAR(64) NOT NULL,
            item_id VARCHAR(128) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            category VARCHAR(64) NOT NULL,
            item_key VARCHAR(255) NOT NULL,
            mutation VARCHAR(32) NOT NULL,
            previous_hash VARCHAR(128) NULL,
            next_hash VARCHAR(128) NULL,
            previous_version BIGINT NULL,
            next_version BIGINT NULL,
            payload_json LONGTEXT NULL,
            provenance_event_id VARCHAR(128) NULL,
            request_id VARCHAR(128) NULL,
            trace_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_state_item_event_mutation CHECK (mutation IN ('insert', 'update', 'replace', 'archive', 'delete', 'bubble_up', 'apply_suggestion', 'activate')),
            PRIMARY KEY (user_id, event_id),
            INDEX idx_state_events_item_created (item_id, created_at, event_id),
            INDEX idx_state_events_owner_session_created (user_id, session_id, created_at, event_id),
            INDEX idx_state_events_category_created (category, created_at, event_id)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "session_state_item_events",
        &["user_id", "event_id"],
        &["id"],
        &[],
    )
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "session_state_item_events",
        "idx_state_events_session_created",
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_state_item_events",
        &["user_id", "event_id"],
        "ALTER TABLE session_state_item_events ADD PRIMARY KEY (user_id, event_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_state_item_events",
        "idx_state_events_item_created",
        &["item_id", "created_at", "event_id"],
        "ALTER TABLE session_state_item_events ADD INDEX idx_state_events_item_created (item_id, created_at, event_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_state_item_events",
        "idx_state_events_owner_session_created",
        &["user_id", "session_id", "created_at", "event_id"],
        "ALTER TABLE session_state_item_events ADD INDEX idx_state_events_owner_session_created (user_id, session_id, created_at, event_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_state_item_events",
        "idx_state_events_category_created",
        &["category", "created_at", "event_id"],
        "ALTER TABLE session_state_item_events ADD INDEX idx_state_events_category_created (category, created_at, event_id)",
    )
    .await?;

    core_schema_create!(pool, "session_delegations",
        "CREATE TABLE IF NOT EXISTS session_delegations (
            delegation_id VARCHAR(128) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            parent_run_id VARCHAR(128) NOT NULL,
            child_run_id VARCHAR(128) NOT NULL,
            root_run_id VARCHAR(128) NOT NULL,
            ancestor_path VARCHAR(2048) NOT NULL,
            depth INT NOT NULL DEFAULT 0,
            agent_id VARCHAR(255) NULL,
            title VARCHAR(255) NULL,
            status VARCHAR(32) NOT NULL DEFAULT 'running',
            retry_of VARCHAR(128) NULL,
            retry_scope VARCHAR(32) NOT NULL DEFAULT 'node',
            last_summary_ref VARCHAR(255) NULL,
            last_summary_text TEXT NULL,
            sibling_exposed_artifacts_json LONGTEXT NULL,
            request_id VARCHAR(128) NULL,
            trace_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_session_delegations_retry_scope CHECK (retry_scope IN ('node', 'subtree', 'siblings')),
            UNIQUE KEY uq_session_delegations_child (child_run_id),
            INDEX idx_delegations_owner_root_depth (user_id, root_run_id, depth, created_at),
            INDEX idx_delegations_owner_parent_status_updated (user_id, parent_run_id, status, updated_at),
            INDEX idx_delegations_owner_session_status (user_id, session_id, status, updated_at),
            INDEX idx_delegations_retry_of (retry_of)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in [
        "idx_delegations_root_depth",
        "idx_delegations_parent",
        "idx_delegations_session_status",
    ] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "session_delegations",
            removed_index,
        )
        .await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_delegations_owner_root_depth",
            &["user_id", "root_run_id", "depth", "created_at"][..],
            "ALTER TABLE session_delegations ADD INDEX idx_delegations_owner_root_depth (user_id, root_run_id, depth, created_at)",
        ),
        (
            "idx_delegations_owner_parent_status_updated",
            &["user_id", "parent_run_id", "status", "updated_at"][..],
            "ALTER TABLE session_delegations ADD INDEX idx_delegations_owner_parent_status_updated (user_id, parent_run_id, status, updated_at)",
        ),
        (
            "idx_delegations_owner_session_status",
            &["user_id", "session_id", "status", "updated_at"][..],
            "ALTER TABLE session_delegations ADD INDEX idx_delegations_owner_session_status (user_id, session_id, status, updated_at)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_delegations",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    core_schema_create!(pool, "session_history_chunks",
        "CREATE TABLE IF NOT EXISTS session_history_chunks (
            chunk_id VARCHAR(128) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            source_session_id VARCHAR(128) NULL,
            seq_start BIGINT NOT NULL DEFAULT 0,
            seq_end BIGINT NOT NULL DEFAULT 0,
            item_seq_start BIGINT NULL,
            item_seq_end BIGINT NULL,
            turn_start BIGINT NULL,
            turn_end BIGINT NULL,
            chunk_type VARCHAR(64) NOT NULL,
            source_table VARCHAR(64) NOT NULL,
            source_id VARCHAR(128) NOT NULL,
            content_text LONGTEXT NOT NULL,
            content_hash VARCHAR(128) NOT NULL,
            token_estimate INT NOT NULL DEFAULT 0,
            provenance_json LONGTEXT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_history_user_chunk_created (user_id, chunk_type, created_at),
            INDEX idx_history_owner_session_seq (user_id, session_id, seq_start, seq_end),
            INDEX idx_history_owner_source_session (user_id, source_session_id, chunk_type, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in ["idx_history_session_seq", "idx_history_source_session"] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "session_history_chunks",
            removed_index,
        )
        .await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_history_owner_session_seq",
            &["user_id", "session_id", "seq_start", "seq_end"][..],
            "ALTER TABLE session_history_chunks ADD INDEX idx_history_owner_session_seq (user_id, session_id, seq_start, seq_end)",
        ),
        (
            "idx_history_owner_source_session",
            &["user_id", "source_session_id", "chunk_type", "created_at"][..],
            "ALTER TABLE session_history_chunks ADD INDEX idx_history_owner_source_session (user_id, source_session_id, chunk_type, created_at)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_history_chunks",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    query("DROP TABLE IF EXISTS session_artifact_grants")
        .execute(&pool)
        .await?;

    core_schema_create!(pool, "session_artifacts_grants",
        "CREATE TABLE IF NOT EXISTS session_artifacts_grants (
            grant_id VARCHAR(128) PRIMARY KEY,
            artifact_id VARCHAR(128) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            root_run_id VARCHAR(128) NOT NULL,
            source_run_id VARCHAR(128) NOT NULL,
            target_run_id VARCHAR(128) NULL,
            target_delegation_id VARCHAR(128) NULL,
            grant_scope VARCHAR(32) NOT NULL,
            granted_by VARCHAR(128) NOT NULL,
            reason VARCHAR(128) NULL,
            expires_at DATETIME(6) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_artifacts_grant_target (user_id, session_id, artifact_id, grant_scope, target_run_id, target_delegation_id),
            INDEX idx_artifacts_grants_root (user_id, root_run_id, grant_scope, created_at),
            INDEX idx_artifacts_grants_target (user_id, session_id, target_run_id, artifact_id, expires_at),
            INDEX idx_artifacts_grants_delegation_target (user_id, session_id, target_delegation_id, artifact_id, expires_at)
        )",
    )
    .execute(&pool)
    .await?;
    for (index, expected_columns, ddl) in [
        (
            "uq_artifacts_grant_target",
            &[
                "user_id",
                "session_id",
                "artifact_id",
                "grant_scope",
                "target_run_id",
                "target_delegation_id",
            ][..],
            "ALTER TABLE session_artifacts_grants ADD UNIQUE KEY uq_artifacts_grant_target (user_id, session_id, artifact_id, grant_scope, target_run_id, target_delegation_id)",
        ),
        (
            "idx_artifacts_grants_target",
            &[
                "user_id",
                "session_id",
                "target_run_id",
                "artifact_id",
                "expires_at",
            ][..],
            "ALTER TABLE session_artifacts_grants ADD INDEX idx_artifacts_grants_target (user_id, session_id, target_run_id, artifact_id, expires_at)",
        ),
        (
            "idx_artifacts_grants_delegation_target",
            &[
                "user_id",
                "session_id",
                "target_delegation_id",
                "artifact_id",
                "expires_at",
            ][..],
            "ALTER TABLE session_artifacts_grants ADD INDEX idx_artifacts_grants_delegation_target (user_id, session_id, target_delegation_id, artifact_id, expires_at)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_artifacts_grants",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "agent_event_edges",
        &[
            "user_id",
            "session_id",
            "child_event_id",
            "parent_event_id",
            "relation_kind",
            "parent_order",
        ],
        &[],
        &[
            "idx_agent_event_edges_child",
            "idx_agent_event_edges_parent",
        ],
    )
    .await?;
    fail_if_required_columns_missing_or_nullable(
        &pool,
        &settings.database,
        "agent_event_edges",
        &["user_id", "session_id"],
    )
    .await?;

    core_schema_create!(
        pool,
        "agent_event_edges",
        "CREATE TABLE IF NOT EXISTS agent_event_edges (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            child_event_id VARCHAR(128) NOT NULL,
            parent_event_id VARCHAR(128) NOT NULL,
            relation_kind VARCHAR(32) NOT NULL DEFAULT 'causal',
            parent_order INT NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, child_event_id, parent_event_id, relation_kind),
            INDEX idx_agent_event_edges_owner_session_child (user_id, session_id, child_event_id),
            INDEX idx_agent_event_edges_owner_child (user_id, child_event_id, parent_order),
            INDEX idx_agent_event_edges_owner_parent (user_id, parent_event_id, parent_order)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "agent_event_edges",
        &[
            ("child_event_id", AGENT_EVENT_ID_LEN as u64),
            ("parent_event_id", AGENT_EVENT_ID_LEN as u64),
        ],
    )
    .await?;

    // Harness diagnostic snapshots — separated from agent_events to avoid
    // polluting session event counts and to carry causal_chain_id natively.
    core_schema_create!(
        pool,
        "harness_snapshots",
        "CREATE TABLE IF NOT EXISTS harness_snapshots (
            snapshot_id VARCHAR(64) PRIMARY KEY,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            hook_point VARCHAR(32) NOT NULL,
            turn_number INT UNSIGNED NOT NULL DEFAULT 0,
            snapshot_json LONGTEXT NOT NULL,
            causal_chain_id VARCHAR(128),
            created_at DATETIME(6) DEFAULT NOW(6),
            INDEX idx_harness_owner_session_created (user_id, session_id, created_at),
            INDEX idx_harness_owner_session_turn (user_id, session_id, turn_number),
            INDEX idx_harness_owner_chain (user_id, causal_chain_id)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in [
        "idx_harness_session",
        "idx_harness_session_turn",
        "idx_harness_chain",
    ] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "harness_snapshots",
            removed_index,
        )
        .await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_harness_owner_session_created",
            &["user_id", "session_id", "created_at"][..],
            "ALTER TABLE harness_snapshots ADD INDEX idx_harness_owner_session_created (user_id, session_id, created_at)",
        ),
        (
            "idx_harness_owner_session_turn",
            &["user_id", "session_id", "turn_number"][..],
            "ALTER TABLE harness_snapshots ADD INDEX idx_harness_owner_session_turn (user_id, session_id, turn_number)",
        ),
        (
            "idx_harness_owner_chain",
            &["user_id", "causal_chain_id"][..],
            "ALTER TABLE harness_snapshots ADD INDEX idx_harness_owner_chain (user_id, causal_chain_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "harness_snapshots",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    // Product harness workflow state. This is separate from the diagnostic
    // `harness_snapshots` table above: these rows are the durable product model
    // for reusable user workflows such as Skillify.
    core_schema_create!(
        pool,
        "harness_runs",
        "CREATE TABLE IF NOT EXISTS harness_runs (
            harness_run_id VARCHAR(128) PRIMARY KEY,
            harness_id VARCHAR(128) NOT NULL,
            version_id VARCHAR(128) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NULL,
            workflow_run_id VARCHAR(128) NULL,
            agent_run_id VARCHAR(128) NULL,
            parent_agent_run_id VARCHAR(128) NULL,
            status VARCHAR(64) NOT NULL,
            input_json LONGTEXT NOT NULL,
            output_json LONGTEXT NOT NULL,
            error TEXT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_harness_runs_user_status_updated (user_id, status, updated_at),
            INDEX idx_harness_runs_harness_user (harness_id, user_id, updated_at),
            INDEX idx_harness_runs_owner_session_updated (user_id, session_id, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "harness_runs",
        "idx_harness_runs_session",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "harness_runs",
        "idx_harness_runs_owner_session_updated",
        &["user_id", "session_id", "updated_at"],
        "ALTER TABLE harness_runs ADD INDEX idx_harness_runs_owner_session_updated (user_id, session_id, updated_at)",
    )
    .await?;

    core_schema_create!(
        pool,
        "harness_items",
        "CREATE TABLE IF NOT EXISTS harness_items (
            item_id VARCHAR(128) PRIMARY KEY,
            harness_run_id VARCHAR(128) NOT NULL,
            parent_item_id VARCHAR(128) NULL,
            item_type VARCHAR(64) NOT NULL,
            locator_json LONGTEXT NOT NULL,
            input_json LONGTEXT NOT NULL,
            proposed_output_json LONGTEXT NOT NULL,
            final_output_json LONGTEXT NOT NULL,
            decision_history_json LONGTEXT NULL,
            status VARCHAR(64) NOT NULL,
            confidence DOUBLE NULL,
            assigned_to VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_harness_items_run_status (harness_run_id, status, updated_at),
            INDEX idx_harness_items_assigned (assigned_to, status, updated_at)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "harness_skill_drafts",
        "CREATE TABLE IF NOT EXISTS harness_skill_drafts (
            skill_draft_id VARCHAR(128) PRIMARY KEY,
            harness_run_id VARCHAR(128) NOT NULL,
            candidate_name VARCHAR(128) NOT NULL,
            description TEXT NOT NULL,
            target_scope VARCHAR(32) NOT NULL,
            publish_visibility VARCHAR(32) NOT NULL,
            content_markdown LONGTEXT NOT NULL,
            source_summary_json LONGTEXT NOT NULL,
            decision_history_json LONGTEXT NULL,
            status VARCHAR(64) NOT NULL,
            confidence DOUBLE NULL,
            created_by_node_id VARCHAR(128) NULL,
            revision BIGINT NOT NULL DEFAULT 1,
            published_version_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_harness_skill_drafts_run_status (harness_run_id, status, updated_at),
            INDEX idx_harness_skill_drafts_run_revision (harness_run_id, revision)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "harness_skill_rules",
        "CREATE TABLE IF NOT EXISTS harness_skill_rules (
            skill_rule_id VARCHAR(128) PRIMARY KEY,
            skill_draft_id VARCHAR(128) NOT NULL,
            harness_run_id VARCHAR(128) NOT NULL,
            rule_type VARCHAR(64) NOT NULL,
            statement TEXT NOT NULL,
            rationale TEXT NOT NULL,
            decision_history_json LONGTEXT NULL,
            status VARCHAR(64) NOT NULL,
            confidence DOUBLE NULL,
            source_count BIGINT NOT NULL DEFAULT 0,
            created_by_node_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_harness_skill_rules_draft_status (skill_draft_id, status, updated_at),
            INDEX idx_harness_skill_rules_run_status (harness_run_id, status, updated_at)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "harness_citations",
        "CREATE TABLE IF NOT EXISTS harness_citations (
            citation_id VARCHAR(128) PRIMARY KEY,
            harness_run_id VARCHAR(128) NOT NULL,
            item_id VARCHAR(128) NOT NULL,
            skill_draft_id VARCHAR(128) NULL,
            skill_rule_id VARCHAR(128) NULL,
            source_id VARCHAR(128) NULL,
            source_locator_json LONGTEXT NOT NULL,
            source_snapshot_ref VARCHAR(128) NULL,
            source_content_hash VARCHAR(128) NULL,
            source_metadata_json LONGTEXT NULL,
            artifact_id VARCHAR(128) NULL,
            quote_hash VARCHAR(128) NULL,
            evidence_text_preview TEXT NULL,
            relevance_score DOUBLE NULL,
            created_by_node_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_harness_citations_item (item_id, created_at),
            INDEX idx_harness_citations_skill_rule (skill_rule_id, created_at),
            INDEX idx_harness_citations_run (harness_run_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    // Context / decisions / evaluation essentials used by turn persistence
    core_schema_create!(
        pool,
        "ctx_snapshots",
        "CREATE TABLE IF NOT EXISTS ctx_snapshots (
            context_capture_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            event_id VARCHAR(128) NOT NULL,
            context_data JSON NULL,
            llm_request_id VARCHAR(64) NULL,
            llm_response_id VARCHAR(64) NULL,
            token_budget INT NULL,
            total_tokens BIGINT NULL,
            assembly_time_ms BIGINT NULL,
            relevance_scores JSON NULL,
            token_usage JSON NULL,
            task_type VARCHAR(64) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_ctx_snapshots_owner_session_created (user_id, session_id, created_at),
            INDEX idx_ctx_snapshots_owner_event_id (user_id, event_id)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "ctx_snapshots",
        &[("event_id", AGENT_EVENT_ID_LEN as u64)],
    )
    .await?;
    for removed_index in [
        "idx_ctx_snapshots_session_created",
        "idx_ctx_snapshots_event_id",
    ] {
        drop_index_if_present(&pool, &settings.database, "ctx_snapshots", removed_index).await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_ctx_snapshots_owner_session_created",
            &["user_id", "session_id", "created_at"][..],
            "ALTER TABLE ctx_snapshots ADD INDEX idx_ctx_snapshots_owner_session_created (user_id, session_id, created_at)",
        ),
        (
            "idx_ctx_snapshots_owner_event_id",
            &["user_id", "event_id"][..],
            "ALTER TABLE ctx_snapshots ADD INDEX idx_ctx_snapshots_owner_event_id (user_id, event_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "ctx_snapshots",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    core_schema_create!(pool, "ctx_decision_audits",
        "CREATE TABLE IF NOT EXISTS ctx_decision_audits (
            decision_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            event_id VARCHAR(128) NULL,
            context_capture_id VARCHAR(64) NULL,
            decision_type VARCHAR(64) NOT NULL,
            decision_output JSON NULL,
            model_params JSON NULL,
            model_used VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_ctx_decisions_owner_session_type_created (user_id, session_id, decision_type, created_at),
            INDEX idx_ctx_decisions_owner_event_id (user_id, event_id),
            INDEX idx_ctx_decisions_owner_context_capture_id (user_id, context_capture_id)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "ctx_decision_audits",
        &[("event_id", AGENT_EVENT_ID_LEN as u64)],
    )
    .await?;
    for removed_index in [
        "idx_ctx_decisions_session_type_created",
        "idx_ctx_decisions_event_id",
        "idx_ctx_decisions_context_capture_id",
    ] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "ctx_decision_audits",
            removed_index,
        )
        .await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_ctx_decisions_owner_session_type_created",
            &["user_id", "session_id", "decision_type", "created_at"][..],
            "ALTER TABLE ctx_decision_audits ADD INDEX idx_ctx_decisions_owner_session_type_created (user_id, session_id, decision_type, created_at)",
        ),
        (
            "idx_ctx_decisions_owner_event_id",
            &["user_id", "event_id"][..],
            "ALTER TABLE ctx_decision_audits ADD INDEX idx_ctx_decisions_owner_event_id (user_id, event_id)",
        ),
        (
            "idx_ctx_decisions_owner_context_capture_id",
            &["user_id", "context_capture_id"][..],
            "ALTER TABLE ctx_decision_audits ADD INDEX idx_ctx_decisions_owner_context_capture_id (user_id, context_capture_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "ctx_decision_audits",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    fail_if_required_columns_missing_or_nullable(
        &pool,
        &settings.database,
        "skill_selection_events",
        &["user_id"],
    )
    .await?;
    core_schema_create!(
        pool,
        "skill_selection_events",
        "CREATE TABLE IF NOT EXISTS skill_selection_events (
            event_id VARCHAR(64) PRIMARY KEY,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            agent_id VARCHAR(255) NULL,
            user_query LONGTEXT NULL,
            selected_skills JSON NULL,
            skill_name VARCHAR(255) NULL,
            skill_version VARCHAR(64) NULL,
            selection_method VARCHAR(64) NULL,
            execution_success BIGINT NULL,
            execution_time_ms BIGINT NULL,
            user_feedback_score BIGINT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_skill_selection_owner_session_created (user_id, session_id, created_at),
            INDEX idx_skill_selection_user_created (user_id, created_at),
            INDEX idx_skill_selection_skill_created (skill_name, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "skill_selection_events",
        "idx_skill_selection_session_created",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "skill_selection_events",
        "idx_skill_selection_owner_session_created",
        &["user_id", "session_id", "created_at"],
        "ALTER TABLE skill_selection_events ADD INDEX idx_skill_selection_owner_session_created (user_id, session_id, created_at)",
    )
    .await?;

    core_schema_create!(
        pool,
        "infra_llm_models",
        "CREATE TABLE IF NOT EXISTS infra_llm_models (
            model_id VARCHAR(64) PRIMARY KEY,
            model_name VARCHAR(100) NOT NULL UNIQUE,
            provider VARCHAR(50) NOT NULL,
            api_key_encrypted TEXT NULL,
            base_url VARCHAR(500) NULL,
            description TEXT NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            context_window INT NOT NULL,
            max_completion_tokens INT NULL,
            input_modalities JSON NOT NULL,
            output_modalities JSON NOT NULL,
            supported_parameters JSON NOT NULL,
            pricing JSON NOT NULL,
            architecture VARCHAR(100) NULL,
            tags JSON NOT NULL,
            quirks JSON NOT NULL,
            thinking_capability VARCHAR(20) NULL,
            thinking_probe_error TEXT NULL,
            created_by VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_infra_llm_models_context_window CHECK (context_window > 0),
            INDEX idx_infra_llm_models_active_provider_name (is_active, provider, model_name)
        )",
    )
    .execute(&pool)
    .await?;

    // Canonical inference execution ledger. Admission writes the immutable route
    // and logical invocation together; each physical attempt is then committed
    // before its provider I/O. Route rows contain no credential or endpoint
    // material and remain safe to project after execution.
    core_schema_create!(pool, "inference_routes",
        "CREATE TABLE IF NOT EXISTS inference_routes (
            route_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NULL,
            scope_kind VARCHAR(16) NOT NULL,
            run_id VARCHAR(64) NULL,
            harness_run_id VARCHAR(128) NULL,
            offering_id VARCHAR(64) NOT NULL,
            resolved_model_name VARCHAR(255) NOT NULL,
            upstream_model_name VARCHAR(255) NOT NULL,
            provider VARCHAR(64) NOT NULL,
            execution_placement VARCHAR(32) NOT NULL,
            access_kind VARCHAR(32) NOT NULL,
            purpose VARCHAR(64) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, route_id),
            CONSTRAINT chk_inference_routes_scope_kind
                CHECK (scope_kind IN ('run', 'session', 'harness_run')),
            CONSTRAINT chk_inference_routes_scope_owner
                CHECK ((scope_kind = 'run' AND session_id IS NOT NULL
                        AND run_id IS NOT NULL AND harness_run_id IS NULL)
                    OR (scope_kind = 'session' AND session_id IS NOT NULL
                        AND run_id IS NULL AND harness_run_id IS NULL)
                    OR (scope_kind = 'harness_run' AND session_id IS NULL
                        AND run_id IS NULL AND harness_run_id IS NOT NULL)),
            INDEX idx_inference_routes_owner_session_created (user_id, session_id, created_at, route_id),
            INDEX idx_inference_routes_owner_run_created (user_id, run_id, created_at, route_id),
            INDEX idx_inference_routes_owner_harness_created (user_id, harness_run_id, created_at, route_id),
            INDEX idx_inference_routes_offering_created (offering_id, created_at, route_id)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(pool, "inference_invocations",
        "CREATE TABLE IF NOT EXISTS inference_invocations (
            invocation_id VARCHAR(64) NOT NULL,
            route_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NULL,
            scope_kind VARCHAR(16) NOT NULL,
            run_id VARCHAR(64) NULL,
            harness_run_id VARCHAR(128) NULL,
            admission_token CHAR(32) NOT NULL,
            owner_token CHAR(32) NOT NULL,
            owner_generation BIGINT NOT NULL,
            owner_lease_expires_at DATETIME(6) NOT NULL,
            turn_index BIGINT NULL,
            round_index BIGINT NULL,
            operation_id VARCHAR(64) NOT NULL,
            logical_attempt BIGINT NOT NULL,
            purpose VARCHAR(64) NOT NULL,
            status VARCHAR(32) NOT NULL,
            terminal_fingerprint CHAR(64) NULL,
            usage_status VARCHAR(32) NOT NULL,
            provider_delivery_state VARCHAR(32) NOT NULL,
            input_tokens BIGINT NOT NULL DEFAULT 0,
            output_tokens BIGINT NOT NULL DEFAULT 0,
            cache_read_tokens BIGINT NOT NULL DEFAULT 0,
            cache_creation_tokens BIGINT NOT NULL DEFAULT 0,
            provider_response_id VARCHAR(255) NULL,
            error_kind VARCHAR(64) NULL,
            error_message TEXT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            terminal_at DATETIME(6) NULL,
            PRIMARY KEY (user_id, invocation_id),
            CONSTRAINT chk_inference_invocations_scope_kind
                CHECK (scope_kind IN ('run', 'session', 'harness_run')),
            CONSTRAINT chk_inference_invocations_scope_owner
                CHECK ((scope_kind = 'run' AND session_id IS NOT NULL
                        AND run_id IS NOT NULL AND harness_run_id IS NULL
                        AND turn_index IS NOT NULL AND round_index IS NOT NULL)
                    OR (scope_kind = 'session' AND session_id IS NOT NULL
                        AND run_id IS NULL AND harness_run_id IS NULL
                        AND turn_index IS NOT NULL AND round_index IS NOT NULL)
                    OR (scope_kind = 'harness_run' AND session_id IS NULL
                        AND run_id IS NULL AND harness_run_id IS NOT NULL
                        AND turn_index IS NULL AND round_index IS NULL)),
            CONSTRAINT chk_inference_invocations_status
                CHECK (status IN ('admitted', 'succeeded', 'failed', 'cancelled', 'delivery_unknown')),
            CONSTRAINT chk_inference_invocations_usage_status
                CHECK (usage_status IN ('provider_exact', 'provider_partial', 'unavailable')),
            CONSTRAINT chk_inference_invocations_delivery_state
                CHECK (provider_delivery_state IN ('unknown', 'pre_delivery', 'delivery_authorized')),
            UNIQUE KEY uq_inference_invocation_route (user_id, route_id),
            INDEX idx_inference_invocations_owner_session_created (user_id, session_id, created_at, invocation_id),
            INDEX idx_inference_invocations_owner_run_created (user_id, run_id, created_at, invocation_id),
            INDEX idx_inference_invocations_owner_harness_created (user_id, harness_run_id, created_at, invocation_id),
            INDEX idx_inference_invocations_logical_cursor
                (user_id, scope_kind, session_id, run_id, harness_run_id,
                 turn_index, round_index, operation_id, purpose, logical_attempt),
            INDEX idx_inference_invocations_owner_lease
                (owner_lease_expires_at, user_id, invocation_id)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "inference_invocations",
        "idx_inference_invocations_logical_cursor",
        &[
            "user_id",
            "scope_kind",
            "session_id",
            "run_id",
            "harness_run_id",
            "turn_index",
            "round_index",
            "operation_id",
            "purpose",
            "logical_attempt",
        ],
        "ALTER TABLE inference_invocations ADD INDEX idx_inference_invocations_logical_cursor (user_id, scope_kind, session_id, run_id, harness_run_id, turn_index, round_index, operation_id, purpose, logical_attempt)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "inference_invocations",
        "idx_inference_invocations_owner_lease",
        &["owner_lease_expires_at", "user_id", "invocation_id"],
        "ALTER TABLE inference_invocations ADD INDEX idx_inference_invocations_owner_lease (owner_lease_expires_at, user_id, invocation_id)",
    )
    .await?;

    core_schema_create!(pool, "inference_provider_attempts",
        "CREATE TABLE IF NOT EXISTS inference_provider_attempts (
            attempt_id VARCHAR(64) NOT NULL,
            invocation_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NULL,
            run_id VARCHAR(64) NULL,
            harness_run_id VARCHAR(128) NULL,
            attempt_index BIGINT NOT NULL,
            provider VARCHAR(64) NOT NULL,
            admission_token CHAR(32) NOT NULL,
            provider_protocol VARCHAR(32) NOT NULL,
            provider_wire_hash CHAR(64) NOT NULL,
            provider_wire_bytes BIGINT NOT NULL,
            canonical_transition_id CHAR(64) NULL,
            canonical_parent_transition_id CHAR(64) NULL,
            canonical_transition_hash CHAR(64) NULL,
            status VARCHAR(32) NOT NULL,
            terminal_fingerprint CHAR(64) NULL,
            usage_status VARCHAR(32) NOT NULL,
            input_tokens BIGINT NOT NULL DEFAULT 0,
            output_tokens BIGINT NOT NULL DEFAULT 0,
            cache_read_tokens BIGINT NOT NULL DEFAULT 0,
            cache_creation_tokens BIGINT NOT NULL DEFAULT 0,
            provider_response_id VARCHAR(255) NULL,
            error_kind VARCHAR(64) NULL,
            error_message TEXT NULL,
            started_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            terminal_at DATETIME(6) NULL,
            context_expired_at DATETIME(6) NULL,
            PRIMARY KEY (user_id, attempt_id),
            CONSTRAINT chk_inference_provider_attempts_scope_owner
                CHECK ((session_id IS NOT NULL AND harness_run_id IS NULL)
                    OR (session_id IS NULL AND run_id IS NULL
                        AND harness_run_id IS NOT NULL)),
            CONSTRAINT chk_inference_provider_attempts_status
                CHECK (status IN ('started', 'succeeded', 'failed', 'cancelled', 'delivery_unknown')),
            CONSTRAINT chk_inference_provider_attempts_usage_status
                CHECK (usage_status IN ('provider_exact', 'provider_partial', 'unavailable')),
            CONSTRAINT chk_inference_provider_attempts_wire
                CHECK (provider_protocol IN ('openai_compatible', 'anthropic_messages', 'bedrock_converse')
                    AND provider_wire_bytes > 0),
            UNIQUE KEY uq_inference_provider_attempt (user_id, invocation_id, attempt_index),
            INDEX idx_inference_attempts_owner_session_started (user_id, session_id, started_at, attempt_id),
            INDEX idx_inference_attempts_owner_run_started (user_id, run_id, started_at, attempt_id),
            INDEX idx_inference_attempts_owner_harness_started (user_id, harness_run_id, started_at, attempt_id)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "inference_canonical_transition_heads",
        "CREATE TABLE IF NOT EXISTS inference_canonical_transition_heads (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            turn_index BIGINT NOT NULL,
            head_transition_id CHAR(64) NOT NULL,
            head_attempt_id VARCHAR(64) NOT NULL,
            head_result_count BIGINT NOT NULL,
            head_result_root_hash CHAR(64) NOT NULL,
            chain_length BIGINT NOT NULL,
            chain_payload_bytes BIGINT NOT NULL,
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, turn_index),
            UNIQUE KEY uq_inference_canonical_head_attempt (user_id, head_attempt_id)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "inference_canonical_transition_wal",
        "CREATE TABLE IF NOT EXISTS inference_canonical_transition_wal (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            turn_index BIGINT NOT NULL,
            round_index BIGINT NOT NULL,
            logical_attempt BIGINT NOT NULL,
            physical_attempt BIGINT NOT NULL,
            transition_id CHAR(64) NOT NULL,
            parent_transition_id CHAR(64) NULL,
            attempt_id VARCHAR(64) NOT NULL,
            payload_json LONGTEXT NOT NULL,
            payload_hash CHAR(64) NOT NULL,
            payload_bytes BIGINT NOT NULL,
            predecessor_count BIGINT NOT NULL,
            predecessor_root_hash CHAR(64) NOT NULL,
            result_count BIGINT NOT NULL,
            result_root_hash CHAR(64) NOT NULL,
            recovery_mode VARCHAR(32) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, turn_index, transition_id),
            UNIQUE KEY uq_inference_canonical_wal_attempt (user_id, attempt_id),
            INDEX idx_inference_canonical_wal_parent
                (user_id, session_id, turn_index, parent_transition_id),
            CONSTRAINT chk_inference_canonical_wal_recovery_mode
                CHECK (recovery_mode IN ('append_from_durable_base', 'replace_from_durable_base')),
            CONSTRAINT chk_inference_canonical_wal_bounds
                CHECK (payload_bytes > 0 AND predecessor_count >= 0 AND result_count >= 0)
        )",
    )
    .execute(&pool)
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "inference_provider_attempts",
        "context_expired_at",
        "ALTER TABLE inference_provider_attempts ADD COLUMN context_expired_at DATETIME(6) NULL",
    )
    .await?;

    core_schema_create!(
        pool,
        "model_request_context_events",
        "CREATE TABLE IF NOT EXISTS model_request_context_events (
            event_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            attempt_id VARCHAR(64) NOT NULL,
            invocation_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NULL,
            run_id VARCHAR(64) NULL,
            harness_run_id VARCHAR(128) NULL,
            event_stage VARCHAR(16) NOT NULL,
            terminal_status VARCHAR(32) NULL,
            topology VARCHAR(32) NOT NULL,
            provider VARCHAR(64) NOT NULL,
            model_family VARCHAR(128) NOT NULL,
            purpose VARCHAR(64) NOT NULL,
            input_tokens BIGINT NULL,
            output_tokens BIGINT NULL,
            cache_read_tokens BIGINT NULL,
            cache_creation_tokens BIGINT NULL,
            event_json LONGTEXT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, event_id),
            CONSTRAINT chk_model_request_context_stage
                CHECK (event_stage IN ('accepted', 'terminal')),
            CONSTRAINT chk_model_request_context_terminal
                CHECK ((event_stage = 'accepted' AND terminal_status IS NULL)
                    OR (event_stage = 'terminal' AND terminal_status IN
                        ('succeeded', 'failed', 'cancelled', 'delivery_unknown'))),
            UNIQUE KEY uq_model_request_context_attempt_stage
                (user_id, attempt_id, event_stage),
            INDEX idx_model_request_context_owner_session_created
                (user_id, session_id, created_at, event_id),
            INDEX idx_model_request_context_owner_harness_created
                (user_id, harness_run_id, created_at, event_id),
            INDEX idx_model_request_context_created_event
                (created_at, event_id),
            INDEX idx_model_request_context_metrics
                (topology, provider, model_family, purpose, created_at),
            INDEX idx_model_request_context_terminal_status
                (terminal_status, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "model_request_context_events",
        "idx_model_request_context_created_event",
        &["created_at", "event_id"],
        "ALTER TABLE model_request_context_events ADD INDEX idx_model_request_context_created_event (created_at, event_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "model_request_context_events",
        "idx_model_request_context_owner_harness_created",
        &["user_id", "harness_run_id", "created_at", "event_id"],
        "ALTER TABLE model_request_context_events ADD INDEX idx_model_request_context_owner_harness_created (user_id, harness_run_id, created_at, event_id)",
    )
    .await?;

    // Terminal request metrics are accumulated transactionally with their
    // append-only event. Scrapers read this bounded low-cardinality
    // projection instead of repeatedly scanning every historical request.
    core_schema_create!(
        pool,
        "model_request_metric_shards",
        "CREATE TABLE IF NOT EXISTS model_request_metric_shards (
            metric_shard SMALLINT NOT NULL,
            topology VARCHAR(32) NOT NULL,
            provider VARCHAR(64) NOT NULL,
            model_family VARCHAR(128) NOT NULL,
            purpose VARCHAR(64) NOT NULL,
            terminal_status VARCHAR(32) NOT NULL,
            requests BIGINT NOT NULL DEFAULT 0,
            input_tokens BIGINT NOT NULL DEFAULT 0,
            output_tokens BIGINT NOT NULL DEFAULT 0,
            cache_read_tokens BIGINT NOT NULL DEFAULT 0,
            cache_creation_tokens BIGINT NOT NULL DEFAULT 0,
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY
                (metric_shard, topology, provider, model_family, purpose, terminal_status),
            CONSTRAINT chk_model_request_metric_shard
                CHECK (metric_shard >= 0 AND metric_shard < 64),
            CONSTRAINT chk_model_request_metric_terminal
                CHECK (terminal_status IN
                    ('succeeded', 'failed', 'cancelled', 'delivery_unknown')),
            CONSTRAINT chk_model_request_metric_nonnegative
                CHECK (requests >= 0 AND input_tokens >= 0 AND output_tokens >= 0
                    AND cache_read_tokens >= 0 AND cache_creation_tokens >= 0)
        )",
    )
    .execute(&pool)
    .await?;

    // A settlement debt is written by the logical lifecycle owner, or by a
    // successful provider attempt (which is itself a final fact). It is the
    // only recovery input: a failed physical attempt may still be retried and
    // therefore must never be inferred as a failed logical invocation.
    core_schema_create!(
        pool,
        "inference_invocation_settlement_debts",
        "CREATE TABLE IF NOT EXISTS inference_invocation_settlement_debts (
            user_id VARCHAR(128) NOT NULL,
            invocation_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NULL,
            harness_run_id VARCHAR(128) NULL,
            terminal_status VARCHAR(32) NOT NULL,
            terminal_fingerprint CHAR(64) NOT NULL,
            usage_status VARCHAR(32) NOT NULL DEFAULT 'unavailable',
            input_tokens BIGINT NOT NULL DEFAULT 0,
            output_tokens BIGINT NOT NULL DEFAULT 0,
            cache_read_tokens BIGINT NOT NULL DEFAULT 0,
            cache_creation_tokens BIGINT NOT NULL DEFAULT 0,
            provider_response_id VARCHAR(255) NULL,
            error_kind VARCHAR(64) NULL,
            error_message TEXT NULL,
            provider_attempt_id VARCHAR(64) NULL,
            provider_delivery_state VARCHAR(32) NOT NULL DEFAULT 'unknown',
            reconciliation_status VARCHAR(16) NOT NULL DEFAULT 'pending',
            quarantine_reason VARCHAR(255) NULL,
            next_retry_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, invocation_id),
            INDEX idx_inference_settlement_owner_session_created
                (user_id, session_id, created_at, invocation_id),
            INDEX idx_inference_settlement_owner_harness_created
                (user_id, harness_run_id, created_at, invocation_id),
            INDEX idx_inference_settlement_recovery_ready
                (reconciliation_status, next_retry_at, user_id, invocation_id),
            CONSTRAINT chk_inference_invocation_settlement_debts_scope_owner
                CHECK ((session_id IS NOT NULL AND harness_run_id IS NULL)
                    OR (session_id IS NULL AND harness_run_id IS NOT NULL)),
            CONSTRAINT chk_inference_invocation_settlement_debts_status
                CHECK (terminal_status IN ('succeeded', 'failed', 'cancelled', 'delivery_unknown')),
            CONSTRAINT chk_inference_invocation_settlement_debts_usage_status
                CHECK (usage_status IN ('provider_exact', 'provider_partial', 'unavailable')),
            CONSTRAINT chk_inference_invocation_settlement_debts_delivery_state
                CHECK (provider_delivery_state IN ('unknown', 'pre_delivery', 'delivery_authorized')),
            CONSTRAINT chk_inference_invocation_settlement_debts_reconciliation_status
                CHECK (reconciliation_status IN ('pending', 'quarantined'))
        )",
    )
    .execute(&pool)
    .await?;
    for (column, ddl) in [
        (
            "session_id",
            "ALTER TABLE inference_invocation_settlement_debts ADD COLUMN session_id VARCHAR(64) NULL",
        ),
        (
            "harness_run_id",
            "ALTER TABLE inference_invocation_settlement_debts ADD COLUMN harness_run_id VARCHAR(128) NULL",
        ),
        (
            "provider_attempt_id",
            "ALTER TABLE inference_invocation_settlement_debts ADD COLUMN provider_attempt_id VARCHAR(64) NULL",
        ),
        (
            "usage_status",
            "ALTER TABLE inference_invocation_settlement_debts ADD COLUMN usage_status VARCHAR(32) NOT NULL DEFAULT 'unavailable'",
        ),
        (
            "provider_delivery_state",
            "ALTER TABLE inference_invocation_settlement_debts ADD COLUMN provider_delivery_state VARCHAR(32) NOT NULL DEFAULT 'unknown'",
        ),
        (
            "reconciliation_status",
            "ALTER TABLE inference_invocation_settlement_debts ADD COLUMN reconciliation_status VARCHAR(16) NOT NULL DEFAULT 'pending'",
        ),
        (
            "quarantine_reason",
            "ALTER TABLE inference_invocation_settlement_debts ADD COLUMN quarantine_reason VARCHAR(255) NULL",
        ),
        (
            "next_retry_at",
            "ALTER TABLE inference_invocation_settlement_debts ADD COLUMN next_retry_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)",
        ),
    ] {
        add_column_if_missing(
            &pool,
            &settings.database,
            "inference_invocation_settlement_debts",
            column,
            ddl,
        )
        .await?;
    }
    for (index, columns, ddl) in [
        (
            "idx_inference_settlement_owner_session_created",
            &["user_id", "session_id", "created_at", "invocation_id"][..],
            "ALTER TABLE inference_invocation_settlement_debts ADD INDEX idx_inference_settlement_owner_session_created (user_id, session_id, created_at, invocation_id)",
        ),
        (
            "idx_inference_settlement_owner_harness_created",
            &["user_id", "harness_run_id", "created_at", "invocation_id"][..],
            "ALTER TABLE inference_invocation_settlement_debts ADD INDEX idx_inference_settlement_owner_harness_created (user_id, harness_run_id, created_at, invocation_id)",
        ),
        (
            "idx_inference_settlement_recovery_ready",
            &[
                "reconciliation_status",
                "next_retry_at",
                "user_id",
                "invocation_id",
            ],
            "ALTER TABLE inference_invocation_settlement_debts ADD INDEX idx_inference_settlement_recovery_ready (reconciliation_status, next_retry_at, user_id, invocation_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "inference_invocation_settlement_debts",
            index,
            columns,
            ddl,
        )
        .await?;
    }

    // Lifecycle writes are exact owner+identity transitions. MatrixOne can
    // omit eligible rows when it plans those transitions through a secondary
    // index led by mutable status, so remove the old scan indexes from both
    // fresh and existing deployments before any reconciliation reads them.
    for (table, index) in [
        (
            "inference_invocations",
            "idx_inference_invocations_status_created",
        ),
        (
            "inference_provider_attempts",
            "idx_inference_attempts_status_started",
        ),
    ] {
        drop_index_if_present(&pool, &settings.database, table, index).await?;
    }
    fail_if_required_columns_missing_or_nullable(
        &pool,
        &settings.database,
        "inference_routes",
        &["scope_kind"],
    )
    .await?;
    fail_if_required_columns_missing_or_nullable(
        &pool,
        &settings.database,
        "inference_invocation_settlement_debts",
        &[
            "usage_status",
            "provider_delivery_state",
            "reconciliation_status",
            "next_retry_at",
        ],
    )
    .await?;
    fail_if_required_columns_missing_or_nullable(
        &pool,
        &settings.database,
        "inference_invocations",
        &["scope_kind", "operation_id"],
    )
    .await?;
    verify_inference_invocation_schema_contract(&pool, &settings.database).await?;
    verify_inference_provider_attempt_schema_contract(&pool, &settings.database).await?;
    verify_inference_canonical_transition_head_schema_contract(&pool, &settings.database).await?;
    verify_inference_canonical_transition_wal_schema_contract(&pool, &settings.database).await?;
    for (table, nullable_columns) in [
        (
            "inference_routes",
            &["session_id", "run_id", "harness_run_id"][..],
        ),
        (
            "inference_invocations",
            &[
                "session_id",
                "run_id",
                "harness_run_id",
                "turn_index",
                "round_index",
            ][..],
        ),
        (
            "inference_provider_attempts",
            &["session_id", "run_id", "harness_run_id"][..],
        ),
        (
            "inference_invocation_settlement_debts",
            &[
                "session_id",
                "harness_run_id",
                "provider_attempt_id",
                "quarantine_reason",
            ][..],
        ),
    ] {
        fail_if_required_columns_missing_or_not_nullable(
            &pool,
            &settings.database,
            table,
            nullable_columns,
        )
        .await?;
    }

    // Server-wide admin config KV store. Holds settings that the admin explicitly manages
    // via `astra admin config set/get/unset` (first key: `reasoning_offering_id`).
    core_schema_create!(
        pool,
        "admin_config",
        "CREATE TABLE IF NOT EXISTS admin_config (
            config_key VARCHAR(100) PRIMARY KEY,
            config_value TEXT NOT NULL,
            updated_by VARCHAR(128) NULL,
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "user_preferences",
        "CREATE TABLE IF NOT EXISTS user_preferences (
            pref_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            pref_key VARCHAR(100) NOT NULL,
            pref_value LONGTEXT NOT NULL,
            version INT NOT NULL DEFAULT 1,
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY idx_prefs_user_key (user_id, pref_key)
        )",
    )
    .execute(&pool)
    .await?;

    query("DROP TABLE IF EXISTS session_sync_log")
        .execute(&pool)
        .await?;

    // Skills registry — master catalog for database-backed skills.
    //
    // Important visibility contract:
    // - Local filesystem skills discovered by the CLI stay local to the CLI.
    // - Web/runtime skills must live in this table.
    // - Query paths expose only `created_by = current_user OR is_public = 1`.
    //
    // The paired visibility indexes below support that union without forcing
    // MatrixOne to scan every active skill when a user has many private skills
    // and the public catalog is also large.
    core_schema_create!(pool, "skills_registry",
        "CREATE TABLE IF NOT EXISTS skills_registry (
            skill_id VARCHAR(64) PRIMARY KEY,
            skill_name VARCHAR(255) NOT NULL,
            version VARCHAR(64) NOT NULL,
            description TEXT NULL,
            skill_definition JSON NULL,
            dependencies JSON NULL,
            manifest JSON NULL,
            publisher_id VARCHAR(255) NULL,
            publisher_verified SMALLINT NOT NULL DEFAULT 0,
            trust_tier VARCHAR(32) NULL,
            min_runtime_version VARCHAR(50) NULL,
            compatibility_platforms JSON NULL,
            category VARCHAR(64) NULL,
            priority INT NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            status VARCHAR(20) NOT NULL DEFAULT 'active',
            source VARCHAR(50) NOT NULL DEFAULT 'user',
            is_public SMALLINT NOT NULL DEFAULT 0,
            created_by VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_skill_name_version (skill_name, version),
            INDEX idx_skill_active_name (is_active, status, skill_name),
            INDEX idx_skill_active_created_at (is_active, created_at),
            INDEX idx_skill_visible_owner (is_active, created_by, created_at),
            INDEX idx_skill_visible_public (is_active, is_public, created_at),
            INDEX idx_skill_visible_name_owner (is_active, skill_name, created_by, is_public, created_at),
            INDEX idx_skill_source_name (source, skill_name),
            INDEX idx_skill_active_name_ver (is_active, skill_name, version)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "skills_registry",
        "idx_skill_active_name_ver",
        &["is_active", "skill_name", "version"],
        "ALTER TABLE skills_registry ADD INDEX idx_skill_active_name_ver (is_active, skill_name, version)",
    )
    .await?;
    core_schema_create!(
        pool,
        "skill_metrics",
        "CREATE TABLE IF NOT EXISTS skill_metrics (
            metric_id            VARCHAR(255) PRIMARY KEY,
            skill_name           VARCHAR(255) NOT NULL,
            metric_type          VARCHAR(32) NOT NULL,
            metric_slot          VARCHAR(255) NOT NULL,
            skill_version        VARCHAR(50) NULL,
            runtime_version      VARCHAR(50) NULL,
            publisher_id         VARCHAR(255) NULL,
            total_installs       BIGINT NOT NULL DEFAULT 0,
            active_users_7d      INT NOT NULL DEFAULT 0,
            avg_quality          FLOAT NOT NULL DEFAULT 0.0,
            avg_rating           FLOAT NOT NULL DEFAULT 0.0,
            report_count         INT NOT NULL DEFAULT 0,
            compatibility_score  FLOAT NOT NULL DEFAULT 0.0,
            trust_tier           VARCHAR(32) NULL,
            success_rate         FLOAT NULL,
            avg_tokens           FLOAT NULL,
            invocation_count     INT NULL,
            created_at           DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at           DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_skill_metrics_slot (skill_name, metric_type, metric_slot),
            INDEX idx_skill_metrics_lookup (skill_name, metric_type, created_at),
            INDEX idx_skill_metrics_ranking (metric_type, avg_quality, active_users_7d),
            INDEX idx_skill_metrics_tier (metric_type, trust_tier, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    core_schema_create!(
        pool,
        "edge_agent_registry",
        "CREATE TABLE IF NOT EXISTS edge_agent_registry (
            user_id VARCHAR(128) NOT NULL,
            registry_id VARCHAR(64) NOT NULL,
            edge_agent_id VARCHAR(255) NOT NULL,
            edge_id VARCHAR(128) NOT NULL,
            hostname VARCHAR(255) NULL,
            worktree_path VARCHAR(512) NULL,
            capabilities_json TEXT NULL,
            workspace_id VARCHAR(512) NULL,
            registration_claim_id VARCHAR(64) NULL,
            registration_claim_expires_at DATETIME(6) NULL,
            registration_state TINYINT NOT NULL DEFAULT 1,
            registration_previous_edge_id VARCHAR(128) NULL,
            registered_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            last_heartbeat_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, registry_id),
            UNIQUE KEY uq_edge_registry_user_agent (user_id, edge_agent_id),
            INDEX idx_edge_registry_user_heartbeat (user_id, last_heartbeat_at),
            INDEX idx_edge_registry_agent_workspace (edge_agent_id, workspace_id)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "edge_agent_registry",
        &["user_id", "registry_id"],
        "ALTER TABLE edge_agent_registry ADD PRIMARY KEY (user_id, registry_id)",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "edge_agent_registry",
        "workspace_id",
        "ALTER TABLE edge_agent_registry ADD COLUMN workspace_id VARCHAR(512) NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "edge_agent_registry",
        "registration_state",
        "ALTER TABLE edge_agent_registry ADD COLUMN registration_state TINYINT NOT NULL DEFAULT 1",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "edge_agent_registry",
        "registration_previous_edge_id",
        "ALTER TABLE edge_agent_registry ADD COLUMN registration_previous_edge_id VARCHAR(128) NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "edge_agent_registry",
        "registration_claim_id",
        "ALTER TABLE edge_agent_registry ADD COLUMN registration_claim_id VARCHAR(64) NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "edge_agent_registry",
        "registration_claim_expires_at",
        "ALTER TABLE edge_agent_registry ADD COLUMN registration_claim_expires_at DATETIME(6) NULL",
    )
    .await?;
    add_index_if_missing(
        &pool,
        &settings.database,
        "edge_agent_registry",
        "idx_edge_registry_agent_workspace",
        "ALTER TABLE edge_agent_registry ADD INDEX idx_edge_registry_agent_workspace (edge_agent_id, workspace_id)",
    )
    .await?;

    migrate_legacy_edge_pending_dispatch_if_needed(&pool, &settings.database).await?;
    core_schema_create!(pool, "edge_pending_dispatch",
        "CREATE TABLE IF NOT EXISTS edge_pending_dispatch (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            run_id VARCHAR(128) NOT NULL,
            turn_chain_id VARCHAR(128) NOT NULL,
            edge_agent_id VARCHAR(255) NOT NULL,
            request_id VARCHAR(128) NOT NULL,
            payload_json JSON NOT NULL,
            result_json JSON NULL,
            status VARCHAR(16) NOT NULL DEFAULT 'pending',
            pod_id VARCHAR(128) NULL,
            dispatched_at DATETIME(6) NULL,
            completed_at DATETIME(6) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, run_id, turn_chain_id, request_id),
            INDEX idx_edge_dispatch_user_status (user_id, edge_agent_id, status, created_at, session_id, run_id, turn_chain_id, request_id),
            INDEX idx_edge_dispatch_created (created_at)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "edge_pending_dispatch",
        EDGE_PENDING_DISPATCH_IDENTITY_COLUMNS,
        &["dispatch_id"],
        &["uq_edge_dispatch_owner_request"],
    )
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "edge_pending_dispatch",
        "uq_edge_dispatch_request_id",
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "edge_pending_dispatch",
        EDGE_PENDING_DISPATCH_IDENTITY_COLUMNS,
        "ALTER TABLE edge_pending_dispatch ADD PRIMARY KEY (user_id, session_id, run_id, turn_chain_id, request_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "edge_pending_dispatch",
        "idx_edge_dispatch_user_status",
        &[
            "user_id",
            "edge_agent_id",
            "status",
            "created_at",
            "session_id",
            "run_id",
            "turn_chain_id",
            "request_id",
        ],
        "ALTER TABLE edge_pending_dispatch ADD INDEX idx_edge_dispatch_user_status (user_id, edge_agent_id, status, created_at, session_id, run_id, turn_chain_id, request_id)",
    )
    .await?;

    core_schema_create!(
        pool,
        "agent_bindings",
        "CREATE TABLE IF NOT EXISTS agent_bindings (
            id VARCHAR(64) PRIMARY KEY,
            owner_user_id VARCHAR(128) NOT NULL,
            principal_scope_id VARCHAR(64) NOT NULL,
            binding_name VARCHAR(255) NOT NULL,
            idempotency_key VARCHAR(255) NOT NULL,
            status VARCHAR(32) NOT NULL DEFAULT 'active',
            agent_md LONGTEXT NOT NULL,
            metadata_json LONGTEXT NULL,
            binding_schema_version VARCHAR(32) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            disabled_at DATETIME(6) NULL,
            UNIQUE KEY uq_agent_bindings_owner_scope_name (owner_user_id, principal_scope_id, binding_name),
            UNIQUE KEY uq_agent_bindings_owner_scope_idempotency (owner_user_id, principal_scope_id, idempotency_key),
            INDEX idx_agent_bindings_owner_scope_status_created (owner_user_id, principal_scope_id, status, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_required_column_nullability_mismatches(
        &pool,
        &settings.database,
        "agent_bindings",
        &[
            ("owner_user_id", ColumnNullability::NotNull),
            ("principal_scope_id", ColumnNullability::NotNull),
        ],
    )
    .await?;
    let owner_column_rows = query(
        "SELECT COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH \
         FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'agent_bindings' \
           AND COLUMN_NAME IN ('owner_user_id', 'principal_scope_id')",
    )
    .bind(&settings.database)
    .fetch_all(&pool)
    .await?;
    let mut owner_column_shapes = BTreeMap::new();
    for row in owner_column_rows {
        owner_column_shapes.insert(
            row.try_get::<String, _>("COLUMN_NAME")?,
            (
                row.try_get::<String, _>("DATA_TYPE")?,
                row.try_get::<Option<i64>, _>("CHARACTER_MAXIMUM_LENGTH")?,
            ),
        );
    }
    for (column, expected_width) in [("owner_user_id", 128_i64), ("principal_scope_id", 64_i64)] {
        match owner_column_shapes.get(column) {
            Some((data_type, Some(width)))
                if data_type.eq_ignore_ascii_case("varchar") && *width == expected_width => {}
            actual => {
                return Err(sqlx::Error::Protocol(format!(
                    "obsolete agent_bindings.{column} shape {actual:?}; expected VARCHAR({expected_width}) NOT NULL"
                )));
            }
        }
    }
    let obsolete_index = query(
        "SELECT INDEX_NAME FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'agent_bindings' \
           AND INDEX_NAME IN ('uq_agent_bindings_name', \
                              'uq_agent_bindings_idempotency_key', \
                              'idx_agent_bindings_status_created') \
         LIMIT 1",
    )
    .bind(&settings.database)
    .fetch_optional(&pool)
    .await?;
    if let Some(row) = obsolete_index {
        let index_name: String = row.try_get("INDEX_NAME")?;
        return Err(sqlx::Error::Protocol(format!(
            "obsolete unscoped agent_bindings index {index_name} requires explicit schema replacement before startup"
        )));
    }
    ensure_index_shape(
        &pool,
        &settings.database,
        "agent_bindings",
        "uq_agent_bindings_owner_scope_name",
        &["owner_user_id", "principal_scope_id", "binding_name"],
        "ALTER TABLE agent_bindings ADD UNIQUE INDEX uq_agent_bindings_owner_scope_name (owner_user_id, principal_scope_id, binding_name)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "agent_bindings",
        "uq_agent_bindings_owner_scope_idempotency",
        &["owner_user_id", "principal_scope_id", "idempotency_key"],
        "ALTER TABLE agent_bindings ADD UNIQUE INDEX uq_agent_bindings_owner_scope_idempotency (owner_user_id, principal_scope_id, idempotency_key)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "agent_bindings",
        "idx_agent_bindings_owner_scope_status_created",
        &["owner_user_id", "principal_scope_id", "status", "created_at"],
        "ALTER TABLE agent_bindings ADD INDEX idx_agent_bindings_owner_scope_status_created (owner_user_id, principal_scope_id, status, created_at)",
    )
    .await?;

    core_schema_create!(
        pool,
        "mcp_servers",
        "CREATE TABLE IF NOT EXISTS mcp_servers (
            id VARCHAR(64) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            name VARCHAR(128) NOT NULL,
            description TEXT NULL,
            transport VARCHAR(32) NOT NULL,
            url TEXT NOT NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (owner_user_id, id),
            UNIQUE KEY uq_mcp_servers_owner_name (owner_user_id, name),
            INDEX idx_mcp_servers_owner_active (owner_user_id, is_active, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_varchar_columns_shorter_than(&pool, &settings.database, "mcp_servers", &[("id", 64)])
        .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "mcp_servers",
        &["owner_user_id", "id"],
        "ALTER TABLE mcp_servers ADD PRIMARY KEY (owner_user_id, id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "mcp_servers",
        "uq_mcp_servers_owner_name",
        &["owner_user_id", "name"],
        "ALTER TABLE mcp_servers ADD UNIQUE INDEX uq_mcp_servers_owner_name (owner_user_id, name)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "mcp_servers",
        "idx_mcp_servers_owner_active",
        &["owner_user_id", "is_active", "updated_at"],
        "ALTER TABLE mcp_servers ADD INDEX idx_mcp_servers_owner_active (owner_user_id, is_active, updated_at)",
    )
    .await?;

    core_schema_create!(
        pool,
        "mcp_bindings",
        "CREATE TABLE IF NOT EXISTS mcp_bindings (
            id VARCHAR(64) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            mcp_id VARCHAR(64) NOT NULL,
            key_hash VARCHAR(128) NOT NULL,
            key_value_encrypted TEXT NOT NULL,
            comment TEXT NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (owner_user_id, id),
            UNIQUE KEY uq_mcp_bindings_owner_mcp_key (owner_user_id, mcp_id, key_hash),
            INDEX idx_mcp_bindings_owner_active (owner_user_id, is_active, updated_at),
            INDEX idx_mcp_bindings_owner_mcp (owner_user_id, mcp_id)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "mcp_bindings",
        &[("id", 64), ("mcp_id", 64)],
    )
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "mcp_bindings",
        "idx_mcp_bindings_mcp_id",
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "mcp_bindings",
        &["owner_user_id", "id"],
        "ALTER TABLE mcp_bindings ADD PRIMARY KEY (owner_user_id, id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "mcp_bindings",
        "uq_mcp_bindings_owner_mcp_key",
        &["owner_user_id", "mcp_id", "key_hash"],
        "ALTER TABLE mcp_bindings ADD UNIQUE INDEX uq_mcp_bindings_owner_mcp_key (owner_user_id, mcp_id, key_hash)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "mcp_bindings",
        "idx_mcp_bindings_owner_active",
        &["owner_user_id", "is_active", "updated_at"],
        "ALTER TABLE mcp_bindings ADD INDEX idx_mcp_bindings_owner_active (owner_user_id, is_active, updated_at)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "mcp_bindings",
        "idx_mcp_bindings_owner_mcp",
        &["owner_user_id", "mcp_id"],
        "ALTER TABLE mcp_bindings ADD INDEX idx_mcp_bindings_owner_mcp (owner_user_id, mcp_id)",
    )
    .await?;
    core_schema_create!(
        pool,
        "mcp_tools",
        "CREATE TABLE IF NOT EXISTS mcp_tools (
            owner_user_id VARCHAR(128) NOT NULL,
            binding_id VARCHAR(64) NOT NULL,
            tool_name VARCHAR(256) NOT NULL,
            public_name VARCHAR(384) NOT NULL,
            description TEXT NULL,
            input_schema_json JSON NULL,
            output_schema_json JSON NULL,
            schema_hash VARCHAR(128) NOT NULL,
            discovered_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (owner_user_id, binding_id, tool_name),
            UNIQUE KEY uq_mcp_tools_owner_binding_public (owner_user_id, binding_id, public_name),
            INDEX idx_mcp_tools_owner_binding (owner_user_id, binding_id)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "mcp_tools",
        &["owner_user_id", "binding_id", "tool_name"],
        &["id"],
        &["uq_mcp_tools_binding_tool", "uq_mcp_tools_binding_public"],
    )
    .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "mcp_tools",
        &[("binding_id", 64)],
    )
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "mcp_tools",
        "idx_mcp_tools_binding",
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "mcp_tools",
        &["owner_user_id", "binding_id", "tool_name"],
        "ALTER TABLE mcp_tools ADD PRIMARY KEY (owner_user_id, binding_id, tool_name)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "mcp_tools",
        "uq_mcp_tools_owner_binding_public",
        &["owner_user_id", "binding_id", "public_name"],
        "ALTER TABLE mcp_tools ADD UNIQUE INDEX uq_mcp_tools_owner_binding_public (owner_user_id, binding_id, public_name)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "mcp_tools",
        "idx_mcp_tools_owner_binding",
        &["owner_user_id", "binding_id"],
        "ALTER TABLE mcp_tools ADD INDEX idx_mcp_tools_owner_binding (owner_user_id, binding_id)",
    )
    .await?;

    // ── Plans: cloud-authoritative plan state (user-owned, session-linked) ──
    // `subtask_count` is denormalized so list endpoints don't need to parse
    // `plan_json` just to render a card. Maintained by `PlanRepository::save`.
    core_schema_create!(
        pool,
        "plans",
        "CREATE TABLE IF NOT EXISTS plans (
            user_id       VARCHAR(128) NOT NULL,
            plan_id       VARCHAR(64) NOT NULL,
            session_id    VARCHAR(64) NULL,
            goal          TEXT NOT NULL,
            phase         VARCHAR(32) NOT NULL,
            version       BIGINT NOT NULL DEFAULT 0,
            plan_json     LONGTEXT NOT NULL,
            plan_md       LONGTEXT NULL,
            progress_pct  INT NOT NULL DEFAULT 0,
            subtask_count INT NOT NULL DEFAULT 0,
            created_by    VARCHAR(128) NULL,
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, plan_id),
            INDEX idx_plans_user_updated (user_id, updated_at DESC),
            INDEX idx_plans_owner_session_updated (user_id, session_id, updated_at DESC),
            INDEX idx_plans_user_phase (user_id, phase)
        )",
    )
    .execute(&pool)
    .await?;
    drop_index_if_present(&pool, &settings.database, "plans", "idx_plans_session").await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "plans",
        &["user_id", "plan_id"],
        "ALTER TABLE plans ADD PRIMARY KEY (user_id, plan_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "plans",
        "idx_plans_owner_session_updated",
        &["user_id", "session_id", "updated_at"],
        "ALTER TABLE plans ADD INDEX idx_plans_owner_session_updated (user_id, session_id, updated_at DESC)",
    )
    .await?;

    // ── Plan step runs: append-only attempt chain for every subtask ──
    core_schema_create!(
        pool,
        "plan_step_runs",
        "CREATE TABLE IF NOT EXISTS plan_step_runs (
            run_id       VARCHAR(64) NOT NULL,
            user_id      VARCHAR(128) NOT NULL,
            plan_id      VARCHAR(64) NOT NULL,
            subtask_id   VARCHAR(64) NOT NULL,
            attempt      INT NOT NULL,
            status       VARCHAR(16) NOT NULL,
            session_id   VARCHAR(64) NOT NULL,
            started_at   DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            finished_at  DATETIME(6) NULL,
            request_id   VARCHAR(64) NOT NULL,
            error        TEXT NULL,
            artifact_ref VARCHAR(255) NULL,
            PRIMARY KEY (user_id, run_id),
            INDEX idx_step_runs_plan_started (user_id, plan_id, started_at DESC),
            UNIQUE KEY uq_step_runs_subtask_attempt (user_id, plan_id, subtask_id, attempt)
        )",
    )
    .execute(&pool)
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "plan_step_runs",
        "idx_step_runs_session",
    )
    .await?;
    // Upgrade: old schema used single-column PK (run_id). Rebuild to composite.
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "plan_step_runs",
        &["user_id", "run_id"],
        "ALTER TABLE plan_step_runs ADD PRIMARY KEY (user_id, run_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "plan_step_runs",
        "idx_step_runs_plan_started",
        &["user_id", "plan_id", "started_at"],
        "ALTER TABLE plan_step_runs ADD INDEX idx_step_runs_plan_started (user_id, plan_id, started_at DESC)",
    )
    .await?;

    core_schema_create!(
        pool,
        "session_checkpoints",
        "CREATE TABLE IF NOT EXISTS session_checkpoints (
            checkpoint_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            number INT NOT NULL,
            turn INT NOT NULL,
            title VARCHAR(500) NULL,
            summary LONGTEXT NULL,
            tools_json JSON NULL,
            state_json LONGTEXT NULL,
            total_tokens BIGINT NOT NULL DEFAULT 0,
            had_stalls SMALLINT NOT NULL DEFAULT 0,
            error_count INT NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, checkpoint_id),
            UNIQUE KEY uq_session_checkpoints_owner_number (user_id, session_id, number),
            INDEX idx_ckpt_owner_session_turn (user_id, session_id, turn),
            INDEX idx_ckpt_user_created (user_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in ["idx_ckpt_session_number", "idx_ckpt_session_turn"] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "session_checkpoints",
            removed_index,
        )
        .await?;
    }
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_checkpoints",
        &["user_id", "session_id", "checkpoint_id"],
        "ALTER TABLE session_checkpoints ADD PRIMARY KEY (user_id, session_id, checkpoint_id)",
    )
    .await?;
    for (index, expected_columns, ddl) in [
        (
            "uq_session_checkpoints_owner_number",
            &["user_id", "session_id", "number"][..],
            "ALTER TABLE session_checkpoints ADD UNIQUE KEY uq_session_checkpoints_owner_number (user_id, session_id, number)",
        ),
        (
            "idx_ckpt_owner_session_turn",
            &["user_id", "session_id", "turn"][..],
            "ALTER TABLE session_checkpoints ADD INDEX idx_ckpt_owner_session_turn (user_id, session_id, turn)",
        ),
        (
            "idx_ckpt_user_created",
            &["user_id", "created_at"][..],
            "ALTER TABLE session_checkpoints ADD INDEX idx_ckpt_user_created (user_id, created_at)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_checkpoints",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    core_schema_create!(pool, "session_artifacts",
        "CREATE TABLE IF NOT EXISTS session_artifacts (
            artifact_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            project_id VARCHAR(128) NULL,
            owner_run_id VARCHAR(128) NULL,
            owner_delegation_id VARCHAR(128) NULL,
            root_run_id VARCHAR(128) NULL,
            artifact_kind VARCHAR(64) NOT NULL,
            source VARCHAR(64) NULL,
            turn INT NULL,
            round INT NULL,
            content_json LONGTEXT NOT NULL,
            metadata JSON NULL,
            access_scope VARCHAR(32) NOT NULL DEFAULT 'delegation',
            retention_policy VARCHAR(32) NOT NULL DEFAULT 'default',
            retention_until DATETIME(6) NULL,
            status VARCHAR(32) NOT NULL DEFAULT 'active',
            normalize_version VARCHAR(16) NULL,
            cold_storage_ref VARCHAR(255) NULL,
            derived_from_artifact_id VARCHAR(128) NULL,
            referenced_by_manifest_count INT NOT NULL DEFAULT 0,
            referenced_by_state_items_count INT NOT NULL DEFAULT 0,
            referenced_by_citation_count INT NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_session_artifacts_access_scope CHECK (access_scope IN ('private', 'delegation', 'delegation_direct', 'same_root_tree', 'user')),
            CONSTRAINT chk_session_artifacts_retention_policy CHECK (retention_policy IN ('default', 'permanent', 'project_long_term')),
            CONSTRAINT chk_session_artifacts_status CHECK (status IN ('active', 'expiring', 'expired')),
            PRIMARY KEY (user_id, session_id, artifact_id),
            INDEX idx_session_artifacts_owner_kind_order (user_id, session_id, artifact_kind, created_at, artifact_id),
            INDEX idx_session_artifacts_owner_session_order (user_id, session_id, created_at, artifact_id),
            INDEX idx_session_artifacts_owner_source_order (user_id, session_id, source, created_at, artifact_id),
            INDEX idx_session_artifacts_owner_created (user_id, created_at, session_id, artifact_id),
            INDEX idx_artifacts_root_scope (user_id, root_run_id, access_scope, status, updated_at, artifact_id),
            INDEX idx_artifacts_owner_run (user_id, owner_run_id, status, updated_at, artifact_id),
            INDEX idx_artifacts_retention (status, retention_until, retention_policy, user_id, session_id, artifact_id),
            INDEX idx_artifacts_project (user_id, project_id, status, retention_until, artifact_id),
            INDEX idx_artifacts_derived (user_id, session_id, derived_from_artifact_id)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_artifacts",
        &["user_id", "session_id", "artifact_id"],
        "ALTER TABLE session_artifacts ADD PRIMARY KEY (user_id, session_id, artifact_id)",
    )
    .await?;
    for (index, expected_columns, ddl) in [
        (
            "idx_session_artifacts_owner_kind_order",
            &[
                "user_id",
                "session_id",
                "artifact_kind",
                "created_at",
                "artifact_id",
            ][..],
            "ALTER TABLE session_artifacts ADD INDEX idx_session_artifacts_owner_kind_order (user_id, session_id, artifact_kind, created_at, artifact_id)",
        ),
        (
            "idx_session_artifacts_owner_session_order",
            &["user_id", "session_id", "created_at", "artifact_id"][..],
            "ALTER TABLE session_artifacts ADD INDEX idx_session_artifacts_owner_session_order (user_id, session_id, created_at, artifact_id)",
        ),
        (
            "idx_session_artifacts_owner_source_order",
            &[
                "user_id",
                "session_id",
                "source",
                "created_at",
                "artifact_id",
            ][..],
            "ALTER TABLE session_artifacts ADD INDEX idx_session_artifacts_owner_source_order (user_id, session_id, source, created_at, artifact_id)",
        ),
        (
            "idx_artifacts_root_scope",
            &[
                "user_id",
                "root_run_id",
                "access_scope",
                "status",
                "updated_at",
                "artifact_id",
            ][..],
            "ALTER TABLE session_artifacts ADD INDEX idx_artifacts_root_scope (user_id, root_run_id, access_scope, status, updated_at, artifact_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_artifacts",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_artifacts",
        "idx_artifacts_retention",
        &[
            "status",
            "retention_until",
            "retention_policy",
            "user_id",
            "session_id",
            "artifact_id",
        ],
        "ALTER TABLE session_artifacts ADD INDEX idx_artifacts_retention (status, retention_until, retention_policy, user_id, session_id, artifact_id)",
    )
    .await?;

    core_schema_create!(
        pool,
        "session_artifact_references",
        "CREATE TABLE IF NOT EXISTS session_artifact_references (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            artifact_id VARCHAR(64) NOT NULL,
            reference_kind VARCHAR(32) NOT NULL,
            reference_id VARCHAR(128) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_artifact_reference_kind CHECK (
                reference_kind IN ('invocation_ledger', 'manifest', 'state_item', 'citation')
            ),
            PRIMARY KEY (user_id, session_id, artifact_id, reference_kind, reference_id),
            INDEX idx_artifact_references_owner_reference
                (user_id, session_id, reference_kind, reference_id, artifact_id)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_artifact_references",
        &[
            "user_id",
            "session_id",
            "artifact_id",
            "reference_kind",
            "reference_id",
        ],
        "ALTER TABLE session_artifact_references ADD PRIMARY KEY (user_id, session_id, artifact_id, reference_kind, reference_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_artifact_references",
        "idx_artifact_references_owner_reference",
        &[
            "user_id",
            "session_id",
            "reference_kind",
            "reference_id",
            "artifact_id",
        ],
        "ALTER TABLE session_artifact_references ADD INDEX idx_artifact_references_owner_reference (user_id, session_id, reference_kind, reference_id, artifact_id)",
    )
    .await?;

    for (table, column, ddl) in [
        (
            "agent_sessions",
            "project_id",
            "ALTER TABLE agent_sessions ADD COLUMN project_id VARCHAR(128) NULL",
        ),
        (
            "agent_sessions",
            "project_retention_policy",
            "ALTER TABLE agent_sessions ADD COLUMN project_retention_policy VARCHAR(32) NOT NULL DEFAULT 'session'",
        ),
    ] {
        if let Err(e) = add_column_if_missing(&pool, &settings.database, table, column, ddl).await {
            tracing::warn!("core schema additive column skipped: {table}.{column}: {e}");
        }
    }

    for (table, index, ddl) in [
        (
            "agent_sessions",
            "idx_sessions_project",
            "ALTER TABLE agent_sessions ADD INDEX idx_sessions_project (user_id, project_id, updated_at)",
        ),
        (
            "agent_events",
            "idx_agent_events_owner_session_turn",
            AGENT_EVENTS_OWNER_SESSION_TURN_INDEX_ALTER_SQL,
        ),
    ] {
        if let Err(e) = add_index_if_missing(&pool, &settings.database, table, index, ddl).await {
            tracing::debug!("phase4 additive index migration skipped: {table}.{index}: {e}");
        }
    }

    // ─── Skill management tables ─────────────────────────────────────────────────

    core_schema_create!(pool, "user_skill_sources",
        "CREATE TABLE IF NOT EXISTS user_skill_sources (
            source_id VARCHAR(128) PRIMARY KEY,
            owner_user_id VARCHAR(128) NOT NULL,
            skill_name VARCHAR(128) NOT NULL,
            visibility VARCHAR(32) NOT NULL DEFAULT 'private',
            status VARCHAR(32) NOT NULL DEFAULT 'active',
            description TEXT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_user_skill_sources_visibility CHECK (visibility IN ('private', 'workspace', 'public')),
            CONSTRAINT chk_user_skill_sources_status CHECK (status IN ('active', 'archived')),
            UNIQUE KEY uq_user_skill_source_name (owner_user_id, skill_name),
            INDEX idx_user_skill_owner_name (owner_user_id, skill_name),
            INDEX idx_user_skill_status_updated (owner_user_id, status, updated_at)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(pool, "user_skill_versions",
        "CREATE TABLE IF NOT EXISTS user_skill_versions (
            version_id VARCHAR(128) PRIMARY KEY,
            source_id VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            skill_name VARCHAR(128) NOT NULL,
            version VARCHAR(64) NOT NULL,
            manifest_json LONGTEXT NOT NULL,
            content_markdown LONGTEXT NOT NULL,
            content_hash VARCHAR(128) NOT NULL,
            normalize_version VARCHAR(32) NOT NULL DEFAULT 'skill_md_v1',
            token_estimate INT NOT NULL DEFAULT 0,
            status VARCHAR(32) NOT NULL DEFAULT 'draft',
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_user_skill_versions_status CHECK (status IN ('draft', 'published', 'superseded', 'quarantined')),
            UNIQUE KEY uq_user_skill_source_version (source_id, version),
            INDEX idx_user_skill_versions_owner_name (owner_user_id, skill_name, status, created_at),
            INDEX idx_user_skill_versions_source (source_id, created_at),
            INDEX idx_user_skill_versions_hash (content_hash)
        )",
    )
    .execute(&pool)
    .await?;

    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "user_skill_evaluations",
        &[
            "evaluation_id",
            "owner_user_id",
            "source_id",
            "version_id",
            "run_id",
            "hits",
            "suspects",
            "false_positives",
            "payload_json",
            "created_at",
        ],
        &[],
        &[
            "idx_user_skill_eval_source_created",
            "idx_user_skill_eval_version_created",
            "idx_user_skill_eval_run",
        ],
    )
    .await?;

    core_schema_create!(
        pool,
        "user_skill_evaluations",
        "CREATE TABLE IF NOT EXISTS user_skill_evaluations (
            evaluation_id VARCHAR(128) PRIMARY KEY,
            owner_user_id VARCHAR(128) NOT NULL,
            source_id VARCHAR(128) NOT NULL,
            version_id VARCHAR(128) NOT NULL,
            run_id VARCHAR(128) NULL,
            hits BIGINT NOT NULL DEFAULT 0,
            suspects BIGINT NOT NULL DEFAULT 0,
            false_positives BIGINT NOT NULL DEFAULT 0,
            payload_json LONGTEXT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_user_skill_eval_owner_source_created (owner_user_id, source_id, created_at),
            INDEX idx_user_skill_eval_owner_version_created (owner_user_id, version_id, created_at),
            INDEX idx_user_skill_eval_owner_run (owner_user_id, run_id)
        )",
    )
    .execute(&pool)
    .await?;

    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "skill_installations",
        &[
            "installation_id",
            "user_id",
            "skill_name",
            "skill_version",
            "status",
            "installed_at",
            "updated_at",
        ],
        &[
            "scope",
            "session_id",
            "workspace_id",
            "auto_activate_on_topic_match",
        ],
        &["idx_si_scope_target", "idx_si_auto_activate"],
    )
    .await?;

    core_schema_create!(pool, "skill_installations",
        "CREATE TABLE IF NOT EXISTS skill_installations (
            installation_id  VARCHAR(36) PRIMARY KEY,
            user_id          VARCHAR(128) NOT NULL,
            skill_name       VARCHAR(128) NOT NULL,
            skill_version    VARCHAR(32) NOT NULL,
            status           VARCHAR(32) NOT NULL DEFAULT 'active',
            previous_version VARCHAR(32),
            installed_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at       DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            UNIQUE INDEX idx_si_user_skill (user_id, skill_name),
            INDEX idx_si_status (status)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(pool, "skill_settings",
        "CREATE TABLE IF NOT EXISTS skill_settings (
            setting_id    VARCHAR(36) PRIMARY KEY,
            skill_id      VARCHAR(36),
            skill_name    VARCHAR(128) NOT NULL,
            setting_name  VARCHAR(128) NOT NULL,
            setting_value TEXT,
            is_secret     SMALLINT NOT NULL DEFAULT 0,
            scope_type    VARCHAR(32) NOT NULL DEFAULT 'global',
            scope_id      VARCHAR(36),
            updated_by    VARCHAR(128),
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            UNIQUE INDEX idx_ss_skill_setting_scope (skill_name, setting_name, scope_type, scope_id),
            INDEX idx_ss_skill (skill_name)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(pool, "runtime_llm_trusted_domains",
        "CREATE TABLE IF NOT EXISTS runtime_llm_trusted_domains (
            domain_id     VARCHAR(36) PRIMARY KEY,
            domain_host   VARCHAR(255) NOT NULL,
            domain_port   INT NOT NULL DEFAULT 0,
            is_enabled    SMALLINT NOT NULL DEFAULT 1,
            description   VARCHAR(255),
            created_by    VARCHAR(128),
            updated_by    VARCHAR(128),
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            UNIQUE INDEX idx_rld_host_port (domain_host, domain_port),
            INDEX idx_rld_enabled (is_enabled)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(pool, "skill_resource_bindings",
        "CREATE TABLE IF NOT EXISTS skill_resource_bindings (
            binding_id    VARCHAR(36) PRIMARY KEY,
            user_id       VARCHAR(128) NOT NULL,
            skill_name    VARCHAR(128) NOT NULL,
            resource_type VARCHAR(64) NOT NULL,
            resource_key  VARCHAR(128) NOT NULL,
            binding_name  VARCHAR(128) NOT NULL,
            binding_value TEXT,
            is_secret     SMALLINT NOT NULL DEFAULT 0,
            updated_by    VARCHAR(128),
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_srb_user_skill (user_id, skill_name),
            INDEX idx_srb_resource (resource_type, resource_key)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(pool, "skill_user_credentials",
        "CREATE TABLE IF NOT EXISTS skill_user_credentials (
            credential_id   VARCHAR(36) PRIMARY KEY,
            user_id         VARCHAR(128) NOT NULL,
            skill_name      VARCHAR(128) NOT NULL,
            credential_name VARCHAR(128) NOT NULL,
            value_encrypted TEXT NOT NULL,
            created_at      DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at      DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            UNIQUE INDEX idx_suc_user_skill_cred (user_id, skill_name, credential_name)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(pool, "wf_triggers",
        "CREATE TABLE IF NOT EXISTS wf_triggers (
            trigger_id   VARCHAR(36) PRIMARY KEY,
            user_id      VARCHAR(128) NOT NULL,
            agent_id     VARCHAR(255) NOT NULL,
            trigger_type VARCHAR(32) NOT NULL,
            name         VARCHAR(128) NOT NULL,
            user_input   TEXT NOT NULL,
            context      LONGTEXT,
            cron_expr    VARCHAR(64),
            secret       VARCHAR(128),
            session_id   VARCHAR(36),
            is_active    SMALLINT NOT NULL DEFAULT 1,
            created_at   DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at   DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_wft_user (user_id),
            INDEX idx_wft_type (trigger_type),
            INDEX idx_wft_active (is_active)
        )",
    )
    .execute(&pool)
    .await?;

    // ─── Agent management tables ─────────────────────────────────────────────────

    core_schema_create!(pool, "agent_agents",
        "CREATE TABLE IF NOT EXISTS agent_agents (
            agent_id       VARCHAR(255) PRIMARY KEY,
            agent_name     VARCHAR(128) NOT NULL,
            agent_type     VARCHAR(64) NOT NULL DEFAULT 'general',
            owner_user_id  VARCHAR(128) NOT NULL,
            is_active      SMALLINT NOT NULL DEFAULT 1,
            agent_config   LONGTEXT,
            data_source    TEXT,
            created_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            UNIQUE INDEX idx_aa_owner_name (owner_user_id, agent_name),
            INDEX idx_aa_type (agent_type)
        )",
    )
    .execute(&pool)
    .await?;

    // ─── Infrastructure tables ───────────────────────────────────────────────────

    core_schema_create!(pool, "infra_sandbox_metadata",
        "CREATE TABLE IF NOT EXISTS infra_sandbox_metadata (
            sandbox_name VARCHAR(128) PRIMARY KEY,
            user_id      VARCHAR(128) NOT NULL,
            description  TEXT NOT NULL,
            created_by   VARCHAR(128) NOT NULL,
            status       VARCHAR(32) NOT NULL DEFAULT 'active',
            created_at   DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at   DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_ism_user (user_id),
            INDEX idx_ism_status (status)
        )",
    )
    .execute(&pool)
    .await?;

    // ─── Data versioning tables ──────────────────────────────────────────────────

    core_schema_create!(
        pool,
        "data_versioning_checkpoints",
        "CREATE TABLE IF NOT EXISTS data_versioning_checkpoints (
            checkpoint_id   VARCHAR(36) PRIMARY KEY,
            checkpoint_name VARCHAR(128) NOT NULL,
            user_id         VARCHAR(128) NOT NULL,
            description     TEXT,
            created_at      DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE INDEX idx_dvc_user_name (user_id, checkpoint_name)
        )",
    )
    .execute(&pool)
    .await?;

    // ─── Evaluation tables ───────────────────────────────────────────────────────

    core_schema_create!(
        pool,
        "eval_gate_results",
        "CREATE TABLE IF NOT EXISTS eval_gate_results (
            gate_id         VARCHAR(36) PRIMARY KEY,
            user_id         VARCHAR(128) NULL,
            change_type     VARCHAR(64) NOT NULL,
            change_id       VARCHAR(64) NOT NULL,
            sessions_tested INT NOT NULL DEFAULT 0,
            error_rate      DECIMAL(5,4),
            score_delta     DECIMAL(5,4),
            passed          SMALLINT NOT NULL DEFAULT 0,
            created_at      DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_egr_user_created (user_id, created_at),
            INDEX idx_egr_change (change_type, change_id),
            INDEX idx_egr_passed (passed)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(pool, "eval_quality_assessments",
        "CREATE TABLE IF NOT EXISTS eval_quality_assessments (
            assessment_id VARCHAR(64) PRIMARY KEY,
            user_id       VARCHAR(128) NULL,
            target_id     VARCHAR(64) NOT NULL,
            score         DECIMAL(5,4) NOT NULL,
            step_count    INT NOT NULL DEFAULT 0,
            level         VARCHAR(32) NOT NULL DEFAULT 'unknown',
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_eqa_user_level_updated (user_id, level, updated_at),
            INDEX idx_eqa_target (target_id),
            INDEX idx_eqa_level (level),
            INDEX idx_eqa_user_level_target (user_id, level, target_id)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "eval_calibration_assessments",
        EVAL_CALIBRATION_ASSESSMENTS_CREATE_SQL
    )
    .execute(&pool)
    .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "eval_calibration_assessments",
        &[("user_id", USER_ID_MAX_LEN as u64)],
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "eval_calibration_assessments",
        &["user_id", "calibration_id"],
        "ALTER TABLE eval_calibration_assessments ADD PRIMARY KEY (user_id, calibration_id)",
    )
    .await?;

    for (index, ddl) in [
        (
            "idx_eval_calibration_user_created",
            "ALTER TABLE eval_calibration_assessments ADD INDEX idx_eval_calibration_user_created (user_id, created_at)",
        ),
        (
            "idx_eval_calibration_user_agent_created",
            "ALTER TABLE eval_calibration_assessments ADD INDEX idx_eval_calibration_user_agent_created (user_id, agent_id, created_at)",
        ),
        (
            "idx_eval_calibration_session",
            "ALTER TABLE eval_calibration_assessments ADD INDEX idx_eval_calibration_session (user_id, session_id, created_at)",
        ),
    ] {
        if let Err(e) = add_index_if_missing(
            &pool,
            &settings.database,
            "eval_calibration_assessments",
            index,
            ddl,
        )
        .await
        {
            tracing::debug!("eval calibration additive index migration skipped: {index}: {e}");
        }
    }

    core_schema_create!(pool, "eval_training_datasets",
        "CREATE TABLE IF NOT EXISTS eval_training_datasets (
            dataset_id        VARCHAR(36) PRIMARY KEY,
            user_id           VARCHAR(128) NOT NULL,
            request_json      JSON NULL,
            dataset_json      LONGTEXT NOT NULL,
            sample_count      INT NOT NULL DEFAULT 0,
            quality_threshold DECIMAL(5,4) NOT NULL DEFAULT 0.7000,
            status            VARCHAR(32) NOT NULL DEFAULT 'ready',
            created_at        DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at        DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_eval_training_datasets_user_created (user_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    core_schema_create!(
        pool,
        "eval_user_feedback",
        "CREATE TABLE IF NOT EXISTS eval_user_feedback (
            feedback_id   VARCHAR(36) PRIMARY KEY,
            user_id       VARCHAR(128) NOT NULL,
            agent_id      VARCHAR(255),
            session_id    VARCHAR(36),
            turn_id       VARCHAR(36),
            feedback_type VARCHAR(64) NOT NULL,
            rating        INT,
            comment       TEXT,
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_euf_user (user_id),
            INDEX idx_euf_agent_created (agent_id, created_at),
            INDEX idx_euf_created (created_at),
            INDEX idx_euf_owner_session_created (user_id, session_id, created_at),
            INDEX idx_euf_type_created (feedback_type, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "eval_user_feedback",
        "idx_euf_session",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "eval_user_feedback",
        "idx_euf_owner_session_created",
        &["user_id", "session_id", "created_at"],
        "ALTER TABLE eval_user_feedback ADD INDEX idx_euf_owner_session_created (user_id, session_id, created_at)",
    )
    .await?;

    // ─── Team definitions ───────────────────────────────────────────────────────

    core_schema_create!(
        pool,
        "team_definitions",
        "CREATE TABLE IF NOT EXISTS team_definitions (
            team_id       VARCHAR(64)  PRIMARY KEY,
            user_id       VARCHAR(128)  NOT NULL,
            name          VARCHAR(128) NOT NULL,
            description   TEXT,
            coordination  TEXT         NOT NULL,
            members_json  TEXT         NOT NULL,
            context_json  TEXT,
            worktree_mode VARCHAR(32)  DEFAULT 'shared',
            budget_json   TEXT,
            max_parallel  INT UNSIGNED NOT NULL DEFAULT 0,
            created_at    DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at    DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_team_user_name (user_id, name)
        )",
    )
    .execute(&pool)
    .await?;

    // ─── Team execution history ─────────────────────────────────────────────────

    core_schema_create!(
        pool,
        "team_execution_history",
        "CREATE TABLE IF NOT EXISTS team_execution_history (
            execution_id  VARCHAR(64)  PRIMARY KEY,
            team_id       VARCHAR(64)  NOT NULL,
            user_id       VARCHAR(128)  NOT NULL,
            `task`        TEXT         NOT NULL,
            status        VARCHAR(32)  NOT NULL DEFAULT 'pending',
            result_json   LONGTEXT,
            started_at    DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            completed_at  DATETIME(6),
            INDEX idx_teh_team (team_id, started_at),
            INDEX idx_teh_user (user_id, started_at)
        )",
    )
    .execute(&pool)
    .await?;

    // ─── Team snapshots ─────────────────────────────────────────────────────────

    core_schema_create!(
        pool,
        "team_snapshots",
        "CREATE TABLE IF NOT EXISTS team_snapshots (
            snapshot_id          VARCHAR(64)  PRIMARY KEY,
            team_name            VARCHAR(128) NOT NULL,
            user_id              VARCHAR(128)  NOT NULL,
            label                VARCHAR(255) DEFAULT '',
            git_commit           VARCHAR(64),
            session_id           VARCHAR(64),
            team_definition_json LONGTEXT,
            created_at           DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_ts_user_team (user_id, team_name, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    // ─── Conversation State Log (CSL) ──────────────────────────────────────────

    core_schema_create!(
        pool,
        "conversation_log",
        "CREATE TABLE IF NOT EXISTS conversation_log (
            user_id       VARCHAR(128) NOT NULL,
            session_id    VARCHAR(64) NOT NULL,
            seq           BIGINT NOT NULL,
            turn          INT NOT NULL,
            entry_type    TINYINT NOT NULL,
            trace_id      VARCHAR(64) DEFAULT NULL,
            message_count INT DEFAULT NULL,
            payload       MEDIUMTEXT NOT NULL,
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, seq),
            INDEX idx_csl_owner_snapshot (user_id, session_id, entry_type, seq DESC),
            INDEX idx_csl_owner_turn (user_id, session_id, turn)
        )",
    )
    .execute(&pool)
    .await?;
    // ─── Content-addressed config versions (Step 4a) ────────────────────────────
    //
    // One row per unique RuntimeConfig hash per tenant. Populated by
    // the CLI via enqueue_journal_events → IngestionEvent::ConfigVersionSaved,
    // consumed by `astra config sync pull` on new machines. See
    // `crate::config_version_cloud` for the DDL string and bind helpers
    // that the push / pull pipeline uses.

    let config_version_schema = pool.owned_by("config_version_cloud");
    config_version_schema.authority.declare(
        config_version_schema.owner,
        "config_versions",
        crate::config_version_cloud::CONFIG_VERSIONS_CREATE_SQL,
    );
    query(crate::config_version_cloud::CONFIG_VERSIONS_CREATE_SQL)
        .execute(&config_version_schema)
        .await?;

    let declarations = pool.authority.declarations()?;
    publish_core_schema_table_contracts(&pool, &declarations).await?;
    verify_core_schema_catalog(&pool, &settings.database).await?;
    mark_core_schema_contract_current(&pool, holder_id).await?;
    Ok(())
}

trait DatabaseUserRow {
    fn string_column(&self, column: &'static str) -> Result<String, sqlx::Error>;
    fn optional_string_column(&self, column: &'static str) -> Result<Option<String>, sqlx::Error>;
    fn i16_column(&self, column: &'static str) -> Result<i16, sqlx::Error>;
}

impl DatabaseUserRow for sqlx::mysql::MySqlRow {
    fn string_column(&self, column: &'static str) -> Result<String, sqlx::Error> {
        self.try_get(column)
    }

    fn optional_string_column(&self, column: &'static str) -> Result<Option<String>, sqlx::Error> {
        self.try_get(column)
    }

    fn i16_column(&self, column: &'static str) -> Result<i16, sqlx::Error> {
        self.try_get(column)
    }
}

fn decode_database_user_row(row: &impl DatabaseUserRow) -> Result<DatabaseUserRecord, sqlx::Error> {
    Ok(DatabaseUserRecord {
        user_id: row.string_column("user_id")?,
        username: row.string_column("username")?,
        email: row.string_column("email")?,
        password_hash: row.string_column("password_hash")?,
        display_name: row.optional_string_column("display_name")?,
        is_active: row.i16_column("is_active")? != 0,
    })
}

pub fn database_user_from_row(
    row: sqlx::mysql::MySqlRow,
) -> Result<DatabaseUserRecord, sqlx::Error> {
    decode_database_user_row(&row)
}

trait ActiveSkillVersionRow {
    fn string_column(&self, column: &'static str) -> Result<String, sqlx::Error>;
}

impl ActiveSkillVersionRow for sqlx::mysql::MySqlRow {
    fn string_column(&self, column: &'static str) -> Result<String, sqlx::Error> {
        self.try_get(column)
    }
}

fn invalid_active_skill_version_value(column: &'static str, value: &str) -> sqlx::Error {
    sqlx::Error::Decode(Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("skills_registry decode column `{column}`: empty value `{value}`"),
    )))
}

fn decode_active_skill_version_row(
    row: &impl ActiveSkillVersionRow,
) -> Result<(String, String), sqlx::Error> {
    let skill_name = row.string_column("skill_name")?;
    if skill_name.trim().is_empty() {
        return Err(invalid_active_skill_version_value(
            "skill_name",
            &skill_name,
        ));
    }
    let version = row.string_column("version")?;
    if version.trim().is_empty() {
        return Err(invalid_active_skill_version_value("version", &version));
    }
    Ok((skill_name, version))
}

pub fn session_record_from_row(
    row: sqlx::mysql::MySqlRow,
) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
    let metadata_json: String = row.try_get("metadata_json").map_err(internal_error)?;
    let metadata =
        serde_json::from_str::<serde_json::Value>(&metadata_json).map_err(internal_error)?;
    let metadata = match metadata {
        serde_json::Value::Object(map) => map,
        serde_json::Value::Null => serde_json::Map::new(),
        _ => {
            return Err(internal_error(
                "session metadata must deserialize to a JSON object",
            ));
        }
    };

    Ok(SessionRecord {
        session_id: row.try_get("session_id").map_err(internal_error)?,
        user_id: row.try_get("user_id").map_err(internal_error)?,
        agent_id: row.try_get("agent_id").map_err(internal_error)?,
        title: row.try_get("title").map_err(internal_error)?,
        metadata,
        status: row.try_get("status").map_err(internal_error)?,
        event_count: row.try_get("event_count").map_err(internal_error)?,
        created_at: row.try_get("created_at").map_err(internal_error)?,
        updated_at: row.try_get("updated_at").map_err(internal_error)?,
        ended_at: row.try_get("ended_at").map_err(internal_error)?,
    })
}

pub async fn log_session_audit(
    pool: &sqlx::Pool<MySql>,
    user_id: &str,
    action: &str,
    session_id: &str,
    details: serde_json::Value,
) {
    if let Err(e) = query(
        "INSERT INTO auth_audit_logs \
         (log_id, user_id, action, resource_type, resource_id, details, created_at) \
         VALUES (?, ?, ?, 'session', ?, ?, NOW())",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(user_id)
    .bind(action)
    .bind(session_id)
    .bind(details.to_string())
    .execute(pool)
    .await
    {
        tracing::warn!(
            target: "astra_services::storage",
            user_id = %user_id,
            action = %action,
            session_id = %session_id,
            error = %e,
            "failed to write auth audit log"
        );
    }
}

pub async fn update_turn_skill_selection_version(
    tx: &mut sqlx::Transaction<'_, MySql>,
    event_id: &str,
    user_id: &str,
    session_id: &str,
    skill_version: &str,
) -> Result<(), sqlx::Error> {
    let result = query(
        "UPDATE skill_selection_events
         SET skill_version = ?
         WHERE event_id = ? AND user_id = ? AND session_id = ?",
    )
    .bind(skill_version)
    .bind(event_id)
    .bind(user_id)
    .bind(session_id)
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

pub async fn resolve_active_skill_versions(
    pool: &sqlx::Pool<MySql>,
    skill_names: BTreeSet<&str>,
) -> Result<HashMap<String, String>, sqlx::Error> {
    if skill_names.is_empty() {
        return Ok(HashMap::new());
    }

    let mut query_builder = QueryBuilder::<MySql>::new(
        "SELECT skill_name, version FROM skills_registry WHERE is_active = 1 AND skill_name IN (",
    );
    {
        let mut separated = query_builder.separated(", ");
        for skill_name in &skill_names {
            separated.push_bind(skill_name);
        }
    }
    query_builder.push(") ORDER BY skill_name ASC, version DESC");

    let rows = query_builder.build().fetch_all(pool).await?;
    let mut versions = HashMap::new();
    for row in rows {
        let (skill_name, version) = decode_active_skill_version_row(&row)?;
        if versions.contains_key(&skill_name) {
            continue;
        }
        versions.insert(skill_name, version);
    }
    Ok(versions)
}

// ─── Expired Data Cleanup ────────────────────────────────────────────────────

/// Result of a single table cleanup operation.
#[derive(Debug, Clone)]
pub struct CleanupResult {
    pub table: &'static str,
    pub rows_deleted: u64,
}

/// Configuration for expiry policies of authentication and operational data.
pub struct RetentionPolicy {
    /// Max age in days for expired/revoked refresh tokens (default: 7)
    pub refresh_token_days: u32,
    /// Max age in days for inactive auth tokens (default: 30)
    pub auth_token_days: u32,
    /// Max age in days for audit logs (default: 90)
    pub audit_log_days: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            refresh_token_days: 7,
            auth_token_days: 30,
            audit_log_days: 90,
        }
    }
}

/// Purge expired authentication and operational records with TTL/expiry semantics.
///
/// Returns a list of per-table cleanup results showing how many rows were deleted.
/// Each DELETE uses a LIMIT to avoid long-running locks; callers should invoke
/// repeatedly until all results show 0 rows deleted for a full sweep.
pub async fn cleanup_expired_data(
    pool: &sqlx::Pool<MySql>,
    policy: &RetentionPolicy,
) -> Result<Vec<CleanupResult>, String> {
    const AUTH_REFRESH_TOKEN_BATCH_LIMIT: u32 = 1000;
    const AUTH_PROOF_BATCH_LIMIT: u32 = 1000;
    const DEVICE_CHALLENGE_BATCH_LIMIT: u32 = 1000;
    const AUTH_TOKEN_BATCH_LIMIT: u32 = 1000;
    const AUTH_PROVIDER_REQUEST_REPLAY_BATCH_LIMIT: u32 = 1000;
    const AUTH_AUDIT_LOG_BATCH_LIMIT: u32 = 1000;
    let mut results = Vec::new();

    // 1. Expired + revoked refresh tokens
    let deleted = sqlx::query(
        "DELETE FROM auth_refresh_tokens \
         WHERE (expires_at < NOW(6) OR is_revoked = 1) \
           AND created_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         ORDER BY created_at ASC, token_id ASC \
         LIMIT ?",
    )
    .bind(policy.refresh_token_days)
    .bind(AUTH_REFRESH_TOKEN_BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .map_err(|e| format!("cleanup auth_refresh_tokens: {e}"))?;
    results.push(CleanupResult {
        table: "auth_refresh_tokens",
        rows_deleted: deleted,
    });

    let deleted = sqlx::query(
        "DELETE FROM auth_reauthentication_proofs
         WHERE expires_at < NOW(6) OR consumed_at IS NOT NULL
         ORDER BY created_at ASC, proof_id ASC
         LIMIT ?",
    )
    .bind(AUTH_PROOF_BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|result| result.rows_affected())
    .map_err(|error| format!("cleanup auth_reauthentication_proofs: {error}"))?;
    results.push(CleanupResult {
        table: "auth_reauthentication_proofs",
        rows_deleted: deleted,
    });

    let deleted = sqlx::query(
        "DELETE FROM session_device_challenges
         WHERE expires_at < NOW(6) OR consumed_at IS NOT NULL
         ORDER BY created_at ASC, challenge_id ASC
         LIMIT ?",
    )
    .bind(DEVICE_CHALLENGE_BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|result| result.rows_affected())
    .map_err(|error| format!("cleanup session_device_challenges: {error}"))?;
    results.push(CleanupResult {
        table: "session_device_challenges",
        rows_deleted: deleted,
    });

    // 2. Inactive auth tokens
    let deleted = sqlx::query(
        "DELETE FROM auth_tokens \
         WHERE is_active = 0 \
           AND created_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         ORDER BY created_at ASC, token_id ASC \
         LIMIT ?",
    )
    .bind(policy.auth_token_days)
    .bind(AUTH_TOKEN_BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .map_err(|e| format!("cleanup auth_tokens: {e}"))?;
    results.push(CleanupResult {
        table: "auth_tokens",
        rows_deleted: deleted,
    });

    // 3. Expired provider request replay guards. These are replay-prevention
    // facts, not audit facts: after the capability token expiry passes, the
    // row only consumes index space and can no longer authorize anything.
    let deleted = sqlx::query(
        "DELETE FROM auth_provider_request_replay \
         WHERE expires_at_unix < UNIX_TIMESTAMP() \
         ORDER BY expires_at_unix ASC, provider ASC, request_authorization_id ASC \
         LIMIT ?",
    )
    .bind(AUTH_PROVIDER_REQUEST_REPLAY_BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .map_err(|e| format!("cleanup auth_provider_request_replay: {e}"))?;
    results.push(CleanupResult {
        table: "auth_provider_request_replay",
        rows_deleted: deleted,
    });

    // 4. Old audit logs
    let deleted = sqlx::query(
        "DELETE FROM auth_audit_logs \
         WHERE created_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         ORDER BY created_at ASC, log_id ASC \
         LIMIT ?",
    )
    .bind(policy.audit_log_days)
    .bind(AUTH_AUDIT_LOG_BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .map_err(|e| format!("cleanup auth_audit_logs: {e}"))?;
    results.push(CleanupResult {
        table: "auth_audit_logs",
        rows_deleted: deleted,
    });

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_visibility_retry_is_limited_to_unknown_database() {
        assert!(is_fresh_database_visibility_error_code(1049));
        assert!(!is_fresh_database_visibility_error_code(1045));
        assert!(!is_fresh_database_visibility_error_code(1062));
        assert!(!is_fresh_database_visibility_error_code(2003));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires ASTRA_TEST_DB_IT=1 and a real MySQL/MatrixOne instance"]
    async fn bootstrap_pool_release_waits_for_checked_out_connection() {
        let _ = dotenvy::dotenv();
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for the real connection test"
        );
        let mut settings = MatrixOneSettings::from_env();
        settings.database = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
            .unwrap_or_else(|_| "mysql".to_string());
        settings.db_pool_max_connections = 1;
        settings.db_pool_min_connections = 0;
        let pool = BootstrapPool::new(
            connect_matrixone(&settings)
                .await
                .expect("connect a real one-slot bootstrap pool"),
            1,
        );
        let mut checked_out = pool
            .pool()
            .acquire()
            .await
            .expect("check out the real connection");
        let connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(&mut *checked_out)
            .await
            .expect("read the checked-out server connection id");
        let observer = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&settings.database_url_with_password())
            .await
            .expect("connect an independent server-side observer");

        let started = tokio::time::Instant::now();
        let release = tokio::spawn(pool.release());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !release.is_finished(),
            "teardown must retain the quota while a connection is checked out"
        );
        let live_before_return: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE conn_id = ?",
        )
        .bind(connection_id)
        .fetch_one(&observer)
        .await
        .expect("observe the checked-out server connection");
        assert_eq!(live_before_return, 1);

        drop(checked_out);
        release
            .await
            .expect("teardown task")
            .expect("release waits for return, then detaches the socket");
        let live_after_release: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE conn_id = ?",
        )
        .bind(connection_id)
        .fetch_one(&observer)
        .await
        .expect("observe server connection teardown");

        assert!(
            started.elapsed() >= std::time::Duration::from_millis(100),
            "quota must not be returned while a connection remains checked out"
        );
        assert_eq!(
            live_after_release, 0,
            "detached socket must be gone server-side"
        );
    }

    #[tokio::test]
    async fn dropping_schema_lease_cancels_heartbeat_task() {
        struct NotifyOnDrop(Option<tokio::sync::oneshot::Sender<()>>);

        impl Drop for NotifyOnDrop {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect_lazy("mysql://invalid:invalid@127.0.0.1:1/nonexistent")
            .expect("lazy test pool");
        let (stop_heartbeat, stop_rx) = tokio::sync::oneshot::channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let heartbeat = tokio::spawn(async move {
            let _notify_on_drop = NotifyOnDrop(Some(dropped_tx));
            let _ = started_tx.send(());
            let _ = stop_rx.await;
            std::future::pending::<()>().await;
        });
        started_rx.await.expect("heartbeat task started");
        let abort_handle = heartbeat.abort_handle();
        let lease = CoreSchemaDatabaseLease {
            pool,
            holder_id: "test-holder".to_string(),
            stop_heartbeat: Some(stop_heartbeat),
            heartbeat: Some(heartbeat),
        };

        drop(lease);

        tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
            .await
            .expect("heartbeat future must be dropped promptly")
            .expect("heartbeat drop notification");
        assert!(
            abort_handle.is_finished(),
            "dropping the lease must not leave its heartbeat task detached"
        );
    }

    #[test]
    fn identity_column_widening_preserves_nullability() {
        assert_eq!(
            identity_column_widening_ddl("eval_quality_assessments", "user_id", true)
                .expect("nullable identity migration DDL"),
            "ALTER TABLE `eval_quality_assessments` MODIFY COLUMN `user_id` VARCHAR(128) NULL"
        );
        assert_eq!(
            identity_column_widening_ddl("auth_users", "username", false)
                .expect("required identity migration DDL"),
            "ALTER TABLE `auth_users` MODIFY COLUMN `username` VARCHAR(128) NOT NULL"
        );
    }

    struct FakeDatabaseUserRow {
        failed_column: Option<&'static str>,
        display_name: Option<String>,
        is_active: i16,
    }

    impl FakeDatabaseUserRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                display_name: Some("Test User".to_string()),
                is_active: 1,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn fail_if_needed(&self, column: &'static str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl DatabaseUserRow for FakeDatabaseUserRow {
        fn string_column(&self, column: &'static str) -> Result<String, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "user_id" => "user-1",
                "username" => "test-user",
                "email" => "test@example.com",
                "password_hash" => "bcrypt-hash",
                _ => unreachable!("unexpected string column: {column}"),
            }
            .to_string())
        }

        fn optional_string_column(
            &self,
            column: &'static str,
        ) -> Result<Option<String>, sqlx::Error> {
            self.fail_if_needed(column)?;
            assert_eq!(column, "display_name");
            Ok(self.display_name.clone())
        }

        fn i16_column(&self, column: &'static str) -> Result<i16, sqlx::Error> {
            self.fail_if_needed(column)?;
            assert_eq!(column, "is_active");
            Ok(self.is_active)
        }
    }

    #[test]
    fn database_user_row_decode_preserves_database_values() {
        let user = decode_database_user_row(&FakeDatabaseUserRow::complete()).unwrap();

        assert_eq!(user.user_id, "user-1");
        assert_eq!(user.username, "test-user");
        assert_eq!(user.email, "test@example.com");
        assert_eq!(user.password_hash, "bcrypt-hash");
        assert_eq!(user.display_name.as_deref(), Some("Test User"));
        assert!(user.is_active);

        let inactive = decode_database_user_row(&FakeDatabaseUserRow {
            display_name: None,
            is_active: 0,
            ..FakeDatabaseUserRow::complete()
        })
        .unwrap();
        assert_eq!(inactive.display_name, None);
        assert!(!inactive.is_active);
    }

    #[test]
    fn database_user_row_decode_fails_loudly_on_any_missing_column() {
        for column in [
            "user_id",
            "username",
            "email",
            "password_hash",
            "display_name",
            "is_active",
        ] {
            match decode_database_user_row(&FakeDatabaseUserRow::fail_on(column)).unwrap_err() {
                sqlx::Error::ColumnNotFound(name) => assert_eq!(name, column),
                err => panic!("expected ColumnNotFound({column}), got {err:?}"),
            }
        }
    }

    struct FakeActiveSkillVersionRow {
        failed_column: Option<&'static str>,
        skill_name: &'static str,
        version: &'static str,
    }

    impl FakeActiveSkillVersionRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                skill_name: "skill-a",
                version: "v2",
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_values(skill_name: &'static str, version: &'static str) -> Self {
            Self {
                failed_column: None,
                skill_name,
                version,
            }
        }
    }

    impl ActiveSkillVersionRow for FakeActiveSkillVersionRow {
        fn string_column(&self, column: &'static str) -> Result<String, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            Ok(match column {
                "skill_name" => self.skill_name,
                "version" => self.version,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }
    }

    #[test]
    fn active_skill_version_row_decode_preserves_database_values() {
        let (skill_name, version) =
            decode_active_skill_version_row(&FakeActiveSkillVersionRow::complete()).unwrap();

        assert_eq!(skill_name, "skill-a");
        assert_eq!(version, "v2");
    }

    #[test]
    fn active_skill_version_row_decode_fails_loudly_on_missing_columns() {
        for column in ["skill_name", "version"] {
            match decode_active_skill_version_row(&FakeActiveSkillVersionRow::fail_on(column))
                .unwrap_err()
            {
                sqlx::Error::ColumnNotFound(name) => assert_eq!(name, column),
                err => panic!("expected ColumnNotFound({column}), got {err:?}"),
            }
        }
    }

    #[test]
    fn active_skill_version_row_decode_fails_loudly_on_empty_values() {
        for (column, row) in [
            (
                "skill_name",
                FakeActiveSkillVersionRow::with_values("   ", "v2"),
            ),
            (
                "version",
                FakeActiveSkillVersionRow::with_values("skill-a", ""),
            ),
        ] {
            let error = decode_active_skill_version_row(&row).unwrap_err();
            assert!(
                matches!(error, sqlx::Error::Decode(_)),
                "expected decode error for `{column}`, got {error:?}"
            );
            assert!(
                error.to_string().contains(column),
                "decode error should identify `{column}`: {error}"
            );
        }
    }

    /// Every `agent_id`, `edge_agent_id`, `holder_agent_id`, and
    /// `parent_agent_id` column in DDL MUST use the width encoded in
    /// [`AGENT_ID_LEN`].
    #[test]
    fn agent_id_columns_match_agreed_width() {
        // sanity: AGENT_ID_LEN must produce a reasonable VARCHAR width
        if AGENT_ID_LEN < 32 {
            panic!("AGENT_ID_LEN ({AGENT_ID_LEN}) is too small");
        }
    }

    #[test]
    fn invocation_ledger_bootstrap_requires_the_canonical_identity_key_shape() {
        assert_eq!(TOOL_INVOCATION_LEDGER_REQUIRED_COLUMNS, &["identity_key"]);
        assert_eq!(
            TOOL_INVOCATION_LEDGER_REQUIRED_VARCHAR_WIDTHS,
            &[("identity_key", 71)]
        );
    }

    #[test]
    fn agent_runs_canonical_columns_have_one_runtime_and_work_authority() {
        let columns = agent_runs_canonical_columns();
        for required in [
            "model_offering_id",
            "resolved_model_name",
            "start_request_fingerprint",
            "work_id",
            "work_branch_id",
            "work_graph_revision",
            "work_item_id",
            "work_item_revision",
            "work_item_attempt_id",
        ] {
            assert!(
                columns.contains(required),
                "missing canonical column {required}"
            );
        }
        for retired in [
            "selected_model_json",
            "selected_model_name",
            "selected_model_gateway",
            "provider_request_fingerprint",
        ] {
            assert!(
                !columns.contains(retired),
                "retired column {retired} must not enter the canonical contract"
            );
        }
    }

    fn canonical_provider_attempt_columns() -> BTreeMap<String, ObservedColumnShape> {
        [
            (
                "admission_token",
                ObservedColumnShape {
                    data_type: "char".to_string(),
                    character_maximum_length: Some(32),
                    nullable: false,
                },
            ),
            (
                "provider_protocol",
                ObservedColumnShape {
                    data_type: "varchar".to_string(),
                    character_maximum_length: Some(32),
                    nullable: false,
                },
            ),
            (
                "provider_wire_hash",
                ObservedColumnShape {
                    data_type: "char".to_string(),
                    character_maximum_length: Some(64),
                    nullable: false,
                },
            ),
            (
                "provider_wire_bytes",
                ObservedColumnShape {
                    data_type: "bigint".to_string(),
                    character_maximum_length: None,
                    nullable: false,
                },
            ),
            (
                "context_expired_at",
                ObservedColumnShape {
                    data_type: "datetime".to_string(),
                    character_maximum_length: None,
                    nullable: true,
                },
            ),
            (
                "canonical_transition_id",
                ObservedColumnShape {
                    data_type: "char".to_string(),
                    character_maximum_length: Some(64),
                    nullable: true,
                },
            ),
            (
                "canonical_parent_transition_id",
                ObservedColumnShape {
                    data_type: "char".to_string(),
                    character_maximum_length: Some(64),
                    nullable: true,
                },
            ),
            (
                "canonical_transition_hash",
                ObservedColumnShape {
                    data_type: "char".to_string(),
                    character_maximum_length: Some(64),
                    nullable: true,
                },
            ),
            (
                "usage_status",
                ObservedColumnShape {
                    data_type: "varchar".to_string(),
                    character_maximum_length: Some(32),
                    nullable: false,
                },
            ),
        ]
        .into_iter()
        .map(|(name, shape)| (name.to_string(), shape))
        .collect()
    }

    fn canonical_provider_attempt_indexes() -> BTreeMap<String, ObservedIndexShape> {
        [
            (
                "PRIMARY",
                ObservedIndexShape {
                    columns: vec!["user_id".to_string(), "attempt_id".to_string()],
                    non_unique: false,
                },
            ),
            (
                "uq_inference_provider_attempt",
                ObservedIndexShape {
                    columns: vec![
                        "user_id".to_string(),
                        "invocation_id".to_string(),
                        "attempt_index".to_string(),
                    ],
                    non_unique: false,
                },
            ),
        ]
        .into_iter()
        .map(|(name, shape)| (name.to_string(), shape))
        .collect()
    }

    fn canonical_transition_head_columns() -> BTreeMap<String, ObservedColumnShape> {
        [
            ("user_id", "varchar", Some(128), false),
            ("session_id", "varchar", Some(64), false),
            ("turn_index", "bigint", None, false),
            ("head_transition_id", "char", Some(64), false),
            ("head_attempt_id", "varchar", Some(64), false),
            ("head_result_count", "bigint", None, false),
            ("head_result_root_hash", "char", Some(64), false),
            ("chain_length", "bigint", None, false),
            ("chain_payload_bytes", "bigint", None, false),
            ("updated_at", "datetime", None, false),
        ]
        .into_iter()
        .map(|(name, data_type, character_maximum_length, nullable)| {
            (
                name.to_string(),
                ObservedColumnShape {
                    data_type: data_type.to_string(),
                    character_maximum_length,
                    nullable,
                },
            )
        })
        .collect()
    }

    fn canonical_transition_head_indexes() -> BTreeMap<String, ObservedIndexShape> {
        [
            ("PRIMARY", vec!["user_id", "session_id", "turn_index"]),
            (
                "uq_inference_canonical_head_attempt",
                vec!["user_id", "head_attempt_id"],
            ),
        ]
        .into_iter()
        .map(|(name, columns)| {
            (
                name.to_string(),
                ObservedIndexShape {
                    columns: columns.into_iter().map(str::to_string).collect(),
                    non_unique: false,
                },
            )
        })
        .collect()
    }

    fn canonical_transition_wal_columns() -> BTreeMap<String, ObservedColumnShape> {
        [
            ("user_id", "varchar", Some(128), false),
            ("session_id", "varchar", Some(64), false),
            ("turn_index", "bigint", None, false),
            ("round_index", "bigint", None, false),
            ("logical_attempt", "bigint", None, false),
            ("physical_attempt", "bigint", None, false),
            ("transition_id", "char", Some(64), false),
            ("parent_transition_id", "char", Some(64), true),
            ("attempt_id", "varchar", Some(64), false),
            ("payload_json", "text", None, false),
            ("payload_hash", "char", Some(64), false),
            ("payload_bytes", "bigint", None, false),
            ("predecessor_count", "bigint", None, false),
            ("predecessor_root_hash", "char", Some(64), false),
            ("result_count", "bigint", None, false),
            ("result_root_hash", "char", Some(64), false),
            ("recovery_mode", "varchar", Some(32), false),
            ("created_at", "datetime", None, false),
        ]
        .into_iter()
        .map(|(name, data_type, character_maximum_length, nullable)| {
            (
                name.to_string(),
                ObservedColumnShape {
                    data_type: data_type.to_string(),
                    character_maximum_length,
                    nullable,
                },
            )
        })
        .collect()
    }

    fn canonical_transition_wal_indexes() -> BTreeMap<String, ObservedIndexShape> {
        [
            (
                "PRIMARY",
                vec!["user_id", "session_id", "turn_index", "transition_id"],
                false,
            ),
            (
                "uq_inference_canonical_wal_attempt",
                vec!["user_id", "attempt_id"],
                false,
            ),
            (
                "idx_inference_canonical_wal_parent",
                vec![
                    "user_id",
                    "session_id",
                    "turn_index",
                    "parent_transition_id",
                ],
                true,
            ),
        ]
        .into_iter()
        .map(|(name, columns, non_unique)| {
            (
                name.to_string(),
                ObservedIndexShape {
                    columns: columns.into_iter().map(str::to_string).collect(),
                    non_unique,
                },
            )
        })
        .collect()
    }

    #[test]
    fn provider_attempt_schema_contract_accepts_the_exact_wire_shape() {
        assert!(
            inference_provider_attempt_schema_mismatches(
                &canonical_provider_attempt_columns(),
                &canonical_provider_attempt_indexes(),
            )
            .is_empty()
        );
    }

    #[test]
    fn canonical_transition_head_schema_contract_is_exact_and_fail_closed() {
        let exact_columns = canonical_transition_head_columns();
        let exact_indexes = canonical_transition_head_indexes();
        assert!(
            inference_canonical_transition_head_schema_mismatches(&exact_columns, &exact_indexes,)
                .is_empty()
        );

        let mut missing_id = exact_columns.clone();
        missing_id.remove("head_transition_id");
        assert!(
            inference_canonical_transition_head_schema_mismatches(&missing_id, &exact_indexes)
                .iter()
                .any(|reason| reason.contains("missing NOT NULL column head_transition_id"))
        );

        let mut wrong_width = exact_columns.clone();
        wrong_width
            .get_mut("head_transition_id")
            .unwrap()
            .character_maximum_length = Some(32);
        assert!(
            inference_canonical_transition_head_schema_mismatches(&wrong_width, &exact_indexes)
                .iter()
                .any(|reason| reason.contains("head_transition_id has width Some(32)"))
        );

        let mut wrong_primary = exact_indexes;
        wrong_primary.get_mut("PRIMARY").unwrap().columns.swap(1, 2);
        assert!(
            inference_canonical_transition_head_schema_mismatches(&exact_columns, &wrong_primary)
                .iter()
                .any(|reason| reason.contains("constraint PRIMARY has columns"))
        );
    }

    #[test]
    fn canonical_transition_wal_schema_contract_is_exact_and_fail_closed() {
        let exact_columns = canonical_transition_wal_columns();
        let exact_indexes = canonical_transition_wal_indexes();
        assert!(
            inference_canonical_transition_wal_schema_mismatches(&exact_columns, &exact_indexes,)
                .is_empty()
        );

        let mut nullable_owner = exact_columns.clone();
        nullable_owner.get_mut("user_id").unwrap().nullable = true;
        assert!(
            inference_canonical_transition_wal_schema_mismatches(&nullable_owner, &exact_indexes,)
                .iter()
                .any(|reason| reason.contains("user_id has nullable=true"))
        );

        let mut wrong_parent_index = exact_indexes.clone();
        wrong_parent_index
            .get_mut("idx_inference_canonical_wal_parent")
            .unwrap()
            .columns
            .remove(0);
        assert!(
            inference_canonical_transition_wal_schema_mismatches(
                &exact_columns,
                &wrong_parent_index,
            )
            .iter()
            .any(|reason| reason.contains("idx_inference_canonical_wal_parent has columns"))
        );

        let mut non_unique_attempt = exact_indexes;
        non_unique_attempt
            .get_mut("uq_inference_canonical_wal_attempt")
            .unwrap()
            .non_unique = true;
        assert!(
            inference_canonical_transition_wal_schema_mismatches(
                &exact_columns,
                &non_unique_attempt,
            )
            .iter()
            .any(|reason| reason.contains("uq_inference_canonical_wal_attempt has non_unique=true"))
        );
    }

    #[test]
    fn invocation_schema_contract_requires_an_exact_admission_fence() {
        let exact = [
            (
                "admission_token".to_string(),
                ObservedColumnShape {
                    data_type: "char".to_string(),
                    character_maximum_length: Some(32),
                    nullable: false,
                },
            ),
            (
                "owner_token".to_string(),
                ObservedColumnShape {
                    data_type: "char".to_string(),
                    character_maximum_length: Some(32),
                    nullable: false,
                },
            ),
            (
                "owner_generation".to_string(),
                ObservedColumnShape {
                    data_type: "bigint".to_string(),
                    character_maximum_length: None,
                    nullable: false,
                },
            ),
            (
                "owner_lease_expires_at".to_string(),
                ObservedColumnShape {
                    data_type: "datetime".to_string(),
                    character_maximum_length: None,
                    nullable: false,
                },
            ),
            (
                "usage_status".to_string(),
                ObservedColumnShape {
                    data_type: "varchar".to_string(),
                    character_maximum_length: Some(32),
                    nullable: false,
                },
            ),
            (
                "provider_delivery_state".to_string(),
                ObservedColumnShape {
                    data_type: "varchar".to_string(),
                    character_maximum_length: Some(32),
                    nullable: false,
                },
            ),
        ]
        .into_iter()
        .collect();
        assert!(inference_invocation_schema_mismatches(&exact).is_empty());

        let mut nullable = exact.clone();
        nullable.get_mut("admission_token").unwrap().nullable = true;
        assert!(!inference_invocation_schema_mismatches(&nullable).is_empty());

        let mut wrong_width = exact;
        wrong_width
            .get_mut("admission_token")
            .unwrap()
            .character_maximum_length = Some(36);
        assert!(!inference_invocation_schema_mismatches(&wrong_width).is_empty());
    }

    #[test]
    fn provider_attempt_schema_contract_rejects_independent_column_and_key_drift() {
        let exact_columns = canonical_provider_attempt_columns();
        let exact_indexes = canonical_provider_attempt_indexes();
        let mut cases = Vec::new();

        let mut columns = exact_columns.clone();
        columns.remove("admission_token");
        cases.push((
            "missing NOT NULL column admission_token",
            columns,
            exact_indexes.clone(),
        ));

        let mut columns = exact_columns.clone();
        columns.remove("provider_protocol");
        cases.push((
            "missing NOT NULL column provider_protocol",
            columns,
            exact_indexes.clone(),
        ));

        let mut columns = exact_columns.clone();
        columns.get_mut("provider_protocol").unwrap().nullable = true;
        cases.push((
            "nullable column provider_protocol",
            columns,
            exact_indexes.clone(),
        ));

        let mut columns = exact_columns.clone();
        columns.get_mut("provider_wire_hash").unwrap().data_type = "varchar".to_string();
        cases.push((
            "provider_wire_hash has type varchar",
            columns,
            exact_indexes.clone(),
        ));

        let mut columns = exact_columns.clone();
        columns
            .get_mut("provider_wire_hash")
            .unwrap()
            .character_maximum_length = Some(32);
        cases.push((
            "provider_wire_hash has width Some(32)",
            columns,
            exact_indexes.clone(),
        ));

        let mut columns = exact_columns.clone();
        columns.get_mut("provider_wire_bytes").unwrap().data_type = "int".to_string();
        cases.push((
            "provider_wire_bytes has type int",
            columns,
            exact_indexes.clone(),
        ));

        let mut columns = exact_columns.clone();
        columns.remove("canonical_transition_id");
        cases.push((
            "missing nullable column canonical_transition_id",
            columns,
            exact_indexes.clone(),
        ));

        let mut columns = exact_columns.clone();
        columns
            .get_mut("canonical_parent_transition_id")
            .unwrap()
            .nullable = false;
        cases.push((
            "non-nullable column canonical_parent_transition_id",
            columns,
            exact_indexes.clone(),
        ));

        let mut columns = exact_columns.clone();
        columns
            .get_mut("canonical_transition_id")
            .unwrap()
            .character_maximum_length = Some(32);
        cases.push((
            "canonical_transition_id has width Some(32)",
            columns,
            exact_indexes.clone(),
        ));

        let mut columns = exact_columns.clone();
        columns
            .get_mut("canonical_transition_hash")
            .unwrap()
            .nullable = false;
        cases.push((
            "non-nullable column canonical_transition_hash",
            columns,
            exact_indexes.clone(),
        ));

        let mut columns = exact_columns.clone();
        columns
            .get_mut("canonical_transition_hash")
            .unwrap()
            .character_maximum_length = Some(32);
        cases.push((
            "canonical_transition_hash has width Some(32)",
            columns,
            exact_indexes.clone(),
        ));

        let mut indexes = exact_indexes.clone();
        indexes.remove("PRIMARY");
        cases.push((
            "missing unique constraint PRIMARY",
            exact_columns.clone(),
            indexes,
        ));

        let mut indexes = exact_indexes.clone();
        indexes
            .get_mut("uq_inference_provider_attempt")
            .unwrap()
            .non_unique = true;
        cases.push((
            "constraint uq_inference_provider_attempt is not unique",
            exact_columns.clone(),
            indexes,
        ));

        let mut indexes = exact_indexes;
        indexes
            .get_mut("uq_inference_provider_attempt")
            .unwrap()
            .columns
            .swap(1, 2);
        cases.push((
            "constraint uq_inference_provider_attempt has columns",
            exact_columns,
            indexes,
        ));

        for (expected, columns, indexes) in cases {
            let reasons = inference_provider_attempt_schema_mismatches(&columns, &indexes);
            assert!(
                reasons.iter().any(|reason| reason.contains(expected)),
                "expected `{expected}` in {reasons:?}"
            );
        }
    }

    #[test]
    fn core_schema_authority_uses_typed_declarations() {
        let authority = CoreSchemaAuthority::default();
        authority.declare(
            "storage",
            "canonical_table",
            "CREATE TABLE IF NOT EXISTS canonical_table (id BIGINT PRIMARY KEY)",
        );

        let declarations = authority.declarations().unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "canonical_table");
        assert_eq!(declarations[0].owner, "storage");
        assert_eq!(declarations[0].ddl_sha256.len(), 64);
    }

    #[test]
    fn core_schema_authority_rejects_multiple_lifecycle_producers() {
        let authority = CoreSchemaAuthority::default();
        authority.declare(
            "storage",
            "duplicate_owner",
            "CREATE TABLE duplicate_owner (id BIGINT)",
        );
        authority.declare(
            "another_module",
            "duplicate_owner",
            "CREATE TABLE IF NOT EXISTS duplicate_owner (id BIGINT)",
        );

        let error = authority.declarations().unwrap_err().to_string();
        assert!(error.contains("duplicate_owner (claims=2)"), "{error}");
    }

    fn persisted_claim(
        name: &str,
        component: &str,
        owner: &str,
        version: &str,
    ) -> PersistedCoreSchemaTableClaim {
        PersistedCoreSchemaTableClaim {
            name: name.to_string(),
            component: component.to_string(),
            owner: owner.to_string(),
            contract_version: version.to_string(),
            ddl_sha256: "a".repeat(64),
        }
    }

    fn desired_table(name: &str, owner: &str) -> CoreSchemaTableSpec {
        CoreSchemaTableSpec {
            name: name.to_string(),
            owner: owner.to_string(),
            ddl_sha256: "b".repeat(64),
        }
    }

    #[test]
    fn core_schema_contract_upgrade_reconciles_same_component_in_place() {
        let existing = vec![persisted_claim(
            "admin_config",
            CORE_SCHEMA_CONTRACT_COMPONENT,
            "storage",
            "2026-07-20-v6",
        )];
        let declarations = vec![desired_table("admin_config", "storage")];

        let stale = stale_core_schema_table_claims(&existing, &declarations).unwrap();

        assert!(
            stale.is_empty(),
            "an older contract for the same component must be updated in place"
        );
    }

    #[test]
    fn core_schema_contract_upgrade_removes_only_retired_same_component_claims() {
        let existing = vec![
            persisted_claim(
                "kept_table",
                CORE_SCHEMA_CONTRACT_COMPONENT,
                "storage",
                "old",
            ),
            persisted_claim(
                "retired_table",
                CORE_SCHEMA_CONTRACT_COMPONENT,
                "storage",
                "old",
            ),
            persisted_claim("foreign_table", "extension", "plugin", "v1"),
        ];
        let declarations = vec![desired_table("kept_table", "storage")];

        let stale = stale_core_schema_table_claims(&existing, &declarations).unwrap();

        assert_eq!(stale, vec!["retired_table"]);
    }

    #[test]
    fn core_schema_contract_upgrade_rejects_foreign_component_claim() {
        let existing = vec![persisted_claim("shared_table", "extension", "plugin", "v1")];
        let declarations = vec![desired_table("shared_table", "storage")];

        let error = stale_core_schema_table_claims(&existing, &declarations)
            .unwrap_err()
            .to_string();

        assert!(error.contains("shared_table"), "{error}");
        assert!(error.contains("extension"), "{error}");
        assert!(error.contains("plugin"), "{error}");
    }

    #[test]
    fn core_schema_contract_write_never_takes_over_a_concurrent_foreign_claim() {
        let declaration = desired_table("shared_table", "storage");
        let raced_claim = persisted_claim("shared_table", "extension", "plugin", "v1");

        let error = validate_reconciled_core_schema_table_claim(&raced_claim, &declaration)
            .expect_err("a foreign claim that appears after the pre-read must still fail")
            .to_string();

        assert!(error.contains("shared_table"), "{error}");
        assert!(error.contains("extension"), "{error}");
        assert!(error.contains("plugin"), "{error}");
        assert!(
            INSERT_CORE_SCHEMA_TABLE_CLAIM_WITHOUT_TAKEOVER_SQL.starts_with("INSERT IGNORE INTO"),
            "claims must use an identity-preserving insert-if-absent operation"
        );
        assert!(
            !INSERT_CORE_SCHEMA_TABLE_CLAIM_WITHOUT_TAKEOVER_SQL
                .contains("ON DUPLICATE KEY UPDATE"),
            "a duplicate claim must never be implemented by updating its identity"
        );
        assert!(
            UPDATE_OWNED_CORE_SCHEMA_TABLE_CLAIM_SQL.contains("AND component = ?"),
            "contract updates must be scoped to the existing owning component"
        );
    }

    #[test]
    fn core_schema_contract_write_validates_the_exact_converged_claim() {
        let declaration = desired_table("shared_table", "storage");
        let converged = PersistedCoreSchemaTableClaim {
            name: declaration.name.clone(),
            component: CORE_SCHEMA_CONTRACT_COMPONENT.to_string(),
            owner: declaration.owner.clone(),
            contract_version: CORE_SCHEMA_CONTRACT_VERSION.to_string(),
            ddl_sha256: declaration.ddl_sha256.clone(),
        };

        validate_reconciled_core_schema_table_claim(&converged, &declaration)
            .expect("the exact owner-scoped contract should validate");

        for divergent in [
            PersistedCoreSchemaTableClaim {
                owner: "other-owner".to_string(),
                ..converged.clone()
            },
            PersistedCoreSchemaTableClaim {
                contract_version: "stale-version".to_string(),
                ..converged.clone()
            },
            PersistedCoreSchemaTableClaim {
                ddl_sha256: "c".repeat(64),
                ..converged
            },
        ] {
            assert!(
                validate_reconciled_core_schema_table_claim(&divergent, &declaration).is_err(),
                "reconciliation must fail loudly unless every claimed contract field converged"
            );
        }
    }

    #[test]
    fn rows_affected_to_i64_fails_loudly_on_overflow() {
        assert_eq!(
            rows_affected_to_i64(i64::MAX as u64, "event_count_delta").unwrap(),
            i64::MAX
        );
        let err = rows_affected_to_i64(i64::MAX as u64 + 1, "event_count_delta")
            .expect_err("overflow must fail");
        let err = err.to_string();
        assert!(
            err.contains("event_count_delta") && err.contains("exceeds i64::MAX"),
            "error should identify conversion context and overflow: {err}"
        );
    }
}
