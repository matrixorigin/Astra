mod common;

use sqlx::Row;

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

    let run_counters = column_names(&pool, &schema, "run_counters").await;
    for expected in [
        "next_event_idx",
        "owner_pod_id",
        "owner_lease_expires_at",
        "run_generation",
    ] {
        assert!(
            run_counters.iter().any(|column| column == expected),
            "run_counters missing {expected}"
        );
    }

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

    let reasons =
        sqlx::query("SELECT reason FROM context_manifest_reason_types WHERE is_active = 1")
            .fetch_all(pool.get())
            .await
            .expect("load context manifest reasons")
            .into_iter()
            .map(|row| row.try_get::<String, _>("reason").unwrap())
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

    let items = column_names(&pool, &schema, "context_manifest_items").await;
    for expected in ["render_mode", "included", "raw_ref", "budget_tokens"] {
        assert!(
            items.iter().any(|column| column == expected),
            "context_manifest_items missing {expected}"
        );
    }

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
        ["session_id", "scope", "category", "item_key"],
        "state current projection must upsert by semantic key"
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
            "idx_artifacts_root_scope"
        )
        .await,
        ["root_run_id", "access_scope", "status", "updated_at"],
        "artifact same-root ACL must use root/scope index"
    );

    let grants = column_names(&pool, &schema, "session_artifacts_grants").await;
    assert!(
        !table_exists(&pool, &schema, "session_artifact_grants").await,
        "deprecated singular session_artifact_grants table must not exist"
    );
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

    let check_text = include_str!("../src/storage.rs");
    for expected in [
        "session",
        "user",
        "project",
        "workspace",
        "bubble_up",
        "apply_suggestion",
        "activate",
        "delegation_direct",
        "same_root_tree",
        "delegation",
    ] {
        assert!(
            check_text.contains(expected),
            "Phase 4 CHECK constraints missing {expected}: {check_text}"
        );
    }
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

    let storage_source = include_str!("../src/storage.rs");
    assert!(
        storage_source.contains("skill_md_v1")
            && storage_source.contains("draft")
            && storage_source.contains("published")
            && storage_source.contains("superseded")
            && storage_source.contains("quarantined"),
        "Phase 5 schema must document normalize_version and version.status enum"
    );
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
    let storage_source = include_str!("../src/storage.rs");
    assert!(
        storage_source.contains("chk_session_artifacts_status")
            && storage_source.contains("expiring")
            && storage_source.contains("expired"),
        "session_artifacts status enum must be constrained in schema source"
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

    let runner_columns = column_names(&pool, &schema, "tool_runner_registry").await;
    for expected in [
        "tool_name",
        "preview_template_version",
        "normalize_version",
        "default_raw_ref_scheme",
    ] {
        assert!(
            runner_columns.iter().any(|column| column == expected),
            "tool_runner_registry missing {expected}"
        );
    }

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
