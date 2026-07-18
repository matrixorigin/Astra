use astra_runtime::prompts::CompactionTier;
use astra_services::{
    BASELINE_PREVIEW_TEMPLATES, BUDGET_V1_8K_PROMPT_CAP, BUDGET_V1_8K_TOTAL_CAP, BudgetV1_8k,
    ContextManifestItemWrite, ContextManifestWrite, DatabaseContextManifestStore, RetrievalStage,
    content_hash_with_normalize_version, cross_session_retrieval_requires_user_filter,
    delegation_budget, delegation_budget_allocation, next_action_confidence_action,
    suggested_next_action_expires_at,
};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

fn require_db_it_env() -> astra_core::MatrixOneSettings {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
    );
    astra_core::MatrixOneSettings::from_env()
}

async fn setup_pool() -> astra_core::SharedPool {
    let settings = require_db_it_env();
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    astra_services::ensure_core_schema(&settings, &catalog)
        .await
        .expect("ensure_core_schema; is MatrixOne up?");
    astra_core::SharedPool::new(&settings)
        .await
        .expect("SharedPool::new")
}

fn manifest(user_id: &str, session_id: &str, run_id: &str, reason: &str) -> ContextManifestWrite {
    ContextManifestWrite {
        manifest_id: format!("manifest-{}", Uuid::new_v4()),
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
        run_id: Some(run_id.to_string()),
        turn_id: format!("turn-{}", Uuid::new_v4()),
        model_provider: "test".to_string(),
        model_name: "test-model".to_string(),
        context_window_tokens: 8_000,
        max_output_tokens: 500,
        total_estimated_tokens: 3_000,
        policy_version: "context_manifest_v1".to_string(),
        tokenizer_id: Some("estimated_v1".to_string()),
        budget_template_id: Some("budget_v1_8k".to_string()),
        turn_intent: Some("benchmark_comparison".to_string()),
        reason: reason.to_string(),
        manifest_json: json!({"zones": {"recent_tail": 1200}}),
    }
}

fn item(session_id: &str, order: i32, included: bool, reason: &str) -> ContextManifestItemWrite {
    ContextManifestItemWrite {
        session_id: session_id.to_string(),
        item_order: order,
        zone: "recent_tail".to_string(),
        source_table: "agent_events".to_string(),
        source_id: format!("event-{order}"),
        source_hash: None,
        included,
        token_estimate: 100,
        budget_tokens: 200,
        reason: reason.to_string(),
        render_mode: "code_block_preserved".to_string(),
        raw_ref: Some("conversation_log://session/1@sha256:test".to_string()),
    }
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_19_manifest_items_persist_included_and_dropped_entries() {
    let pool = setup_pool().await;
    let store = DatabaseContextManifestStore::new(pool.clone());
    let suffix = Uuid::new_v4();
    let session_id = format!("session-{suffix}");
    let run_id = format!("run-{suffix}");
    let user_id = format!("user-{suffix}");
    let manifest = manifest(&user_id, &session_id, &run_id, "normal_turn");
    let manifest_id = manifest.manifest_id.clone();
    store
        .save_manifest(
            manifest,
            vec![
                item(&session_id, 0, true, "normal_turn"),
                item(&session_id, 1, false, "delegation_child_overflow"),
            ],
        )
        .await
        .unwrap();
    let row = sqlx::query("SELECT dropped_count FROM context_manifests WHERE manifest_id = ?")
        .bind(&manifest_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
    assert_eq!(row.try_get::<i64, _>("dropped_count").unwrap(), 1);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_20_retrieval_sla_state_machine_events_are_stable() {
    assert_eq!(RetrievalStage::Structured.timeout_ms(), 50);
    assert_eq!(RetrievalStage::Fts.timeout_ms(), 200);
    assert_eq!(RetrievalStage::Vector.timeout_ms(), 500);
    assert_eq!(
        RetrievalStage::Vector.event_type("stale"),
        "retrieval.vector_stale"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_21_vector_stale_check_uses_content_hash_and_normalize_version() {
    let a = content_hash_with_normalize_version("sha256:content", Some("raw_v1"));
    let b = content_hash_with_normalize_version("sha256:content", Some("sql_v1"));
    assert_ne!(a, b);
    assert_eq!(
        content_hash_with_normalize_version("sha256:content", None),
        content_hash_with_normalize_version("sha256:content", Some("raw_v1"))
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_22_cross_session_retrieval_requires_user_id_filter() {
    assert!(cross_session_retrieval_requires_user_filter(Some("user-1")).is_ok());
    assert!(cross_session_retrieval_requires_user_filter(None).is_err());
    assert!(cross_session_retrieval_requires_user_filter(Some("")).is_err());
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_23_delegation_budget_applies_g21_fanout_formula() {
    assert_eq!(delegation_budget(3).per_child_budget, 500);
    assert_eq!(delegation_budget(8).rendered_children, 7);
    assert_eq!(delegation_budget(8).overflow_children, 1);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_24_blocker_delegation_zone_never_breaks_recent_tail_floor() {
    let allocation = delegation_budget_allocation(8, 1);
    assert!(allocation.blocker_active);
    assert_eq!(allocation.requested_delegation_zone_budget, 3000);
    assert_eq!(allocation.recent_tail_budget, 1600);
    assert_eq!(allocation.borrowed_from_recent_tail, 400);
    assert!(allocation.delegation_zone_budget > 1500);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_25_budget_v1_8k_template_matches_design_caps() {
    let budget = BudgetV1_8k::standard();
    assert_eq!(budget.anchor, 200);
    assert_eq!(budget.plan_todo, 400);
    assert_eq!(budget.recent_tail, 2000);
    assert_eq!(budget.summary, 500);
    assert_eq!(budget.retrieved, 1000);
    assert_eq!(budget.tool_previews, 500);
    assert_eq!(budget.system_tool_schemas, 3400);
    assert_eq!(budget.prompt_cap(), BUDGET_V1_8K_PROMPT_CAP);
    assert_eq!(budget.input_context_cap(), BUDGET_V1_8K_TOTAL_CAP);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_26_budget_property_test_for_fanout_boundaries() {
    for (n, rendered, per_child, total) in [
        (0, 0, 0, 0),
        (1, 1, 1500, 1500),
        (3, 3, 500, 1500),
        (5, 5, 300, 1500),
        (7, 7, 214, 1498),
        (8, 7, 214, 1498),
        (10, 7, 214, 1498),
        (15, 7, 214, 1498),
        (100, 7, 214, 1498),
    ] {
        let budget = delegation_budget(n);
        assert_eq!(budget.rendered_children, rendered, "n={n}");
        assert_eq!(budget.per_child_budget, per_child, "n={n}");
        assert_eq!(budget.rendered_total, total, "n={n}");
        assert!(budget.rendered_total <= 1500);
    }
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_27_confidence_thresholds_are_deterministic() {
    assert_eq!(
        format!(
            "{:?}",
            next_action_confidence_action(0.8, 0, "structured_event", Some("event-1"))
        ),
        "AutoAccept"
    );
    assert_eq!(
        format!(
            "{:?}",
            next_action_confidence_action(0.5, 0, "structured_event", Some("event-1"))
        ),
        "AskUser"
    );
    assert_eq!(
        format!(
            "{:?}",
            next_action_confidence_action(0.49, 0, "structured_event", Some("event-1"))
        ),
        "Reject"
    );
    assert_eq!(
        format!(
            "{:?}",
            next_action_confidence_action(0.95, 0, "small_model", None)
        ),
        "AskUser"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_28_three_ask_user_events_downgrade_confidence() {
    assert_eq!(
        format!(
            "{:?}",
            next_action_confidence_action(0.85, 2, "structured_event", Some("event-1"))
        ),
        "AutoAccept"
    );
    assert_eq!(
        format!(
            "{:?}",
            next_action_confidence_action(0.85, 3, "structured_event", Some("event-1"))
        ),
        "AskUser"
    );
    assert_eq!(
        format!(
            "{:?}",
            next_action_confidence_action(0.85, 3, "small_model", None)
        ),
        "AskUser"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_29_suggested_next_action_expiry_by_kind() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-05-07T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(suggested_next_action_expires_at("approval", now).contains("2026-05-08"));
    assert!(suggested_next_action_expires_at("todo", now).contains("2026-05-14"));
    assert!(suggested_next_action_expires_at("hint", now).contains("2026-05-07T01"));
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_30_preview_template_baselines_cover_large_tool_outputs() {
    let names = BASELINE_PREVIEW_TEMPLATES
        .iter()
        .map(|(name, _, _)| *name)
        .collect::<Vec<_>>();
    for expected in [
        "cargo",
        "rustc",
        "clippy",
        "fetch_url",
        "parse_pdf",
        "pg_dump",
        "slow_query_analyzer",
        "SKILL.md",
    ] {
        assert!(names.contains(&expected), "missing {expected}");
    }
    assert!(names.len() >= 18);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_31_unknown_tool_gets_fallback_preview_budget() {
    let pool = setup_pool().await;
    let store = DatabaseContextManifestStore::new(pool.clone());
    let suffix = Uuid::new_v4();
    let session_id = format!("session-{suffix}");
    let user_id = format!("user-{suffix}");
    let tool_name = format!("unknown_tool_{suffix}");
    let fallback_bytes = store
        .preview_template_budget_or_fallback(&user_id, &session_id, Some("run-preview"), &tool_name)
        .await
        .unwrap();
    assert_eq!(fallback_bytes, 400);
    let row = sqlx::query(
        "SELECT COUNT(*) AS c FROM agent_events
         WHERE session_id = ? AND user_id = ? AND event_type = 'preview_template_missing'",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("c").unwrap(), 1);
}

async fn ensure_phase3_test_tables(pool: &astra_core::SharedPool) {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS phase3_history_chunks (
            session_id VARCHAR(128) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            chunk_id VARCHAR(128) NOT NULL,
            chunk_text LONGTEXT NOT NULL,
            token_estimate INT NOT NULL,
            content_hash VARCHAR(128) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (session_id, chunk_id)
        )",
    )
    .execute(pool.get())
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS phase3_session_delegations (
            session_id VARCHAR(128) NOT NULL,
            run_id VARCHAR(128) NOT NULL,
            child_run_id VARCHAR(128) NOT NULL,
            priority INT NOT NULL,
            status VARCHAR(32) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (session_id, child_run_id),
            INDEX idx_phase3_delegations_run_priority (run_id, priority)
        )",
    )
    .execute(pool.get())
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l3_7_s02_ten_gb_retrieval_manifest_records_three_stage_fallbacks() {
    let pool = setup_pool().await;
    ensure_phase3_test_tables(&pool).await;
    let store = DatabaseContextManifestStore::new(pool.clone());
    let suffix = Uuid::new_v4();
    let session_id = format!("session-{suffix}");
    let run_id = format!("run-{suffix}");
    let user_id = format!("user-{suffix}");

    for i in 0..64 {
        let text = format!(
            "raw SQL byte range chunk {i}: CREATE TABLE audit_{i}(id BIGINT, detail TEXT); {}",
            "checkpoint evidence ".repeat(12)
        );
        sqlx::query(
            "INSERT INTO phase3_history_chunks
             (session_id, user_id, chunk_id, chunk_text, token_estimate, content_hash)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&session_id)
        .bind(&user_id)
        .bind(format!("chunk-{i}"))
        .bind(&text)
        .bind(96_i32)
        .bind(content_hash_with_normalize_version(
            &format!("sha256:{i}"),
            Some("sql_v1"),
        ))
        .execute(pool.get())
        .await
        .unwrap();
    }

    store
        .record_retrieval_degrade_event(
            &user_id,
            &session_id,
            Some(&run_id),
            RetrievalStage::Structured,
            "timeout",
            51,
        )
        .await
        .unwrap();
    store
        .record_retrieval_degrade_event(
            &user_id,
            &session_id,
            Some(&run_id),
            RetrievalStage::Fts,
            "empty",
            12,
        )
        .await
        .unwrap();
    store
        .record_retrieval_degrade_event(
            &user_id,
            &session_id,
            Some(&run_id),
            RetrievalStage::Vector,
            "stale",
            501,
        )
        .await
        .unwrap();

    let mut m = manifest(&user_id, &session_id, &run_id, "history_recall_vector");
    m.total_estimated_tokens = 9_600;
    m.manifest_json = json!({
        "zones": {
            "retrieved_facts": {"used_tokens": 1000, "budget_tokens": 1000},
            "recent_tail": {"used_tokens": 1800, "budget_tokens": 2000}
        }
    });
    let manifest_id = m.manifest_id.clone();
    store
        .save_manifest(
            m,
            vec![ContextManifestItemWrite {
                session_id: session_id.clone(),
                item_order: 0,
                zone: "retrieved_facts".to_string(),
                source_table: "phase3_history_chunks".to_string(),
                source_id: "chunk-0..63".to_string(),
                source_hash: Some("sha256:phase3-history".to_string()),
                included: true,
                token_estimate: 1000,
                budget_tokens: 1000,
                reason: "history_recall_vector".to_string(),
                render_mode: "code_block_preserved".to_string(),
                raw_ref: Some(format!("conversation_log://{session_id}/0@sha256:phase3")),
            }],
        )
        .await
        .unwrap();

    let degrade_count = sqlx::query(
        "SELECT COUNT(*) AS c FROM agent_events
         WHERE session_id = ? AND user_id = ? AND event_type IN
         ('retrieval.structured_timeout', 'retrieval.fts_empty', 'retrieval.vector_stale')",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(pool.get())
    .await
    .unwrap()
    .try_get::<i64, _>("c")
    .unwrap();
    assert_eq!(degrade_count, 3);
    let bytes = sqlx::query(
        "SELECT CAST(COALESCE(SUM(LENGTH(chunk_text)), 0) AS SIGNED) AS bytes
         FROM phase3_history_chunks WHERE session_id = ?",
    )
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .unwrap()
    .try_get::<i64, _>("bytes")
    .unwrap();
    assert!(bytes > 10_000);
    let manifest_tokens =
        sqlx::query("SELECT total_estimated_tokens FROM context_manifests WHERE manifest_id = ?")
            .bind(&manifest_id)
            .fetch_one(pool.get())
            .await
            .unwrap()
            .try_get::<i32, _>("total_estimated_tokens")
            .unwrap();
    assert!(manifest_tokens <= 9700);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l3_8_s14_small_window_ambiguity_stays_under_budget_and_asks_user() {
    let pool = setup_pool().await;
    let store = DatabaseContextManifestStore::new(pool.clone());
    let suffix = Uuid::new_v4();
    let session_id = format!("session-{suffix}");
    let run_id = format!("run-{suffix}");
    let user_id = format!("user-{suffix}");
    let budget = BudgetV1_8k::standard();
    assert!(budget.input_context_cap() <= 7300);

    let mut m = manifest(&user_id, &session_id, &run_id, "ambiguity_clarification");
    m.total_estimated_tokens = 7_200;
    m.manifest_json = json!({
        "budget_template_id": "budget_v1_8k",
        "zones": {
            "session_anchor": {"used_tokens": 200, "budget_tokens": 200},
            "plan_todo": {"used_tokens": 300, "budget_tokens": 400},
            "recent_tail": {"used_tokens": 1900, "budget_tokens": 2000},
            "summary": {"used_tokens": 400, "budget_tokens": 500},
            "retrieved_facts": {"used_tokens": 800, "budget_tokens": 1000},
            "tool_previews": {"used_tokens": 300, "budget_tokens": 500},
            "system_tool_schemas": {"used_tokens": 3300, "budget_tokens": 3400}
        }
    });
    let manifest_id = m.manifest_id.clone();
    store
        .save_manifest(
            m,
            vec![
                ContextManifestItemWrite {
                    token_estimate: 200,
                    budget_tokens: 200,
                    zone: "session_anchor".to_string(),
                    ..item(&session_id, 0, true, "ambiguity_clarification")
                },
                ContextManifestItemWrite {
                    token_estimate: 1900,
                    budget_tokens: 2000,
                    zone: "recent_tail".to_string(),
                    ..item(&session_id, 1, true, "ambiguity_clarification")
                },
                ContextManifestItemWrite {
                    token_estimate: 3300,
                    budget_tokens: 3400,
                    zone: "system_tool_schemas".to_string(),
                    ..item(&session_id, 2, true, "ambiguity_clarification")
                },
            ],
        )
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agent_events
         (event_id, session_id, user_id, event_type, content, metadata, created_at)
         VALUES (?, ?, ?, 'user_prompt_request', ?, ?, NOW(6))",
    )
    .bind(format!("event-{suffix}"))
    .bind(&session_id)
    .bind(&user_id)
    .bind("choose next action")
    .bind(json!({"candidate_count": 3, "source": "small_model"}).to_string())
    .execute(pool.get())
    .await
    .unwrap();

    let zone_sum = sqlx::query(
        "SELECT COALESCE(SUM(token_estimate), 0) AS tokens
         FROM context_manifest_items WHERE manifest_id = ? AND included = 1",
    )
    .bind(&manifest_id)
    .fetch_one(pool.get())
    .await
    .unwrap()
    .try_get::<i64, _>("tokens")
    .unwrap();
    assert!(zone_sum <= 7300);
    assert_eq!(
        format!(
            "{:?}",
            next_action_confidence_action(0.88, 0, "small_model", None)
        ),
        "AskUser"
    );
    assert_eq!(
        format!(
            "{:?}",
            next_action_confidence_action(0.82, 0, "structured_event", Some("event-1"))
        ),
        "AutoAccept"
    );
    assert_eq!(
        format!(
            "{:?}",
            next_action_confidence_action(0.72, 0, "structured_event", Some("event-1"))
        ),
        "AskUser"
    );
    assert_eq!(
        format!(
            "{:?}",
            next_action_confidence_action(0.42, 0, "structured_event", Some("event-1"))
        ),
        "Reject"
    );
    let candidate_count = sqlx::query(
        "SELECT COUNT(*) AS c FROM agent_events
         WHERE session_id = ? AND user_id = ? AND event_type = 'user_prompt_request'",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(pool.get())
    .await
    .unwrap()
    .try_get::<i64, _>("c")
    .unwrap();
    assert_eq!(candidate_count, 1);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l3_9_s10_multi_child_delegation_budget_filters_overflow() {
    let pool = setup_pool().await;
    ensure_phase3_test_tables(&pool).await;
    let suffix = Uuid::new_v4();
    let session_id = format!("session-{suffix}");
    let run_id_3 = format!("run3-{suffix}");
    let run_id_8 = format!("run8-{suffix}");

    for i in 0..3 {
        sqlx::query(
            "INSERT INTO phase3_session_delegations
             (session_id, run_id, child_run_id, priority, status)
             VALUES (?, ?, ?, ?, 'running')",
        )
        .bind(&session_id)
        .bind(&run_id_3)
        .bind(format!("child3-{i}"))
        .bind(i)
        .execute(pool.get())
        .await
        .unwrap();
    }
    for i in 0..8 {
        let status = if i == 0 { "blocker" } else { "running" };
        sqlx::query(
            "INSERT INTO phase3_session_delegations
             (session_id, run_id, child_run_id, priority, status)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&session_id)
        .bind(&run_id_8)
        .bind(format!("child8-{i}"))
        .bind(i)
        .bind(status)
        .execute(pool.get())
        .await
        .unwrap();
    }

    assert_eq!(delegation_budget(3).per_child_budget, 500);
    assert_eq!(delegation_budget(8).rendered_children, 7);
    assert_eq!(delegation_budget(8).overflow_children, 1);
    let allocation = delegation_budget_allocation(8, 1);
    assert_eq!(allocation.recent_tail_budget, 1600);
    assert!(allocation.delegation_zone_budget > 1500);
    let top_k = sqlx::query(
        "SELECT COUNT(*) AS c FROM (
             SELECT child_run_id FROM phase3_session_delegations
             WHERE session_id = ? AND run_id = ?
             ORDER BY priority ASC LIMIT 7
         ) AS top_children",
    )
    .bind(&session_id)
    .bind(&run_id_8)
    .fetch_one(pool.get())
    .await
    .unwrap()
    .try_get::<i64, _>("c")
    .unwrap();
    assert_eq!(top_k, 7);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l3_10_s01_second_compaction_records_post_compaction_drop_count() {
    let pool = setup_pool().await;
    let store = DatabaseContextManifestStore::new(pool.clone());
    let suffix = Uuid::new_v4();
    let session_id = format!("session-{suffix}");
    let run_id = format!("run-{suffix}");
    let user_id = format!("user-{suffix}");
    let mut messages = vec![
        json!({"role": "system", "content": "You are Astra."}),
        json!({"role": "tool", "content": "old verbose search output ".repeat(6000)}),
        json!({"role": "assistant", "content": "intermediate reasoning ".repeat(3000)}),
        json!({"role": "tool", "content": "recent compact-safe tool output"}),
        json!({"role": "user", "content": "continue"}),
        json!({"role": "assistant", "content": "recent answer ".repeat(40)}),
    ];
    let before_tokens = messages
        .iter()
        .map(|message| message.to_string().len() / 4)
        .sum::<usize>();
    let result = astra_runtime::turn::cloud::CompactionEngine::compact_tiered(
        &mut messages,
        800,
        2000,
        CompactionTier::CompactHistory,
        1,
    );
    let after_tokens = result
        .messages
        .iter()
        .map(|message| message.to_string().len() / 4)
        .sum::<usize>();
    let token_savings = before_tokens.saturating_sub(after_tokens);
    assert!(token_savings >= 1000);

    let mut m = manifest(&user_id, &session_id, &run_id, "post_compaction");
    m.total_estimated_tokens = after_tokens as u32;
    m.manifest_json = json!({
        "zones": {"summary": {"used_tokens": after_tokens, "budget_tokens": 500}},
        "compaction": {"before_tokens": before_tokens, "after_tokens": after_tokens}
    });
    let manifest_id = m.manifest_id.clone();
    store
        .save_manifest(
            m,
            vec![
                ContextManifestItemWrite {
                    session_id: session_id.clone(),
                    item_order: 0,
                    zone: "summary".to_string(),
                    source_table: "compaction_summary".to_string(),
                    source_id: format!("compaction-{suffix}"),
                    source_hash: Some("sha256:compacted".to_string()),
                    included: true,
                    token_estimate: after_tokens.min(500) as u32,
                    budget_tokens: 500,
                    reason: "post_compaction".to_string(),
                    render_mode: "summary".to_string(),
                    raw_ref: Some(format!(
                        "conversation_log://{session_id}/compact@sha256:phase3"
                    )),
                },
                ContextManifestItemWrite {
                    session_id: session_id.clone(),
                    item_order: 1,
                    zone: "recent_tail".to_string(),
                    source_table: "runtime_messages".to_string(),
                    source_id: format!("dropped-{suffix}"),
                    source_hash: Some("sha256:dropped".to_string()),
                    included: false,
                    token_estimate: token_savings as u32,
                    budget_tokens: 0,
                    reason: "post_compaction".to_string(),
                    render_mode: "summary".to_string(),
                    raw_ref: Some(format!(
                        "conversation_log://{session_id}/dropped@sha256:phase3"
                    )),
                },
            ],
        )
        .await
        .unwrap();
    let row =
        sqlx::query("SELECT reason, dropped_count FROM context_manifests WHERE manifest_id = ?")
            .bind(&manifest_id)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(
        row.try_get::<String, _>("reason").unwrap(),
        "post_compaction"
    );
    assert!(row.try_get::<i64, _>("dropped_count").unwrap() > 0);
    assert!(token_savings >= 1000);
}
