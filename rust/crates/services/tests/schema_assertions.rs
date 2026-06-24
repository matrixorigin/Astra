mod common;

use sqlx::Row;
use std::path::{Path, PathBuf};

#[test]
fn service_table_queries_name_columns_explicitly() {
    let mut files = Vec::new();
    collect_rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );

    let mut offenders = Vec::new();
    for path in files {
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (line_idx, line) in source.lines().enumerate() {
            let normalized = line.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.contains("SELECT *") && !normalized.contains("mo_diff(") {
                offenders.push(format!("{}:{}", path.display(), line_idx + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "production table reads must name selected columns; offenders:\n{}",
        offenders.join("\n")
    );
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn schema_rationalization_runtime_contract() {
    let pool = common::setup_pool().await;
    let schema = current_schema(&pool).await;

    for removed in [
        "prompt_chunks",
        "harness_sources",
        "harness_decisions",
        "wf_definitions",
        "wf_runs",
        "skill_marketplace_stats",
        "skill_quality_reports",
    ] {
        assert!(
            !table_exists(&pool, &schema, removed).await,
            "removed table must stay dropped: {removed}"
        );
    }

    let prompt_deltas = column_names(&pool, &schema, "prompt_deltas").await;
    assert!(
        !prompt_deltas.iter().any(|column| column == "payload_json"),
        "prompt_deltas must not keep unread payload_json"
    );

    let skill_metrics = column_names(&pool, &schema, "skill_metrics").await;
    assert!(
        skill_metrics.iter().any(|column| column == "metric_slot"),
        "skill_metrics must carry metric_slot to scope aggregate uniqueness"
    );
    assert_eq!(
        unique_key_columns(&pool, &schema, "skill_metrics", "uq_skill_metrics_slot").await,
        ["skill_name", "metric_type", "metric_slot"],
        "skill_metrics must enforce one aggregate slot per skill without collapsing report rows"
    );

    assert!(
        !table_exists(&pool, &schema, "session_artifact_grants").await,
        "deprecated singular session_artifact_grants table must not exist"
    );

    for table in [
        "harness_items",
        "harness_skill_drafts",
        "harness_skill_rules",
    ] {
        let columns = column_names(&pool, &schema, table).await;
        assert!(
            columns
                .iter()
                .any(|column| column == "decision_history_json"),
            "{table} must retain inline decision history"
        );
    }

    let citation_columns = column_names(&pool, &schema, "harness_citations").await;
    for expected in [
        "source_snapshot_ref",
        "source_content_hash",
        "source_metadata_json",
    ] {
        assert!(
            citation_columns.iter().any(|column| column == expected),
            "harness_citations missing {expected}"
        );
    }
}

async fn current_schema(pool: &astra_core::SharedPool) -> String {
    let row = sqlx::query("SELECT DATABASE() AS db")
        .fetch_one(pool.get())
        .await
        .expect("SELECT DATABASE()");
    row.try_get::<String, _>("db").expect("db column")
}

async fn column_names(pool: &astra_core::SharedPool, schema: &str, table: &str) -> Vec<String> {
    sqlx::query(
        "SELECT COLUMN_NAME FROM INFORMATION_SCHEMA.COLUMNS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
         ORDER BY ORDINAL_POSITION",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool.get())
    .await
    .expect("load columns")
    .into_iter()
    .map(|row| row.try_get::<String, _>("COLUMN_NAME").unwrap())
    .collect()
}

async fn column_default(
    pool: &astra_core::SharedPool,
    schema: &str,
    table: &str,
    column: &str,
) -> Option<String> {
    sqlx::query(
        "SELECT COLUMN_DEFAULT FROM INFORMATION_SCHEMA.COLUMNS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = ?
         LIMIT 1",
    )
    .bind(schema)
    .bind(table)
    .bind(column)
    .fetch_optional(pool.get())
    .await
    .expect("load column default")
    .and_then(|row| row.try_get::<Option<String>, _>("COLUMN_DEFAULT").ok())
    .flatten()
}

async fn table_exists(pool: &astra_core::SharedPool, schema: &str, table: &str) -> bool {
    sqlx::query(
        "SELECT COUNT(*) AS count FROM INFORMATION_SCHEMA.TABLES
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
    )
    .bind(schema)
    .bind(table)
    .fetch_one(pool.get())
    .await
    .expect("load table existence")
    .try_get::<i64, _>("count")
    .unwrap_or(0)
        > 0
}

async fn unique_key_columns(
    pool: &astra_core::SharedPool,
    schema: &str,
    table: &str,
    key: &str,
) -> Vec<String> {
    sqlx::query(
        "SELECT COLUMN_NAME FROM INFORMATION_SCHEMA.STATISTICS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME = ? AND NON_UNIQUE = 0
         ORDER BY SEQ_IN_INDEX",
    )
    .bind(schema)
    .bind(table)
    .bind(key)
    .fetch_all(pool.get())
    .await
    .expect("load unique key columns")
    .into_iter()
    .map(|row| row.try_get::<String, _>("COLUMN_NAME").unwrap())
    .collect()
}

async fn primary_key_columns(
    pool: &astra_core::SharedPool,
    schema: &str,
    table: &str,
) -> Vec<String> {
    unique_key_columns(pool, schema, table, "PRIMARY").await
}

async fn index_columns(
    pool: &astra_core::SharedPool,
    schema: &str,
    table: &str,
    key: &str,
) -> Vec<String> {
    sqlx::query(
        "SELECT COLUMN_NAME FROM INFORMATION_SCHEMA.STATISTICS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME = ?
         ORDER BY SEQ_IN_INDEX",
    )
    .bind(schema)
    .bind(table)
    .bind(key)
    .fetch_all(pool.get())
    .await
    .expect("load index columns")
    .into_iter()
    .map(|row| row.try_get::<String, _>("COLUMN_NAME").unwrap())
    .collect()
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn phase1_run_durability_schema_contract() {
    let pool = common::setup_pool().await;
    let schema = current_schema(&pool).await;

    let agent_events = column_names(&pool, &schema, "agent_events").await;
    for expected in [
        "run_id",
        "parent_run_id",
        "turn_id",
        "turn_seq",
        "round_index",
        "tool_call_id",
        "parent_agent_id",
        "trace_kind",
    ] {
        assert!(
            agent_events.iter().any(|column| column == expected),
            "agent_events missing traceability column {expected}"
        );
    }
    assert_eq!(
        index_columns(&pool, &schema, "agent_events", "idx_agent_events_trace").await,
        ["session_id", "turn_id", "created_at"],
        "agent_events trace lookup must use session/turn/created ordering"
    );
    assert_eq!(
        index_columns(&pool, &schema, "agent_events", "idx_agent_events_run").await,
        ["session_id", "run_id", "created_at"],
        "agent_events run lookup must use session/run/created ordering"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_events",
            "idx_agent_events_parent_run"
        )
        .await,
        ["session_id", "parent_run_id", "created_at"],
        "agent_events parent-run lookup must use session/parent-run/created ordering"
    );
    assert_eq!(
        index_columns(&pool, &schema, "agent_events", "idx_agent_events_tool_call").await,
        ["session_id", "tool_call_id"],
        "agent_events tool-call lookup must use session/tool-call ordering"
    );

    let agent_sessions = column_names(&pool, &schema, "agent_sessions").await;
    for expected in ["active_plan_id", "config_version_id"] {
        assert!(
            agent_sessions.iter().any(|column| column == expected),
            "agent_sessions missing {expected}"
        );
    }
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_sessions",
            "idx_agent_sessions_config_version"
        )
        .await,
        ["config_version_id"],
        "session config-version lookup must use the current schema index"
    );

    assert_eq!(
        unique_key_columns(&pool, &schema, "agent_run_events", "uq_run_event_idx").await,
        ["run_id", "event_idx"],
        "agent_run_events must enforce one row per run/event_idx"
    );
    assert_eq!(
        unique_key_columns(
            &pool,
            &schema,
            "agent_run_events",
            "uq_run_event_idempotency"
        )
        .await,
        ["run_id", "idempotency_key"],
        "agent_run_events must dedupe idempotency_key per run"
    );

    let agent_runs = column_names(&pool, &schema, "agent_runs").await;
    for expected in [
        "root_run_id",
        "ancestor_path",
        "depth",
        "retry_of",
        "retry_scope",
        "owner_pod_id",
        "owner_lease_expires_at",
        "run_generation",
    ] {
        assert!(
            agent_runs.iter().any(|column| column == expected),
            "agent_runs missing {expected}"
        );
    }

    // The original assertion relied on `SHOW CREATE TABLE` preserving
    // CHECK constraints — true for MySQL 8.0+ but NOT for MatrixOne
    // (it accepts CHECK syntax at DDL time without storing the
    // constraint, so the dumped DDL omits it). The contract we
    // actually care about is "no row with an out-of-vocabulary
    // retry_scope ever lands" which the application enforces via
    // `validate_retry_scope` at insert/update time. Probe that path
    // instead of trusting the engine's DDL round-trip.
    let retry_scope_column = column_names(&pool, &schema, "agent_runs")
        .await
        .into_iter()
        .find(|column| column == "retry_scope");
    assert!(
        retry_scope_column.is_some(),
        "agent_runs must declare a retry_scope column for the application-level validator to bind to"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "prompt_request_records",
            "uq_prompt_request_attempt"
        )
        .await,
        [
            "user_id",
            "session_id",
            "turn",
            "round",
            "source",
            "attempt"
        ],
        "prompt request idempotency must be bound to owner/session attempt identity"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_runs",
            "idx_agent_runs_user_session_status_updated"
        )
        .await,
        ["user_id", "session_id", "status", "updated_at"],
        "active-run lookup must use owner/session/status ordering"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_run_events",
            "idx_agent_run_events_owner_session_run_idx"
        )
        .await,
        ["user_id", "session_id", "run_id", "event_idx"],
        "reasoning event replay must use owner/session/run/event ordering"
    );

    let batch_columns = column_names(&pool, &schema, "session_tool_output_batches").await;
    for expected in [
        "batch_id",
        "session_id",
        "run_id",
        "user_id",
        "output_count",
        "payload_bytes",
    ] {
        assert!(
            batch_columns.iter().any(|column| column == expected),
            "session_tool_output_batches missing {expected}"
        );
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn phase2_web_hydration_schema_contract() {
    let pool = common::setup_pool().await;
    let schema = current_schema(&pool).await;

    let leases = column_names(&pool, &schema, "session_device_leases").await;
    for expected in [
        "lease_id",
        "user_id",
        "session_id",
        "device_id",
        "device_fingerprint",
        "trust_level",
        "status",
        "last_monotonic_id",
        "expires_at",
        "revoked_at",
        "updated_at",
    ] {
        assert!(
            leases.iter().any(|column| column == expected),
            "session_device_leases missing {expected}"
        );
    }
    assert_eq!(
        unique_key_columns(&pool, &schema, "session_device_leases", "uq_session_device").await,
        ["user_id", "session_id", "device_id"],
        "device leases must enforce owner/session/device uniqueness"
    );

    let lease_events = column_names(&pool, &schema, "session_device_lease_events").await;
    for expected in [
        "lease_event_id",
        "lease_id",
        "session_id",
        "device_id",
        "device_fingerprint",
        "event_type",
        "reason",
        "ended_at_server",
    ] {
        assert!(
            lease_events.iter().any(|column| column == expected),
            "session_device_lease_events missing {expected}"
        );
    }

    let transcript_pk = primary_key_columns(&pool, &schema, "session_transcript_items").await;
    assert_eq!(
        transcript_pk,
        ["session_id", "item_seq"],
        "session_transcript_items must page by stable (session_id,item_seq) primary key"
    );
    let transcript_page_columns = column_names(&pool, &schema, "transcript_pages").await;
    assert!(
        transcript_page_columns
            .iter()
            .any(|column| column == "user_id"),
        "transcript_pages must carry physical owner scope"
    );
    assert!(
        column_default(&pool, &schema, "transcript_pages", "user_id")
            .await
            .is_none_or(|default| !default.trim_matches('\'').is_empty()),
        "transcript_pages.user_id must not use an empty-string owner sentinel"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "transcript_pages",
            "idx_transcript_pages_owner_session_end"
        )
        .await,
        ["user_id", "session_id", "end_item_seq"],
        "transcript page lookups must use owner/session/end index"
    );
    let ctx_snapshot_columns = column_names(&pool, &schema, "ctx_snapshots").await;
    assert!(
        ctx_snapshot_columns
            .iter()
            .any(|column| column == "user_id"),
        "ctx_snapshots must carry physical owner scope"
    );
    assert!(
        column_default(&pool, &schema, "ctx_snapshots", "user_id")
            .await
            .is_none_or(|default| !default.trim_matches('\'').is_empty()),
        "ctx_snapshots.user_id must not use an empty-string owner sentinel"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "ctx_snapshots",
            "idx_ctx_snapshots_owner_session_created"
        )
        .await,
        ["user_id", "session_id", "created_at"],
        "context snapshot lookups must use owner/session recency index"
    );
    let ctx_decision_columns = column_names(&pool, &schema, "ctx_decision_audits").await;
    assert!(
        ctx_decision_columns
            .iter()
            .any(|column| column == "user_id"),
        "ctx_decision_audits must carry physical owner scope"
    );
    assert!(
        column_default(&pool, &schema, "ctx_decision_audits", "user_id")
            .await
            .is_none_or(|default| !default.trim_matches('\'').is_empty()),
        "ctx_decision_audits.user_id must not use an empty-string owner sentinel"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "ctx_decision_audits",
            "idx_ctx_decisions_owner_session_type_created"
        )
        .await,
        ["user_id", "session_id", "decision_type", "created_at"],
        "decision audit lookups must use owner/session/type recency index"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "prompt_request_records",
            "idx_prompt_requests_owner_session_created"
        )
        .await,
        [
            "user_id",
            "session_id",
            "created_at",
            "turn",
            "round",
            "attempt"
        ],
        "session prompt observability must use owner/session recency index"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "prompt_request_records",
            "idx_prompt_requests_owner_run_created"
        )
        .await,
        [
            "user_id",
            "run_id",
            "created_at",
            "turn",
            "round",
            "attempt"
        ],
        "run prompt observability must use owner/run recency index"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "auth_audit_logs",
            "idx_auth_audit_logs_user_resource_created"
        )
        .await,
        ["user_id", "resource_type", "resource_id", "created_at"],
        "auth audit lookups must be observable by owner/resource recency"
    );
    assert_eq!(
        primary_key_columns(&pool, &schema, "tool_exactly_once_results").await,
        ["user_id", "session_id", "dedup_key"],
        "tool exactly-once recovery must dedupe within the owner/session boundary"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "tool_exactly_once_results",
            "idx_tool_exactly_once_session"
        )
        .await,
        ["session_id", "user_id"],
        "session deletion and consistency checks must find exactly-once rows by session and owner"
    );

    let revision_columns = column_names(&pool, &schema, "session_state_revisions").await;
    for expected in [
        "monotonic_id",
        "revision_hash",
        "device_fingerprint",
        "transcript_high_watermark",
        "run_event_high_watermark",
        "state_projection_hash",
    ] {
        assert!(
            revision_columns.iter().any(|column| column == expected),
            "session_state_revisions missing {expected}"
        );
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn phase3_context_manifest_schema_contract() {
    let pool = common::setup_pool().await;
    let schema = current_schema(&pool).await;

    // reason types are a Rust constant (CONTEXT_MANIFEST_REASONS), not a DB table.
    let reasons = astra_services::context_manifest::CONTEXT_MANIFEST_REASONS
        .iter()
        .map(|(r, _, _)| r.to_string())
        .chain(std::iter::once("other".to_string()))
        .collect::<Vec<_>>();
    for expected in [
        "initial_turn",
        "normal_turn",
        "post_compaction",
        "history_recall_structured",
        "history_recall_fts",
        "history_recall_vector",
        "large_tool_output_gated",
        "plan_subtree_query",
        "tree_structured_report",
        "workspace_switch",
        "approval_resume",
        "cross_session_recall",
        "delegation_poll",
        "partial_blocker_review",
        "delegation_aggregate",
        "cross_skill_alignment",
        "skill_quality_review",
        "final_delivery_summary",
        "ambiguity_clarification",
        "execute_after_clarification",
        "user_memory_promote",
        "user_memory_archive",
        "user_memory_revise",
        "user_memory_loaded_on_init",
        "progressive_loading",
        "intent_driven_preview_expand",
        "other",
    ] {
        assert!(
            reasons.iter().any(|reason| reason == expected),
            "context_manifest_reason_types missing {expected}"
        );
    }

    let manifests = column_names(&pool, &schema, "context_manifests").await;
    for expected in [
        "turn_intent",
        "tokenizer_id",
        "budget_template_id",
        "reason",
        "dropped_count",
    ] {
        assert!(
            manifests.iter().any(|column| column == expected),
            "context_manifests missing {expected}"
        );
    }
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "context_manifests",
            "idx_ctx_manifest_owner_session_created"
        )
        .await,
        ["user_id", "session_id", "created_at", "manifest_id"],
        "latest context manifest lookup must use owner/session recency index"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "context_manifests",
            "idx_ctx_manifest_owner_session_run_created"
        )
        .await,
        [
            "user_id",
            "session_id",
            "run_id",
            "created_at",
            "manifest_id"
        ],
        "run-specific context manifest lookup must use owner/session/run recency index"
    );

    let items = column_names(&pool, &schema, "context_manifest_items").await;
    for expected in ["render_mode", "included", "raw_ref", "budget_tokens"] {
        assert!(
            items.iter().any(|column| column == expected),
            "context_manifest_items missing {expected}"
        );
    }
    assert!(
        !items.iter().any(|column| column == "user_id"),
        "context_manifest_items must inherit owner scope through context_manifests, not store a second owner column"
    );

    let render_modes = sqlx::query(
        "SELECT COUNT(*) AS count FROM context_manifest_items
         WHERE render_mode = 'code_block_preserved'",
    )
    .fetch_one(pool.get())
    .await;
    assert!(
        render_modes.is_ok(),
        "context_manifest_items.render_mode must accept code_block_preserved"
    );

    let raw_ref_schemes = sqlx::query("SELECT scheme FROM raw_ref_scheme_registry")
        .fetch_all(pool.get())
        .await
        .expect("load raw_ref schemes")
        .into_iter()
        .map(|row| row.try_get::<String, _>("scheme").unwrap())
        .collect::<Vec<_>>();
    for expected in ["artifact", "s3", "conversation_log"] {
        assert!(
            raw_ref_schemes.iter().any(|scheme| scheme == expected),
            "raw_ref_scheme_registry missing {expected}"
        );
    }

    let preview_count = sqlx::query(
        "SELECT COUNT(*) AS count FROM preview_template_registry WHERE status = 'active'",
    )
    .fetch_one(pool.get())
    .await
    .expect("preview template count")
    .try_get::<i64, _>("count")
    .unwrap();
    assert!(
        preview_count >= 18,
        "preview_template_registry should seed at least 18 baseline templates"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn phase4_state_projection_schema_contract() {
    let pool = common::setup_pool().await;
    let schema = current_schema(&pool).await;

    let conversation_log_columns = column_names(&pool, &schema, "conversation_log").await;
    for expected in ["user_id", "session_id", "seq", "turn", "entry_type"] {
        assert!(
            conversation_log_columns
                .iter()
                .any(|column| column == expected),
            "conversation_log missing {expected}"
        );
    }
    assert_eq!(
        primary_key_columns(&pool, &schema, "conversation_log").await,
        ["user_id", "session_id", "seq"],
        "conversation_log identity must be owner/session/seq"
    );
    assert!(
        column_default(&pool, &schema, "conversation_log", "user_id")
            .await
            .is_none_or(|default| !default.trim_matches('\'').is_empty()),
        "conversation_log.user_id must not use an empty-string owner sentinel"
    );
    assert_eq!(
        index_columns(&pool, &schema, "conversation_log", "idx_csl_owner_snapshot").await,
        ["user_id", "session_id", "entry_type", "seq"],
        "CSL latest-snapshot lookup must use owner/session index"
    );
    assert_eq!(
        index_columns(&pool, &schema, "conversation_log", "idx_csl_owner_turn").await,
        ["user_id", "session_id", "turn"],
        "CSL fork/read-by-turn lookup must use owner/session index"
    );
    assert!(
        index_columns(&pool, &schema, "conversation_log", "idx_csl_snapshot")
            .await
            .is_empty(),
        "conversation_log must not keep obsolete session-only snapshot index"
    );
    assert!(
        index_columns(&pool, &schema, "conversation_log", "idx_csl_turn")
            .await
            .is_empty(),
        "conversation_log must not keep obsolete session-only turn index"
    );

    let todo_counter_columns = column_names(&pool, &schema, "session_todo_counters").await;
    for expected in ["user_id", "session_id", "next_id", "version"] {
        assert!(
            todo_counter_columns.iter().any(|column| column == expected),
            "session_todo_counters missing {expected}"
        );
    }
    assert!(
        column_default(&pool, &schema, "session_todo_counters", "user_id")
            .await
            .is_none_or(|default| !default.trim_matches('\'').is_empty()),
        "session_todo_counters.user_id must not use an empty-string owner sentinel"
    );
    assert_eq!(
        primary_key_columns(&pool, &schema, "session_todo_counters").await,
        ["user_id", "session_id"],
        "session todo counters must be owner-bound at the uniqueness boundary"
    );
    assert!(
        index_columns(
            &pool,
            &schema,
            "session_todo_counters",
            "idx_session_todo_counters_owner_session"
        )
        .await
        .is_empty(),
        "session_todo_counters must not keep a redundant owner/session secondary index"
    );
    assert_eq!(
        primary_key_columns(&pool, &schema, "session_todo_idempotency").await,
        ["session_id", "user_id", "action", "idempotency_key"],
        "todo idempotency ledger must dedupe by session/owner/action/key"
    );

    let state_items = column_names(&pool, &schema, "session_state_items").await;
    for expected in [
        "scope",
        "category",
        "item_key",
        "origin_session_id",
        "origin_chunk_id",
        "origin_state_item_id",
    ] {
        assert!(
            state_items.iter().any(|column| column == expected),
            "session_state_items missing {expected}"
        );
    }
    assert_eq!(
        unique_key_columns(&pool, &schema, "session_state_items", "uq_state_current").await,
        ["user_id", "session_id", "scope", "category", "item_key"],
        "state current projection must upsert by owner/session semantic key"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_state_items",
            "idx_state_owner_session_status_category"
        )
        .await,
        ["user_id", "session_id", "status", "category"],
        "state summary lookup must use owner/session/status/category index"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_state_items",
            "idx_state_user_scope_category"
        )
        .await,
        ["user_id", "scope", "category", "status", "priority"],
        "scope=user memory must use a compound user/scope/category index"
    );

    let state_events = column_names(&pool, &schema, "session_state_item_events").await;
    for expected in [
        "mutation",
        "previous_hash",
        "next_hash",
        "previous_version",
        "next_version",
    ] {
        assert!(
            state_events.iter().any(|column| column == expected),
            "session_state_item_events missing {expected}"
        );
    }

    let delegations = column_names(&pool, &schema, "session_delegations").await;
    for expected in [
        "delegation_id",
        "user_id",
        "session_id",
        "parent_run_id",
        "child_run_id",
        "root_run_id",
        "ancestor_path",
        "depth",
        "agent_id",
        "title",
        "status",
        "retry_of",
        "retry_scope",
        "last_summary_ref",
        "last_summary_text",
    ] {
        assert!(
            delegations.iter().any(|column| column == expected),
            "session_delegations missing {expected}"
        );
    }

    let artifacts = column_names(&pool, &schema, "session_artifacts").await;
    for expected in [
        "access_scope",
        "owner_run_id",
        "owner_delegation_id",
        "root_run_id",
        "retention_policy",
        "status",
    ] {
        assert!(
            artifacts.iter().any(|column| column == expected),
            "session_artifacts missing {expected}"
        );
    }
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_artifacts",
            "idx_session_artifacts_owner_kind_order"
        )
        .await,
        [
            "user_id",
            "session_id",
            "artifact_kind",
            "created_at",
            "artifact_id"
        ],
        "owner-bound artifact latest/list-by-kind reads must use user/session/kind ordering"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_artifacts",
            "idx_session_artifacts_owner_session_order"
        )
        .await,
        ["user_id", "session_id", "created_at", "artifact_id"],
        "owner-bound artifact preview reads must use user/session ordering"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_artifacts",
            "idx_artifacts_root_scope"
        )
        .await,
        ["root_run_id", "access_scope", "status", "updated_at"],
        "artifact same-root ACL must use root/scope index"
    );

    let grants = column_names(&pool, &schema, "session_artifacts_grants").await;
    for expected in [
        "grant_id",
        "artifact_id",
        "root_run_id",
        "source_run_id",
        "target_run_id",
        "target_delegation_id",
        "grant_scope",
    ] {
        assert!(
            grants.iter().any(|column| column == expected),
            "session_artifacts_grants missing {expected}"
        );
    }
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_artifacts_grants",
            "idx_artifacts_grants_target"
        )
        .await,
        ["target_run_id", "artifact_id", "expires_at"],
        "run artifact grants must use a target/artifact/expiry index"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_artifacts_grants",
            "idx_artifacts_grants_delegation_target"
        )
        .await,
        ["target_delegation_id", "artifact_id", "expires_at"],
        "delegation artifact grants must use a target/artifact/expiry index"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn phase5_personal_skill_schema_contract() {
    let pool = common::setup_pool().await;
    let schema = current_schema(&pool).await;

    let sources = column_names(&pool, &schema, "user_skill_sources").await;
    for expected in [
        "source_id",
        "owner_user_id",
        "skill_name",
        "visibility",
        "status",
        "created_at",
        "updated_at",
    ] {
        assert!(
            sources.iter().any(|column| column == expected),
            "user_skill_sources missing {expected}"
        );
    }
    assert_eq!(
        unique_key_columns(
            &pool,
            &schema,
            "user_skill_sources",
            "uq_user_skill_source_name"
        )
        .await,
        ["owner_user_id", "skill_name"],
        "personal skill lookup must be keyed by owner_user_id + skill_name"
    );

    let versions = column_names(&pool, &schema, "user_skill_versions").await;
    for expected in [
        "version_id",
        "source_id",
        "owner_user_id",
        "skill_name",
        "version",
        "manifest_json",
        "content_markdown",
        "content_hash",
        "normalize_version",
        "token_estimate",
        "status",
    ] {
        assert!(
            versions.iter().any(|column| column == expected),
            "user_skill_versions missing {expected}"
        );
    }
    assert_eq!(
        unique_key_columns(
            &pool,
            &schema,
            "user_skill_versions",
            "uq_user_skill_source_version"
        )
        .await,
        ["source_id", "version"],
        "skill versions must be append-only per source/version"
    );

    let evaluations = column_names(&pool, &schema, "user_skill_evaluations").await;
    for expected in [
        "evaluation_id",
        "source_id",
        "version_id",
        "run_id",
        "hits",
        "suspects",
        "false_positives",
        "payload_json",
        "created_at",
    ] {
        assert!(
            evaluations.iter().any(|column| column == expected),
            "user_skill_evaluations missing {expected}"
        );
    }

    let installations = column_names(&pool, &schema, "skill_installations").await;
    for expected in [
        "scope",
        "session_id",
        "workspace_id",
        "auto_activate_on_topic_match",
    ] {
        assert!(
            installations.iter().any(|column| column == expected),
            "skill_installations missing Phase 5 column {expected}"
        );
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn phase6_artifact_retention_preview_schema_contract() {
    let pool = common::setup_pool().await;
    let schema = current_schema(&pool).await;

    let artifacts = column_names(&pool, &schema, "session_artifacts").await;
    for expected in [
        "retention_policy",
        "retention_until",
        "status",
        "referenced_by_manifest_count",
        "referenced_by_state_items_count",
        "referenced_by_citation_count",
    ] {
        assert!(
            artifacts.iter().any(|column| column == expected),
            "session_artifacts missing Phase 6 column {expected}"
        );
    }
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_artifacts",
            "idx_artifacts_retention"
        )
        .await,
        ["status", "retention_until", "retention_policy"],
        "retention GC must use status/retention_until/retention_policy index"
    );

    let tool_outputs = column_names(&pool, &schema, "session_tool_outputs").await;
    for expected in [
        "preview_text",
        "preview_status",
        "artifact_ref",
        "content_hash",
        "normalize_version",
    ] {
        assert!(
            tool_outputs.iter().any(|column| column == expected),
            "session_tool_outputs missing preview column {expected}"
        );
    }
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_tool_outputs",
            "idx_tool_outputs_artifact_ref"
        )
        .await,
        ["artifact_ref"],
        "tool output artifact refs must be directly indexed"
    );

    let preview_templates = column_names(&pool, &schema, "preview_template_registry").await;
    for expected in [
        "tool_name",
        "version",
        "max_preview_bytes",
        "normalize_version",
        "schema_json",
    ] {
        assert!(
            preview_templates.iter().any(|column| column == expected),
            "preview_template_registry missing {expected}"
        );
    }
    let template_count = sqlx::query(
        "SELECT COUNT(*) AS count FROM preview_template_registry WHERE status = 'active'",
    )
    .fetch_one(pool.get())
    .await
    .expect("count preview templates")
    .try_get::<i64, _>("count")
    .unwrap_or(0);
    assert!(
        template_count >= 18,
        "expected at least 18 active preview templates, got {template_count}"
    );
    let required_templates = sqlx::query(
        "SELECT tool_name FROM preview_template_registry
         WHERE tool_name IN ('pg_dump', 'fetch_url', 'parse_pdf', 'SKILL.md', 'cargo', 'rustc',
          'clippy', 'sql_compat_scan', 'pg_schema_structurize', 'slow_query_analyzer', 'curl',
          'git_log', 'docker_logs', 'kubectl', 'python_stdout', 'npm_build', 'csv_head',
          'json_preview', 'markdown_preview')
         AND status = 'active'",
    )
    .fetch_all(pool.get())
    .await
    .expect("load required preview templates");
    assert!(
        required_templates.len() >= 18,
        "required Phase 6 template set is incomplete"
    );

    let scheme_rows = sqlx::query(
        "SELECT scheme FROM raw_ref_scheme_registry
         WHERE scheme IN ('artifact', 's3', 'conversation_log', 'tool_output', 'chunk', 'state_item')
           AND is_active = 1",
    )
    .fetch_all(pool.get())
    .await
    .expect("load raw_ref schemes");
    assert!(
        scheme_rows.len() >= 6,
        "raw_ref_scheme_registry missing Phase 6 schemes"
    );
}
