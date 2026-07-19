mod common;

use sqlx::Row;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use astra_services::storage::{ensure_core_schema, load_core_schema_table_contracts};

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
async fn core_schema_catalog_matches_live_idempotent_bootstrap() {
    let (pool, settings) = common::setup_pool_and_settings().await;
    let bootstrap_catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    ensure_core_schema(&settings, &bootstrap_catalog)
        .await
        .expect("second core schema bootstrap");
    ensure_core_schema(&settings, &bootstrap_catalog)
        .await
        .expect("third core schema bootstrap");

    let schema = current_schema(&pool).await;
    let existing =
        sqlx::query("SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = ?")
            .bind(&schema)
            .fetch_all(pool.get())
            .await
            .expect("load live schema catalog")
            .into_iter()
            .map(|row| row.try_get::<String, _>("TABLE_NAME").unwrap())
            .collect::<BTreeSet<_>>();
    let contracts = load_core_schema_table_contracts(pool.get())
        .await
        .expect("load generated core schema table authority");
    assert!(
        contracts.len() >= 90,
        "bootstrap must publish the complete generated authority, got {} claims",
        contracts.len()
    );
    let missing = contracts
        .iter()
        .filter(|table| !existing.contains(&table.name))
        .map(|table| format!("{} ({})", table.name, table.owner))
        .collect::<Vec<_>>();
    assert!(missing.is_empty(), "missing catalog tables: {missing:?}");
    assert!(contracts.iter().all(|table| table.ddl_sha256.len() == 64));

    let contracts = sqlx::query(
        "SELECT COUNT(*) AS count FROM astra_schema_contracts WHERE component = 'astra-core'",
    )
    .fetch_one(pool.get())
    .await
    .expect("count core schema contract rows")
    .try_get::<i64, _>("count")
    .unwrap();
    assert_eq!(contracts, 1, "repeated bootstrap must remain idempotent");
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
        "task_verification_results",
    ] {
        assert!(
            !table_exists(&pool, &schema, removed).await,
            "removed table must stay dropped: {removed}"
        );
    }

    let prompt_deltas = column_names(&pool, &schema, "prompt_deltas").await;
    for expected in ["user_id", "session_id", "request_id", "delta_seq"] {
        assert!(
            prompt_deltas.iter().any(|column| column == expected),
            "prompt_deltas missing explicit owner-bound key column {expected}"
        );
    }
    assert!(
        !prompt_deltas.iter().any(|column| column == "delta_id"),
        "prompt_deltas must not keep obsolete synthetic delta_id primary key"
    );
    assert!(
        !prompt_deltas.iter().any(|column| column == "payload_json"),
        "prompt_deltas must not keep unread payload_json"
    );
    assert_eq!(
        primary_key_columns(&pool, &schema, "prompt_deltas").await,
        ["user_id", "session_id", "request_id", "delta_seq"],
        "prompt_deltas primary key must carry the owner/session boundary"
    );
    assert_eq!(
        primary_key_columns(&pool, &schema, "eval_calibration_assessments").await,
        ["user_id", "calibration_id"],
        "eval_calibration_assessments identity must be owner-bound"
    );
    assert_eq!(
        primary_key_columns(&pool, &schema, "agent_sessions").await,
        ["user_id", "session_id"],
        "agent_sessions primary key must carry the owner boundary"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "prompt_deltas",
            "idx_prompt_deltas_owner_request_position"
        )
        .await,
        [
            "user_id",
            "session_id",
            "request_id",
            "position",
            "delta_seq"
        ],
        "prompt_deltas previous-chunk lookup must stay owner/session/request scoped"
    );
    for removed_index in [
        "uq_prompt_delta_request_seq",
        "idx_prompt_deltas_request_position",
    ] {
        assert!(
            index_columns(&pool, &schema, "prompt_deltas", removed_index)
                .await
                .is_empty(),
            "prompt_deltas must not keep ownerless index {removed_index}"
        );
    }

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

    let agent_tasks = column_names(&pool, &schema, "agent_tasks").await;
    for expected in ["task_id", "user_id", "session_id", "parent_task_id"] {
        assert!(
            agent_tasks.iter().any(|column| column == expected),
            "agent_tasks missing {expected}"
        );
    }
    assert_eq!(
        primary_key_columns(&pool, &schema, "agent_runs").await,
        ["user_id", "run_id"],
        "agent_runs primary key must carry the owner boundary"
    );
    assert_eq!(
        primary_key_columns(&pool, &schema, "prompt_request_records").await,
        ["user_id", "request_id"],
        "prompt_request_records primary key must carry the owner boundary"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_tasks",
            "idx_tasks_owner_session_updated"
        )
        .await,
        ["user_id", "session_id", "updated_at"],
        "session task lists and session lifecycle deletes must use owner/session ordering"
    );
    for removed_index in ["idx_tasks_session_updated", "idx_tasks_parent_updated"] {
        assert!(
            index_columns(&pool, &schema, "agent_tasks", removed_index)
                .await
                .is_empty(),
            "agent_tasks must not keep obsolete ownerless index {removed_index}"
        );
    }

    assert_eq!(
        primary_key_columns(&pool, &schema, "task_leases").await,
        ["user_id", "task_id"],
        "task leases must be owned at the physical identity boundary"
    );
    assert_eq!(
        index_columns(&pool, &schema, "task_leases", "idx_task_leases_expires").await,
        ["expires_at"],
        "task lease retention cleanup must have a purpose-built global expiry index"
    );
    assert!(
        index_columns(
            &pool,
            &schema,
            "task_leases",
            "idx_task_leases_user_expires"
        )
        .await
        .is_empty(),
        "task_leases must not keep the old owner-prefixed index that cannot serve global expiry cleanup"
    );

    assert_eq!(
        primary_key_columns(&pool, &schema, "edge_agent_registry").await,
        ["user_id", "registry_id"],
        "edge registry identity must be owner-bound so registry_id lookups never scan across tenants"
    );

    let task_contracts = column_names(&pool, &schema, "task_contracts").await;
    for expected in ["user_id", "session_id", "contract_id", "task_id"] {
        assert!(
            task_contracts.iter().any(|column| column == expected),
            "task_contracts missing {expected}"
        );
    }
    assert_eq!(
        primary_key_columns(&pool, &schema, "task_contracts").await,
        ["user_id", "contract_id"],
        "task_contracts primary key must make contract identity owner-scoped"
    );
    for column in ["user_id", "session_id", "contract_id", "task_id"] {
        assert!(
            column_character_maximum_length(&pool, &schema, "task_contracts", column).await
                >= Some(64),
            "task_contracts.{column} must not assume 36-character UUID-only identifiers"
        );
    }
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "task_contracts",
            "idx_tc_owner_task_status_version"
        )
        .await,
        ["user_id", "task_id", "status", "version"],
        "durable task contract lookup must be owner-bound before task/status/version ordering"
    );
    assert!(
        index_columns(&pool, &schema, "task_contracts", "idx_tc_task")
            .await
            .is_empty(),
        "task_contracts must not keep obsolete ownerless task index idx_tc_task"
    );

    let verification_results = column_names(&pool, &schema, "verification_results").await;
    for expected in [
        "user_id",
        "session_id",
        "contract_id",
        "task_id",
        "status",
        "subtask_id",
    ] {
        assert!(
            verification_results.iter().any(|column| column == expected),
            "verification_results missing {expected}"
        );
    }
    assert!(
        !verification_results.iter().any(|column| column == "passed"),
        "verification_results must derive pass/fail from status instead of a redundant passed column"
    );
    assert_eq!(
        primary_key_columns(&pool, &schema, "verification_results").await,
        ["user_id", "result_id"],
        "verification_results result_id is the owner-scoped row identity; contract history uses explicit secondary indexes"
    );
    assert_eq!(
        column_nullable(&pool, &schema, "verification_results", "user_id").await,
        Some(false),
        "verification results must be owner-scoped at write time"
    );
    for column in [
        "user_id",
        "session_id",
        "contract_id",
        "task_id",
        "result_id",
    ] {
        assert!(
            column_character_maximum_length(&pool, &schema, "verification_results", column).await
                >= Some(64),
            "verification_results.{column} must not assume 36-character UUID-only identifiers"
        );
    }
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "verification_results",
            "idx_verification_results_contract_created"
        )
        .await,
        ["user_id", "contract_id", "created_at", "result_id"],
        "verification history reads must use owner/contract ordering"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "verification_results",
            "idx_verification_results_contract_subtask"
        )
        .await,
        ["user_id", "contract_id", "subtask_id", "created_at"],
        "verification subtask history reads must use owner/contract/subtask ordering"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "verification_results",
            "idx_verification_results_status_created"
        )
        .await,
        ["user_id", "status", "created_at"],
        "verification review/failure scans must be owner/status scoped"
    );
    for removed_index in [
        "idx_tvr_task_subtask",
        "idx_tvr_contract",
        "idx_tvr_owner_task_subtask",
        "idx_tvr_owner_contract",
    ] {
        assert!(
            index_columns(&pool, &schema, "verification_results", removed_index)
                .await
                .is_empty(),
            "verification_results must not keep obsolete task_verification_results index {removed_index}"
        );
    }

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

    let harness_snapshots = column_names(&pool, &schema, "harness_snapshots").await;
    for expected in [
        "user_id",
        "session_id",
        "turn_number",
        "causal_chain_id",
        "created_at",
    ] {
        assert!(
            harness_snapshots.iter().any(|column| column == expected),
            "harness_snapshots missing {expected}"
        );
    }
    assert_eq!(
        column_nullable(&pool, &schema, "harness_snapshots", "user_id").await,
        Some(false),
        "harness_snapshots.user_id must be required for owner-bound history reads"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "harness_snapshots",
            "idx_harness_owner_session_created"
        )
        .await,
        ["user_id", "session_id", "created_at"],
        "harness snapshot history reads must be owner-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "harness_snapshots",
            "idx_harness_owner_session_turn"
        )
        .await,
        ["user_id", "session_id", "turn_number"],
        "harness snapshot turn lookups must be owner-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "harness_snapshots",
            "idx_harness_owner_chain"
        )
        .await,
        ["user_id", "causal_chain_id"],
        "harness snapshot chain lookups must be owner-bound"
    );
    for removed_index in [
        "idx_harness_session",
        "idx_harness_session_turn",
        "idx_harness_chain",
    ] {
        assert!(
            index_columns(&pool, &schema, "harness_snapshots", removed_index)
                .await
                .is_empty(),
            "harness_snapshots must not keep ownerless index {removed_index}"
        );
    }

    let harness_runs = column_names(&pool, &schema, "harness_runs").await;
    for expected in ["harness_run_id", "user_id", "session_id", "updated_at"] {
        assert!(
            harness_runs.iter().any(|column| column == expected),
            "harness_runs missing {expected}"
        );
    }
    assert_eq!(
        primary_key_columns(&pool, &schema, "run_display_projections").await,
        ["user_id", "run_id"],
        "run_display_projections primary key must carry the owner boundary"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "harness_runs",
            "idx_harness_runs_owner_session_updated"
        )
        .await,
        ["user_id", "session_id", "updated_at"],
        "harness run session lifecycle paths must be owner/session scoped"
    );
    assert!(
        index_columns(&pool, &schema, "harness_runs", "idx_harness_runs_session")
            .await
            .is_empty(),
        "harness_runs must not keep ownerless session index idx_harness_runs_session"
    );

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

    let eval_feedback = column_names(&pool, &schema, "eval_user_feedback").await;
    for expected in ["feedback_id", "user_id", "session_id", "created_at"] {
        assert!(
            eval_feedback.iter().any(|column| column == expected),
            "eval_user_feedback missing {expected}"
        );
    }
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "eval_user_feedback",
            "idx_euf_owner_session_created"
        )
        .await,
        ["user_id", "session_id", "created_at"],
        "feedback session cleanup must be owner/session scoped"
    );
    assert!(
        index_columns(&pool, &schema, "eval_user_feedback", "idx_euf_session")
            .await
            .is_empty(),
        "eval_user_feedback must not keep ownerless session index idx_euf_session"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn evaluation_schema_supports_calibration_reads() {
    let pool = common::setup_pool().await;
    let schema = current_schema(&pool).await;

    let calibration = column_names(&pool, &schema, "eval_calibration_assessments").await;
    for expected in [
        "calibration_id",
        "user_id",
        "agent_id",
        "session_id",
        "confidence",
        "quality_score",
        "created_at",
    ] {
        assert!(
            calibration.iter().any(|column| column == expected),
            "eval_calibration_assessments missing {expected}"
        );
    }
    assert_eq!(
        column_character_maximum_length(&pool, &schema, "eval_calibration_assessments", "user_id")
            .await,
        Some(128),
        "eval_calibration_assessments.user_id must use the standard owner width"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "eval_calibration_assessments",
            "idx_eval_calibration_user_created"
        )
        .await,
        ["user_id", "created_at"],
        "calibration reads without agent_id must use user/created ordering"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "eval_calibration_assessments",
            "idx_eval_calibration_user_agent_created"
        )
        .await,
        ["user_id", "agent_id", "created_at"],
        "calibration reads with agent_id must use user/agent/created ordering"
    );
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

async fn column_nullable(
    pool: &astra_core::SharedPool,
    schema: &str,
    table: &str,
    column: &str,
) -> Option<bool> {
    sqlx::query(
        "SELECT IS_NULLABLE FROM INFORMATION_SCHEMA.COLUMNS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = ?
         LIMIT 1",
    )
    .bind(schema)
    .bind(table)
    .bind(column)
    .fetch_optional(pool.get())
    .await
    .expect("load column nullability")
    .and_then(|row| row.try_get::<String, _>("IS_NULLABLE").ok())
    .map(|nullable| nullable.eq_ignore_ascii_case("YES"))
}

async fn column_character_maximum_length(
    pool: &astra_core::SharedPool,
    schema: &str,
    table: &str,
    column: &str,
) -> Option<u64> {
    sqlx::query(
        "SELECT CHARACTER_MAXIMUM_LENGTH FROM INFORMATION_SCHEMA.COLUMNS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = ?
         LIMIT 1",
    )
    .bind(schema)
    .bind(table)
    .bind(column)
    .fetch_optional(pool.get())
    .await
    .expect("load column character maximum length")
    .and_then(|row| {
        row.try_get::<Option<i64>, _>("CHARACTER_MAXIMUM_LENGTH")
            .ok()
    })
    .flatten()
    .and_then(|width| u64::try_from(width).ok())
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
        primary_key_columns(&pool, &schema, "agent_events").await,
        ["user_id", "event_id"],
        "agent_events identity must be owner-bound so cross-tenant event ids do not collide"
    );
    for column in ["event_id", "parent_event_id", "causal_chain_id"] {
        assert_eq!(
            column_character_maximum_length(&pool, &schema, "agent_events", column).await,
            Some(128),
            "agent_events.{column} must fit content-addressed event ids"
        );
    }
    assert_eq!(
        index_columns(&pool, &schema, "agent_events", "idx_agent_events_trace").await,
        ["user_id", "session_id", "turn_id", "created_at"],
        "agent_events trace lookup must be owner-bound before session/turn/created ordering"
    );
    assert_eq!(
        index_columns(&pool, &schema, "agent_events", "idx_agent_events_run").await,
        ["user_id", "session_id", "run_id", "created_at"],
        "agent_events run lookup must be owner-bound before session/run/created ordering"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_events",
            "idx_agent_events_owner_session_created"
        )
        .await,
        ["user_id", "session_id", "created_at"],
        "agent_events owner-bound session scans must not post-filter by user_id"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_events",
            "idx_agent_events_owner_session_type_created"
        )
        .await,
        ["user_id", "session_id", "event_type", "created_at"],
        "agent_events typed session scans must stay owner-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_events",
            "idx_agent_events_owner_session_model_created"
        )
        .await,
        ["user_id", "session_id", "llm_model_used", "created_at"],
        "agent_events latest-model session scans must stay owner-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_events",
            "idx_agent_events_owner_session_parent"
        )
        .await,
        ["user_id", "session_id", "parent_event_id"],
        "agent_events parent-event session scans must stay owner-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_events",
            "idx_agent_events_owner_causal_chain_created"
        )
        .await,
        ["user_id", "causal_chain_id", "created_at", "event_id"],
        "agent_events causal-chain reads must stay owner-bound and ordered"
    );
    for removed_index in [
        "idx_agent_events_session_created",
        "idx_agent_events_session_type_created",
        "idx_agent_events_session_model_created",
        "idx_agent_events_session_parent",
        "idx_agent_events_causal_chain_id",
    ] {
        assert!(
            index_columns(&pool, &schema, "agent_events", removed_index)
                .await
                .is_empty(),
            "agent_events must not keep ownerless session index {removed_index}"
        );
    }
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_events",
            "idx_agent_events_parent_run"
        )
        .await,
        ["user_id", "session_id", "parent_run_id", "created_at"],
        "agent_events parent-run lookup must be owner-bound before session/parent-run/created ordering"
    );
    assert_eq!(
        index_columns(&pool, &schema, "agent_events", "idx_agent_events_tool_call").await,
        ["user_id", "session_id", "tool_call_id"],
        "agent_events tool-call lookup must be owner-bound before session/tool-call ordering"
    );

    let event_edges = column_names(&pool, &schema, "agent_event_edges").await;
    for expected in ["user_id", "session_id", "child_event_id", "parent_event_id"] {
        assert!(
            event_edges.iter().any(|column| column == expected),
            "agent_event_edges missing {expected}"
        );
    }
    assert_eq!(
        primary_key_columns(&pool, &schema, "agent_event_edges").await,
        [
            "user_id",
            "child_event_id",
            "parent_event_id",
            "relation_kind"
        ],
        "event edge identity must be owner-bound"
    );
    for column in ["child_event_id", "parent_event_id"] {
        assert_eq!(
            column_character_maximum_length(&pool, &schema, "agent_event_edges", column).await,
            Some(128),
            "agent_event_edges.{column} must fit content-addressed event ids"
        );
    }
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_event_edges",
            "idx_agent_event_edges_owner_session_child"
        )
        .await,
        ["user_id", "session_id", "child_event_id"],
        "session lifecycle deletes must use owner/session edge ordering"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_event_edges",
            "idx_agent_event_edges_owner_child"
        )
        .await,
        ["user_id", "child_event_id", "parent_order"],
        "parent hydration must read edges through the owner boundary"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_event_edges",
            "idx_agent_event_edges_owner_parent"
        )
        .await,
        ["user_id", "parent_event_id", "parent_order"],
        "event deletion must remove parent edges through the owner boundary"
    );
    for removed_index in [
        "idx_agent_event_edges_child",
        "idx_agent_event_edges_parent",
    ] {
        assert!(
            index_columns(&pool, &schema, "agent_event_edges", removed_index)
                .await
                .is_empty(),
            "agent_event_edges must not keep obsolete ownerless index {removed_index}"
        );
    }

    let agent_sessions = column_names(&pool, &schema, "agent_sessions").await;
    for expected in ["active_plan_id", "config_version_id"] {
        assert!(
            agent_sessions.iter().any(|column| column == expected),
            "agent_sessions missing {expected}"
        );
    }
    assert_eq!(
        column_character_maximum_length(&pool, &schema, "agent_sessions", "last_event_id").await,
        Some(128),
        "agent_sessions.last_event_id must fit content-addressed event ids"
    );
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
        primary_key_columns(&pool, &schema, "agent_run_events").await,
        ["user_id", "id"],
        "agent_run_events primary key must carry the owner boundary"
    );
    assert!(
        column_names(&pool, &schema, "agent_run_events")
            .await
            .iter()
            .any(|column| column == "interaction_request_id"),
        "agent_run_events must normalize durable interaction identity"
    );
    assert_eq!(
        unique_key_columns(&pool, &schema, "agent_run_events", "uq_run_event_idx").await,
        ["user_id", "run_id", "event_idx"],
        "agent_run_events must enforce one row per owner/run/event_idx"
    );
    assert_eq!(
        unique_key_columns(
            &pool,
            &schema,
            "agent_run_events",
            "uq_run_event_idempotency"
        )
        .await,
        ["user_id", "run_id", "idempotency_key"],
        "agent_run_events must dedupe idempotency_key per owner/run"
    );
    assert_eq!(
        unique_key_columns(
            &pool,
            &schema,
            "run_checkpoints",
            "uniq_run_checkpoint_idem"
        )
        .await,
        ["user_id", "run_id", "checkpoint_kind", "idempotency_key"],
        "run checkpoints must dedupe idempotency keys inside the owner boundary"
    );
    assert_eq!(
        primary_key_columns(&pool, &schema, "run_checkpoints").await,
        ["user_id", "checkpoint_id"],
        "run_checkpoints primary key must carry the owner boundary"
    );
    assert_eq!(
        primary_key_columns(&pool, &schema, "edge_pending_dispatch").await,
        [
            "user_id",
            "session_id",
            "run_id",
            "turn_chain_id",
            "request_id"
        ],
        "edge dispatch request identity must be owner/session/run/turn/request-bound at the physical key"
    );
    assert!(
        unique_key_columns(
            &pool,
            &schema,
            "edge_pending_dispatch",
            "uq_edge_dispatch_request_id"
        )
        .await
        .is_empty(),
        "edge dispatch must not keep obsolete global request_id uniqueness"
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
    assert_eq!(
        column_character_maximum_length(&pool, &schema, "agent_runs", "trigger_event_id").await,
        Some(128),
        "agent_runs.trigger_event_id must fit content-addressed event ids"
    );

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
            "agent_runs",
            "idx_agent_runs_owner_root_depth"
        )
        .await,
        ["user_id", "root_run_id", "depth", "created_at"],
        "run-tree depth scans must be owner-bound before root/depth ordering"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_runs",
            "idx_agent_runs_owner_parent_status_updated"
        )
        .await,
        ["user_id", "parent_run_id", "status", "updated_at"],
        "parent-run active child scans must be owner-bound before parent/status ordering"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_runs",
            "idx_agent_runs_owner_retry_of"
        )
        .await,
        ["user_id", "retry_of"],
        "retry lineage lookups must be owner-bound"
    );
    assert_eq!(
        index_columns(&pool, &schema, "agent_runs", "idx_agent_runs_recovery_scan").await,
        [
            "status",
            "owner_lease_expires_at",
            "updated_at",
            "user_id",
            "run_id"
        ],
        "restart recovery must use a bounded lease-aware ordered scan"
    );
    assert!(
        index_columns(&pool, &schema, "agent_runs", "idx_agent_runs_status_lease")
            .await
            .is_empty(),
        "the shorter recovery index must not duplicate the covering scan index"
    );
    assert_eq!(
        index_columns(&pool, &schema, "agent_session_execution_slots", "PRIMARY").await,
        ["user_id", "session_id"],
        "session execution slot must be a first-class unique owner/session resource"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_session_execution_slots",
            "idx_session_execution_slots_run"
        )
        .await,
        ["user_id", "run_id"],
        "slot release/cleanup must be owner/run indexed"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_session_execution_slots",
            "idx_session_execution_slots_updated"
        )
        .await,
        ["updated_at"],
        "stale slot cleanup must not require scanning the slot table"
    );
    assert!(
        index_columns(
            &pool,
            &schema,
            "agent_runs",
            "idx_agent_runs_session_updated"
        )
        .await
        .is_empty(),
        "agent_runs must not keep ownerless session recency index idx_agent_runs_session_updated"
    );
    for removed_index in [
        "idx_agent_runs_root_depth",
        "idx_agent_runs_parent",
        "idx_agent_runs_retry_of",
    ] {
        assert!(
            index_columns(&pool, &schema, "agent_runs", removed_index)
                .await
                .is_empty(),
            "agent_runs must not keep ownerless run-tree/retry index {removed_index}"
        );
    }
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "run_display_projections",
            "idx_run_display_projections_owner_session_updated"
        )
        .await,
        ["user_id", "session_id", "updated_at"],
        "run display projection session scans must stay owner-bound"
    );
    assert!(
        index_columns(
            &pool,
            &schema,
            "run_display_projections",
            "idx_run_display_projections_session_updated"
        )
        .await
        .is_empty(),
        "run_display_projections must not keep ownerless session recency index idx_run_display_projections_session_updated"
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
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_run_events",
            "idx_agent_run_events_owner_session_subject"
        )
        .await,
        [
            "user_id",
            "session_id",
            "event_type",
            "subject_run_id",
            "event_idx"
        ],
        "agent recovery must resolve selected child identities without scanning event JSON"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "agent_run_events",
            "idx_agent_run_events_interaction"
        )
        .await,
        [
            "user_id",
            "run_id",
            "interaction_request_id",
            "event_type",
            "event_idx"
        ],
        "durable interaction polling must use normalized run-scoped identity"
    );
    for removed_index in [
        "idx_agent_run_events_run_created",
        "idx_agent_run_events_session_created",
    ] {
        assert!(
            index_columns(&pool, &schema, "agent_run_events", removed_index)
                .await
                .is_empty(),
            "agent_run_events must not keep ownerless replay index {removed_index}"
        );
    }
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "run_checkpoints",
            "idx_run_checkpoints_user_run_created"
        )
        .await,
        ["user_id", "run_id", "created_at"],
        "latest checkpoint reads must use owner/run ordering"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "run_checkpoints",
            "idx_run_checkpoints_session_kind_created"
        )
        .await,
        ["user_id", "session_id", "checkpoint_kind", "created_at"],
        "checkpoint session scans must remain owner-bound"
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
    assert_eq!(
        primary_key_columns(&pool, &schema, "session_tool_output_batches").await,
        ["user_id", "session_id", "batch_id"],
        "tool output batch identity must be owner/session scoped"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_tool_output_batches",
            "idx_tool_output_batches_user_run_created"
        )
        .await,
        ["user_id", "run_id", "created_at", "batch_id"],
        "tool output batch run diagnostics must be owner-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_tool_output_batches",
            "idx_tool_output_batches_user_session_created"
        )
        .await,
        ["user_id", "session_id", "created_at", "batch_id"],
        "tool output batch session list must be owner-bound"
    );
    for removed_index in [
        "idx_tool_output_batches_session",
        "idx_tool_output_batches_run_status",
        "idx_tool_output_batches_run_created",
        "idx_tool_output_batches_session_created",
    ] {
        assert!(
            index_columns(&pool, &schema, "session_tool_output_batches", removed_index)
                .await
                .is_empty(),
            "session_tool_output_batches must not keep legacy ownerless index {removed_index}"
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
        primary_key_columns(&pool, &schema, "session_device_leases").await,
        ["user_id", "lease_id"],
        "session_device_leases primary key must carry the owner boundary"
    );
    assert_eq!(
        unique_key_columns(&pool, &schema, "session_device_leases", "uq_session_device").await,
        ["user_id", "session_id", "device_id"],
        "device leases must enforce owner/session/device uniqueness"
    );

    let lease_events = column_names(&pool, &schema, "session_device_lease_events").await;
    for expected in [
        "lease_event_id",
        "lease_id",
        "user_id",
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
    assert_eq!(
        primary_key_columns(&pool, &schema, "session_device_lease_events").await,
        ["user_id", "lease_event_id"],
        "session_device_lease_events primary key must carry the owner boundary"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_device_lease_events",
            "idx_lease_events_owner_session_device"
        )
        .await,
        ["user_id", "session_id", "device_id", "created_at"],
        "device lease event session/device lookups must stay owner-bound"
    );
    assert!(
        index_columns(
            &pool,
            &schema,
            "session_device_lease_events",
            "idx_lease_events_session_device"
        )
        .await
        .is_empty(),
        "session_device_lease_events must not keep ownerless session/device index idx_lease_events_session_device"
    );

    let transcript_pk = primary_key_columns(&pool, &schema, "session_transcript_items").await;
    assert_eq!(
        transcript_pk,
        ["user_id", "session_id", "item_seq"],
        "session_transcript_items identity must be owner/session scoped"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_transcript_items",
            "idx_transcript_owner_run_event"
        )
        .await,
        ["user_id", "run_id", "source_event_idx"],
        "transcript source event lookups must be owner-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_transcript_items",
            "idx_transcript_owner_session_source_event"
        )
        .await,
        ["user_id", "session_id", "source_event_id"],
        "transcript source-event idempotency lookups must be owner/session-bound"
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
        primary_key_columns(&pool, &schema, "transcript_pages").await,
        ["user_id", "session_id", "page_seq"],
        "transcript page identity must be owner/session scoped"
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
    assert_eq!(
        column_character_maximum_length(&pool, &schema, "ctx_snapshots", "event_id").await,
        Some(128),
        "ctx_snapshots.event_id must fit content-addressed event ids"
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
    for removed_index in [
        "idx_ctx_snapshots_session_created",
        "idx_ctx_snapshots_event_id",
    ] {
        assert!(
            index_columns(&pool, &schema, "ctx_snapshots", removed_index)
                .await
                .is_empty(),
            "ctx_snapshots must not keep ownerless context index {removed_index}"
        );
    }
    let ctx_decision_columns = column_names(&pool, &schema, "ctx_decision_audits").await;
    assert!(
        ctx_decision_columns
            .iter()
            .any(|column| column == "user_id"),
        "ctx_decision_audits must carry physical owner scope"
    );
    assert_eq!(
        column_character_maximum_length(&pool, &schema, "ctx_decision_audits", "event_id").await,
        Some(128),
        "ctx_decision_audits.event_id must fit content-addressed event ids"
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
    for removed_index in [
        "idx_ctx_decisions_session_type_created",
        "idx_ctx_decisions_event_id",
        "idx_ctx_decisions_context_capture_id",
    ] {
        assert!(
            index_columns(&pool, &schema, "ctx_decision_audits", removed_index)
                .await
                .is_empty(),
            "ctx_decision_audits must not keep ownerless decision index {removed_index}"
        );
    }
    let skill_selection_columns = column_names(&pool, &schema, "skill_selection_events").await;
    for expected in ["event_id", "session_id", "user_id", "skill_name"] {
        assert!(
            skill_selection_columns
                .iter()
                .any(|column| column == expected),
            "skill_selection_events missing {expected}"
        );
    }
    assert_eq!(
        column_nullable(&pool, &schema, "skill_selection_events", "user_id").await,
        Some(false),
        "skill_selection_events.user_id must be required because all writes are owner-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "skill_selection_events",
            "idx_skill_selection_owner_session_created"
        )
        .await,
        ["user_id", "session_id", "created_at"],
        "skill selection session scans must stay owner-bound"
    );
    assert!(
        index_columns(
            &pool,
            &schema,
            "skill_selection_events",
            "idx_skill_selection_session_created"
        )
        .await
        .is_empty(),
        "skill_selection_events must not keep ownerless session index idx_skill_selection_session_created"
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
            "prompt_request_records",
            "idx_prompt_requests_retention_ms"
        )
        .await,
        ["created_at_unix_ms", "user_id", "request_id", "session_id"],
        "prompt retention cleanup must use numeric age key to avoid MatrixOne DATETIME cast scans"
    );
    for removed_index in [
        "idx_prompt_requests_session_created",
        "idx_prompt_requests_run_created",
    ] {
        assert!(
            index_columns(&pool, &schema, "prompt_request_records", removed_index)
                .await
                .is_empty(),
            "prompt_request_records must not keep ownerless prompt index {removed_index}"
        );
    }
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
        primary_key_columns(&pool, &schema, "tool_invocation_ledger").await,
        [
            "user_id",
            "session_id",
            "run_id",
            "turn_chain_id",
            "invocation_id"
        ],
        "tool invocation durability must use the complete owner/run/turn/invocation identity"
    );
    assert!(
        column_names(&pool, &schema, "tool_invocation_ledger")
            .await
            .iter()
            .any(|column| column == "outcome_json"),
        "tool invocation terminal state and replay outcome must share one durable row"
    );
    assert!(
        column_names(&pool, &schema, "tool_invocation_ledger")
            .await
            .iter()
            .any(|column| column == "decision_json"),
        "prepared invocation resume requires the complete frozen decision, not only its hash"
    );
    let invocation_columns = column_names(&pool, &schema, "tool_invocation_ledger").await;
    assert!(
        invocation_columns
            .iter()
            .any(|column| column == "identity_key"),
        "bounded archive lookup requires a deterministic invocation identity key"
    );
    assert!(
        invocation_columns
            .iter()
            .any(|column| column == "dispatch_owner"),
        "provider dispatch completion must be fenced by an explicit worker owner"
    );
    assert!(
        invocation_columns
            .iter()
            .any(|column| column == "dispatch_lease_expires_at"),
        "abandoned provider dispatches require a durable liveness deadline"
    );
    assert!(
        invocation_columns
            .iter()
            .any(|column| column == "completion_source_json"),
        "non-dispatched cache completion requires durable, typed provenance"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "tool_invocation_ledger",
            "idx_tool_invocation_run_compaction"
        )
        .await,
        ["user_id", "session_id", "run_id", "state", "identity_key"],
        "terminal-run compaction must remain owner/run scoped and bounded"
    );
    assert_eq!(
        primary_key_columns(&pool, &schema, "tool_invocation_archive_chunks").await,
        ["user_id", "session_id", "run_id", "chunk_index"],
        "invocation archive chunks must remain owner/session/run scoped"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "tool_invocation_archive_chunks",
            "idx_tool_invocation_archive_lookup"
        )
        .await,
        [
            "user_id",
            "session_id",
            "run_id",
            "first_identity_key",
            "last_identity_key"
        ],
        "archived invocation replay must resolve one bounded identity-key range"
    );
    assert_eq!(
        primary_key_columns(&pool, &schema, "semantic_read_observations").await,
        ["user_id", "session_id", "key_id"],
        "semantic observations must be owner/session scoped and content addressed"
    );
    assert_eq!(
        primary_key_columns(&pool, &schema, "semantic_read_observation_budgets").await,
        ["user_id", "session_id"],
        "semantic cache aggregate accounting must use its own owner/session lock instead of the session authority row"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "semantic_read_observations",
            "idx_semantic_read_observations_session_state_access"
        )
        .await,
        ["user_id", "session_id", "state", "last_accessed_at"],
        "capacity and deterministic LRU operations require one session/state/access index"
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
    assert_eq!(
        primary_key_columns(&pool, &schema, "session_state_revisions").await,
        ["user_id", "session_id"],
        "session_state_revisions primary key must carry the owner boundary"
    );
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
    for removed_index in ["idx_ctx_manifest_session_turn", "idx_ctx_manifest_run"] {
        assert!(
            index_columns(&pool, &schema, "context_manifests", removed_index)
                .await
                .is_empty(),
            "context_manifests must not keep ownerless manifest index {removed_index}"
        );
    }

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
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "context_manifest_items",
            "idx_manifest_items_manifest_zone"
        )
        .await,
        ["manifest_id", "zone", "included"],
        "manifest items must be indexed by their parent manifest boundary, not bare session"
    );
    assert!(
        index_columns(
            &pool,
            &schema,
            "context_manifest_items",
            "idx_manifest_items_session_zone"
        )
        .await
        .is_empty(),
        "context_manifest_items must not keep ownerless session-zone index idx_manifest_items_session_zone"
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

    let session_todos = column_names(&pool, &schema, "session_todos").await;
    for expected in ["user_id", "session_id", "todo_id", "ordinal", "status"] {
        assert!(
            session_todos.iter().any(|column| column == expected),
            "session_todos missing {expected}"
        );
    }
    assert_eq!(
        primary_key_columns(&pool, &schema, "session_todos").await,
        ["user_id", "session_id", "todo_id"],
        "session_todos must be owner-bound at the uniqueness boundary"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_todos",
            "idx_session_todos_owner_session_ordinal"
        )
        .await,
        ["user_id", "session_id", "ordinal"],
        "session_todos load path must be owner/session ordered"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_todos",
            "idx_session_todos_owner_session_status_updated"
        )
        .await,
        ["user_id", "session_id", "status", "updated_at"],
        "session_todos active path must be owner/session/status scoped"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_todos",
            "idx_session_todos_user_status_updated"
        )
        .await,
        ["user_id", "status", "updated_at"],
        "cross-session user todo views must stay owner-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_todos",
            "idx_session_todos_status_updated_owner"
        )
        .await,
        ["status", "updated_at", "user_id", "session_id", "todo_id"],
        "global lifecycle sweeps must not full-scan every user's task history"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_todos",
            "idx_session_todos_archived_gc_owner"
        )
        .await,
        ["status", "archived_at", "user_id", "session_id", "todo_id"],
        "archived task GC must have a bounded global candidate path"
    );
    assert!(
        index_columns(
            &pool,
            &schema,
            "session_todos",
            "idx_session_todos_session_status_updated"
        )
        .await
        .is_empty(),
        "session_todos must not keep the obsolete session-only status index"
    );
    assert_eq!(
        primary_key_columns(&pool, &schema, "session_todo_idempotency").await,
        ["user_id", "session_id", "action", "idempotency_key"],
        "todo idempotency ledger must dedupe by owner/session/action/key"
    );

    let session_checkpoints = column_names(&pool, &schema, "session_checkpoints").await;
    for expected in ["user_id", "session_id", "number", "turn", "state_json"] {
        assert!(
            session_checkpoints.iter().any(|column| column == expected),
            "session_checkpoints missing {expected}"
        );
    }
    assert_eq!(
        primary_key_columns(&pool, &schema, "session_checkpoints").await,
        ["user_id", "session_id", "checkpoint_id"],
        "session checkpoint physical identity must stay inside owner/session boundary"
    );
    assert_eq!(
        unique_key_columns(
            &pool,
            &schema,
            "session_checkpoints",
            "uq_session_checkpoints_owner_number"
        )
        .await,
        ["user_id", "session_id", "number"],
        "session checkpoint numbers must be unique inside the owner/session boundary"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_checkpoints",
            "idx_ckpt_owner_session_turn"
        )
        .await,
        ["user_id", "session_id", "turn"],
        "session checkpoint turn lookups must be owner-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_checkpoints",
            "idx_ckpt_user_created"
        )
        .await,
        ["user_id", "created_at"],
        "user checkpoint recency scans must stay owner-bound"
    );
    for removed_index in ["idx_ckpt_session_number", "idx_ckpt_session_turn"] {
        assert!(
            index_columns(&pool, &schema, "session_checkpoints", removed_index)
                .await
                .is_empty(),
            "session_checkpoints must not keep ownerless checkpoint index {removed_index}"
        );
    }

    let plans = column_names(&pool, &schema, "plans").await;
    for expected in ["plan_id", "user_id", "session_id", "updated_at"] {
        assert!(
            plans.iter().any(|column| column == expected),
            "plans missing {expected}"
        );
    }
    assert_eq!(
        primary_key_columns(&pool, &schema, "plans").await,
        ["user_id", "plan_id"],
        "plans primary key must allow the same plan_id under different owners"
    );
    assert_eq!(
        index_columns(&pool, &schema, "plans", "idx_plans_owner_session_updated").await,
        ["user_id", "session_id", "updated_at"],
        "plan session list and lifecycle paths must be owner/session scoped"
    );
    assert!(
        index_columns(&pool, &schema, "plans", "idx_plans_session")
            .await
            .is_empty(),
        "plans must not keep ownerless session index idx_plans_session"
    );

    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "plan_step_runs",
            "idx_step_runs_plan_started"
        )
        .await,
        ["user_id", "plan_id", "started_at"],
        "step-run history must be owner-bound before plan ordering"
    );
    assert!(
        index_columns(&pool, &schema, "plan_step_runs", "idx_step_runs_session")
            .await
            .is_empty(),
        "plan_step_runs must not keep an ownerless session scan index"
    );

    let history_chunks = column_names(&pool, &schema, "session_history_chunks").await;
    for expected in [
        "user_id",
        "session_id",
        "source_session_id",
        "seq_start",
        "seq_end",
    ] {
        assert!(
            history_chunks.iter().any(|column| column == expected),
            "session_history_chunks missing {expected}"
        );
    }
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_history_chunks",
            "idx_history_owner_session_seq"
        )
        .await,
        ["user_id", "session_id", "seq_start", "seq_end"],
        "history chunk seq lookup must stay owner/session-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_history_chunks",
            "idx_history_owner_source_session"
        )
        .await,
        ["user_id", "source_session_id", "chunk_type", "created_at"],
        "history source-session lookup must stay owner-bound"
    );
    for removed_index in ["idx_history_session_seq", "idx_history_source_session"] {
        assert!(
            index_columns(&pool, &schema, "session_history_chunks", removed_index)
                .await
                .is_empty(),
            "session_history_chunks must not keep ownerless history index {removed_index}"
        );
    }

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
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_state_items",
            "idx_state_owner_origin_session"
        )
        .await,
        ["user_id", "origin_session_id", "category", "status"],
        "origin-session cleanup must be owner-bound"
    );
    assert!(
        index_columns(
            &pool,
            &schema,
            "session_state_items",
            "idx_state_session_category"
        )
        .await
        .is_empty(),
        "session_state_items must not keep ownerless category/status index idx_state_session_category"
    );
    assert!(
        index_columns(
            &pool,
            &schema,
            "session_state_items",
            "idx_state_origin_session"
        )
        .await
        .is_empty(),
        "session_state_items must not keep ownerless origin-session index idx_state_origin_session"
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
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_state_item_events",
            "idx_state_events_owner_session_created"
        )
        .await,
        ["user_id", "session_id", "created_at", "event_id"],
        "state event history scans must stay owner-bound"
    );
    assert!(
        index_columns(
            &pool,
            &schema,
            "session_state_item_events",
            "idx_state_events_session_created"
        )
        .await
        .is_empty(),
        "session_state_item_events must not keep ownerless session index idx_state_events_session_created"
    );

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
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_delegations",
            "idx_delegations_owner_root_depth"
        )
        .await,
        ["user_id", "root_run_id", "depth", "created_at"],
        "delegation root-tree scans must stay owner-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_delegations",
            "idx_delegations_owner_parent_status_updated"
        )
        .await,
        ["user_id", "parent_run_id", "status", "updated_at"],
        "delegation parent/status scans must stay owner-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_delegations",
            "idx_delegations_owner_session_status"
        )
        .await,
        ["user_id", "session_id", "status", "updated_at"],
        "delegation session/status scans must stay owner-bound"
    );
    for removed_index in [
        "idx_delegations_root_depth",
        "idx_delegations_parent",
        "idx_delegations_session_status",
    ] {
        assert!(
            index_columns(&pool, &schema, "session_delegations", removed_index)
                .await
                .is_empty(),
            "session_delegations must not keep ownerless delegation index {removed_index}"
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
        primary_key_columns(&pool, &schema, "session_artifacts").await,
        ["user_id", "session_id", "artifact_id"],
        "session artifact identity must be owner/session scoped"
    );
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
            "idx_session_artifacts_owner_source_order"
        )
        .await,
        [
            "user_id",
            "session_id",
            "source",
            "created_at",
            "artifact_id"
        ],
        "artifact source reads must stay owner/session scoped"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_artifacts",
            "idx_artifacts_root_scope"
        )
        .await,
        [
            "user_id",
            "root_run_id",
            "access_scope",
            "status",
            "updated_at",
            "artifact_id"
        ],
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
        unique_key_columns(
            &pool,
            &schema,
            "session_artifacts_grants",
            "uq_artifacts_grant_target"
        )
        .await,
        [
            "user_id",
            "session_id",
            "artifact_id",
            "grant_scope",
            "target_run_id",
            "target_delegation_id"
        ],
        "artifact grant idempotency must include owner/session"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_artifacts_grants",
            "idx_artifacts_grants_target"
        )
        .await,
        [
            "user_id",
            "session_id",
            "target_run_id",
            "artifact_id",
            "expires_at"
        ],
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
        [
            "user_id",
            "session_id",
            "target_delegation_id",
            "artifact_id",
            "expires_at"
        ],
        "delegation artifact grants must use a target/artifact/expiry index"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn phase5_personal_skill_schema_contract() {
    let pool = common::setup_pool().await;
    let schema = current_schema(&pool).await;

    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "skills_registry",
            "idx_skill_active_name_ver"
        )
        .await,
        ["is_active", "skill_name", "version"],
        "skills registry seek pagination must be backed by the product schema, not test-only DDL"
    );

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
        "owner_user_id",
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
    assert_eq!(
        column_nullable(&pool, &schema, "user_skill_evaluations", "owner_user_id").await,
        Some(false),
        "skill evaluations must be owner-scoped at write time"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "user_skill_evaluations",
            "idx_user_skill_eval_owner_source_created"
        )
        .await,
        ["owner_user_id", "source_id", "created_at"],
        "skill evaluation source history must use owner/source ordering"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "user_skill_evaluations",
            "idx_user_skill_eval_owner_version_created"
        )
        .await,
        ["owner_user_id", "version_id", "created_at"],
        "skill evaluation version history must use owner/version ordering"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "user_skill_evaluations",
            "idx_user_skill_eval_owner_run"
        )
        .await,
        ["owner_user_id", "run_id"],
        "session lifecycle deletes must match skill evaluations by owner/run"
    );
    for removed_index in [
        "idx_user_skill_eval_source_created",
        "idx_user_skill_eval_version_created",
        "idx_user_skill_eval_run",
    ] {
        assert!(
            index_columns(&pool, &schema, "user_skill_evaluations", removed_index)
                .await
                .is_empty(),
            "user_skill_evaluations must not keep obsolete ownerless index {removed_index}"
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
        [
            "status",
            "retention_until",
            "retention_policy",
            "user_id",
            "session_id",
            "artifact_id",
        ],
        "retention GC must filter by status before due-date range and select enough identity columns for scoped updates"
    );
    assert_eq!(
        primary_key_columns(&pool, &schema, "session_artifact_references").await,
        [
            "user_id",
            "session_id",
            "artifact_id",
            "reference_kind",
            "reference_id",
        ],
        "artifact reachability must be an owner-scoped durable edge"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_artifact_references",
            "idx_artifact_references_owner_reference"
        )
        .await,
        [
            "user_id",
            "session_id",
            "reference_kind",
            "reference_id",
            "artifact_id",
        ],
        "artifact owners must resolve references without scanning payloads"
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
        primary_key_columns(&pool, &schema, "session_tool_outputs").await,
        ["user_id", "session_id", "output_id"],
        "tool output row identity must be owner/session scoped"
    );
    assert_eq!(
        unique_key_columns(
            &pool,
            &schema,
            "session_tool_outputs",
            "uq_tool_outputs_batch_idx"
        )
        .await,
        ["user_id", "session_id", "batch_id", "output_idx"],
        "tool output batch ordering must be owner/session scoped"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_tool_outputs",
            "idx_tool_outputs_user_run_created"
        )
        .await,
        ["user_id", "run_id", "created_at", "output_id"],
        "tool output run diagnostics must be owner-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_tool_outputs",
            "idx_tool_outputs_user_session_created"
        )
        .await,
        ["user_id", "session_id", "created_at", "output_id"],
        "tool output session list must be owner-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_tool_outputs",
            "idx_tool_outputs_parent"
        )
        .await,
        ["user_id", "parent_output_id"],
        "tool output parent lookup must stay owner-bound"
    );
    assert_eq!(
        index_columns(
            &pool,
            &schema,
            "session_tool_outputs",
            "idx_tool_outputs_artifact_ref"
        )
        .await,
        ["user_id", "artifact_ref"],
        "tool output artifact refs must stay inside the owner boundary"
    );
    for removed_index in [
        "idx_tool_outputs_tool_created",
        "idx_tool_outputs_session_tool_score",
        "idx_tool_outputs_status_created",
        "idx_tool_outputs_batch",
        "idx_tool_outputs_run_created",
        "idx_tool_outputs_session_created",
    ] {
        assert!(
            index_columns(&pool, &schema, "session_tool_outputs", removed_index)
                .await
                .is_empty(),
            "session_tool_outputs must not keep legacy ownerless index {removed_index}"
        );
    }

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
          'clippy', 'pg_schema_structurize', 'slow_query_analyzer', 'curl',
          'git', 'docker_logs', 'kubectl', 'python_stdout', 'npm_build', 'csv_head',
          'json_preview', 'markdown_preview', 'list_dir', 'glob', 'grep', 'symbols',
          'task_board', 'agent', 'agent_fanout', 'session', 'web_fetch', 'tool_search',
          'memory', 'mo_query')
         AND status = 'active'",
    )
    .fetch_all(pool.get())
    .await
    .expect("load required preview templates");
    assert!(
        required_templates.len() >= 30,
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
