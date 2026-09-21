//! System HTTP end-to-end: real Axum app + MatrixOne + full `build_server_state` wiring.
//!
//! The journey implementations live in the `journey_*` modules. This file is
//! deliberately only the test registration layer: every generated case keeps
//! its own libtest identity while sharing the same environment gate and
//! Tokio/ignore metadata.
//!
//! ## How to run
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 \
//! ASTRA_TEST_E2E_SECRET=system-matrix-e2e-secret \
//! ASTRA_BACKEND_SERVICE_KEY=test-service-key-e2e \
//! ASTRA_LLM_RETRY_BASE_MS=10 ASTRA_DEFAULT_RETRY_AFTER_MS=10 ASTRA_BCRYPT_COST=4 \
//! RUST_MIN_STACK=16777216 \
//! cargo test -p astra-runtime --test system_matrix_http_e2e --features e2e-hooks -- \
//!   --ignored --nocapture
//! ```
//!
//! Requires the same environment as `astra-server` startup: `MATRIXONE_*`,
//! `ASTRA_JWT_SECRET`, and the other settings loaded by
//! [`astra_core::AppSettings::from_env`]. See
//! `docs/testing/system-e2e-matrix.md` for the capability mapping and
//! isolation rules.

mod harness;
mod journey_admin_smoke_matrix;
mod journey_branches_matrix;
mod journey_context_decision_chain_matrix;
mod journey_delegate_http_matrix;
mod journey_evaluation_reads_matrix;
mod journey_extended;
mod journey_full;
mod journey_full_capture_matrix;
mod journey_meta_matrix;
mod journey_models_matrix;
mod journey_phase0_production_baseline;
mod journey_phase0_production_topologies;
mod journey_saas_negative_matrix;
mod journey_saas_platform_matrix;
mod journey_session_artifacts_matrix;
mod journey_session_http_db_matrix;
mod journey_stream_persistence;
mod journey_tasks_runs;
mod journey_team_crud_matrix;
mod journey_team_data_fidelity_matrix;
mod journey_team_http_negatives_matrix;
mod journey_team_isolation_matrix;
mod journey_team_snapshots_matrix;

use harness::require_system_e2e_env;

macro_rules! matrix_test {
    (
        $(#[$extra:meta])*
        $name:ident, $workers:literal, $reason:literal, $runner:path
    ) => {
        $(#[$extra])*
        #[tokio::test(flavor = "multi_thread", worker_threads = $workers)]
        #[ignore = $reason]
        async fn $name() {
            require_system_e2e_env();
            $runner().await;
        }
    };
}

macro_rules! current_thread_matrix_test {
    (
        $(#[$extra:meta])*
        $name:ident, $reason:literal, $runner:path
    ) => {
        $(#[$extra])*
        #[tokio::test(flavor = "current_thread")]
        #[ignore = $reason]
        async fn $name() {
            require_system_e2e_env();
            $runner().await;
        }
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn product_matrix_api_journey_hits_multiple_tables() {
    require_system_e2e_env();
    let b = harness::bootstrap().await;
    journey_full::run_product_matrix_full_journey(&b.ctx, &b.auth_header, &b.refresh_token).await;
    b.ctx.close().await;
}

matrix_test! {
    e2e_matrix_chat_run_pause_resume_http, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_tasks_runs::run_chat_run_pause_resume_http
}
matrix_test! {
    e2e_matrix_orphan_cancel_claim_race_http, 4,
    "live MatrixOne; production orphan claim x HTTP DELETE cancellation race",
    journey_tasks_runs::run_orphan_cancel_claim_race_http
}
matrix_test! {
    e2e_matrix_paused_accounting_generation_fence_http, 2,
    "live MatrixOne + mock LLM; paused accounting generation fence",
    journey_tasks_runs::run_paused_accounting_generation_fence_http
}
matrix_test! {
    e2e_matrix_live_pause_wins_post_loop_settlement_accounting, 4,
    "live MatrixOne + mock LLM; pause wins the post-loop settlement race",
    journey_tasks_runs::run_live_pause_wins_post_loop_settlement_accounting
}
matrix_test! {
    e2e_matrix_session_cancel_delete, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_extended::run_session_cancel_then_delete
}
matrix_test! {
    e2e_matrix_chat_stream_session_info, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_extended::run_chat_stream_session_info_smoke
}
matrix_test! {
    e2e_matrix_stream_session_metadata_enables_full_llm_exchange_journaling, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_full_capture_matrix::run_stream_session_metadata_enables_full_llm_exchange_journaling
}
matrix_test! {
    e2e_matrix_stream_bootstrap_cleanup_preserves_live_fixture, 4,
    "live MatrixOne + mock LLM; overlapping fixture cleanup isolation gate",
    journey_stream_persistence::run_stream_bootstrap_cleanup_preserves_live_fixture
}
matrix_test! {
    e2e_matrix_approval_respond_invalid_session_id, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_extended::run_approval_respond_invalid_session_id_rejected
}
matrix_test! {
    e2e_matrix_edge_callback_http_boundary_failures, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_extended::run_edge_callback_http_boundary_failures
}
matrix_test! {
    e2e_matrix_duplicate_tool_result_idempotency, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_extended::run_duplicate_tool_result_server_stream_is_idempotent
}
matrix_test! {
    e2e_matrix_duplicate_approval_response_idempotency, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_extended::run_duplicate_approval_response_is_idempotent
}
matrix_test! {
    e2e_matrix_server_stream_partial_batch_failure, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_extended::run_server_stream_partial_batch_failure
}
matrix_test! {
    e2e_matrix_server_stream_out_of_order_tool_results, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_extended::run_server_stream_out_of_order_tool_results
}
matrix_test! {
    e2e_matrix_auth_session_negative_paths, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_extended::run_auth_and_session_negative_paths
}
matrix_test! {
    e2e_matrix_memory_proxy_user_isolation, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_extended::run_memory_proxy_user_isolation
}
matrix_test! {
    e2e_matrix_models_admin_crud, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_extended::run_models_admin_crud_with_db
}
matrix_test! {
    e2e_matrix_stream_session_and_run_status, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_stream_persistence::run_stream_session_and_run_status
}
matrix_test! {
    e2e_matrix_stream_context_trace_persistence, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_stream_persistence::run_stream_context_trace_persistence
}
matrix_test! {
    e2e_matrix_stream_multi_turn_persistence, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_stream_persistence::run_stream_multi_turn_persistence
}
matrix_test! {
    #[serial_test::serial(phase0_production_baseline)]
    e2e_matrix_phase0_server_only_production_baseline, 2,
    "live MatrixOne + three real provider Offerings; dedicated Phase-0 ServerOnly baseline",
    journey_phase0_production_baseline::run_server_only_production_baseline
}
matrix_test! {
    #[serial_test::serial(phase0_production_baseline)]
    e2e_matrix_phase0_external_production_topologies, 4,
    "live MatrixOne + real provider + explicit production binary paths; complete Phase-0 topology baseline",
    journey_phase0_production_topologies::run_external_production_topologies
}
matrix_test! {
    #[serial_test::serial(phase0_production_baseline)]
    e2e_matrix_phase0_external_edge_server_m1, 4,
    "live MatrixOne + real provider + explicit production binary paths; exact Edge+Server × 1M diagnostic",
    journey_phase0_production_topologies::run_external_edge_server_m1
}
matrix_test! {
    e2e_matrix_stream_structured_fanout_has_one_parent_synthesis_and_durable_tree, 4,
    "live MatrixOne + mock parent/child LLM; structured fan-in online gate",
    journey_stream_persistence::run_stream_structured_fanout_has_one_parent_synthesis_and_durable_tree
}
matrix_test! {
    e2e_matrix_stream_concurrent_fanout_isolates_users_sessions_and_group_ids, 8,
    "live MatrixOne + mock parent/child LLM; concurrent fanout ownership/isolation gate",
    journey_stream_persistence::run_stream_concurrent_fanout_isolates_users_sessions_and_group_ids
}
matrix_test! {
    e2e_matrix_stream_root_cancel_settles_slow_fanout_without_late_synthesis, 6,
    "live MatrixOne + delayed mock child LLM; root/fanout cancellation race gate",
    journey_stream_persistence::run_stream_root_cancel_settles_slow_fanout_without_late_synthesis
}
matrix_test! {
    e2e_matrix_stream_canonical_work_scheduler_prevents_decorative_plan, 4,
    "live MatrixOne + mock parent/child LLM; canonical Work scheduler online gate",
    journey_stream_persistence::run_stream_canonical_work_scheduler_prevents_decorative_plan
}
matrix_test! {
    e2e_matrix_stream_deferred_work_does_not_start_an_attempt, 4,
    "live MatrixOne + mock LLM; deferred canonical Work online gate",
    journey_stream_persistence::run_stream_deferred_work_does_not_start_an_attempt
}
matrix_test! {
    e2e_matrix_stream_failed_fanout_settles_once_without_orphaning_children, 4,
    "live MatrixOne + failing mock child LLM; structured fan-in unhappy-path gate",
    journey_stream_persistence::run_stream_failed_fanout_settles_once_without_orphaning_children
}
matrix_test! {
    e2e_matrix_team_crud_and_db, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_team_crud_matrix::run_team_crud_db
}
matrix_test! {
    e2e_matrix_team_snapshots_and_db, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_team_snapshots_matrix::run_team_snapshots_db
}
matrix_test! {
    e2e_matrix_team_http_negative_paths, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_team_http_negatives_matrix::run_team_http_negative_paths
}
matrix_test! {
    e2e_matrix_team_http_db_fidelity, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_team_data_fidelity_matrix::run_team_http_db_fidelity
}
matrix_test! {
    e2e_matrix_team_cross_user_isolation, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_team_isolation_matrix::run_team_cross_user_isolation
}
matrix_test! {
    e2e_matrix_meta_health, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_meta_matrix::run_meta_root_and_health
}
matrix_test! {
    e2e_matrix_session_http_db, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_session_http_db_matrix::run_session_http_matches_agent_sessions_row
}
matrix_test! {
    e2e_matrix_session_artifact_http_db, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_session_artifacts_matrix::run_session_artifact_http_matches_session_artifacts_rows
}
matrix_test! {
    e2e_matrix_published_artifact_http_round_trip, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_session_artifacts_matrix::run_published_session_artifact_round_trip
}
matrix_test! {
    e2e_matrix_session_artifact_latest_and_download, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_session_artifacts_matrix::run_session_artifact_latest_and_download_routes
}
matrix_test! {
    e2e_matrix_failed_session_artifact_latest_and_download, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_session_artifacts_matrix::run_failed_session_artifact_latest_and_download_routes
}
matrix_test! {
    e2e_matrix_server_loop_block_parse_preserves_partial_without_replay, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_session_artifacts_matrix::run_server_loop_block_parse_preserves_partial_without_replay_routes
}
matrix_test! {
    e2e_matrix_server_loop_transport_preserves_partial_without_replay, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_session_artifacts_matrix::run_server_loop_transport_preserves_partial_without_replay_routes
}
matrix_test! {
    e2e_matrix_server_loop_idle_preserves_partial_without_replay, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_session_artifacts_matrix::run_server_loop_idle_preserves_partial_without_replay_routes
}
matrix_test! {
    e2e_matrix_server_loop_rate_limit_failure_session_artifact_latest_and_download, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_session_artifacts_matrix::run_server_loop_rate_limit_failure_session_artifact_latest_and_download_routes
}
matrix_test! {
    e2e_matrix_server_loop_rate_limit_retry_success_session_artifact_latest_and_download, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_session_artifacts_matrix::run_server_loop_rate_limit_retry_success_session_artifact_latest_and_download_routes
}
matrix_test! {
    e2e_matrix_session_artifact_latest_tiebreaker, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_session_artifacts_matrix::run_session_artifact_latest_route_uses_stable_tiebreaker
}
matrix_test! {
    e2e_matrix_evaluation_reads, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_evaluation_reads_matrix::run_evaluation_read_http_smoke
}
matrix_test! {
    e2e_matrix_context_decision_chain, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_context_decision_chain_matrix::run_context_decision_chain_db
}
matrix_test! {
    e2e_matrix_models, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_models_matrix::run_models_smoke
}
matrix_test! {
    e2e_matrix_branches_cost_estimate_http, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_branches_matrix::run_branches_cost_estimate_http
}
matrix_test! {
    e2e_matrix_delegate_http_boundaries, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_delegate_http_matrix::run_delegate_http_boundaries
}
matrix_test! {
    e2e_matrix_admin_control_plane_rbac, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc",
    journey_admin_smoke_matrix::run_admin_control_plane_rbac
}
matrix_test! {
    e2e_matrix_saas_resource_limits_read_and_admin_override, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.3",
    journey_saas_platform_matrix::run_saas_resource_limits_read_and_admin_override
}
matrix_test! {
    e2e_matrix_saas_resource_daily_session_cap_denies_chat, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.3",
    journey_saas_platform_matrix::run_saas_resource_daily_session_cap_denies_chat
}
matrix_test! {
    e2e_matrix_saas_resource_concurrent_session_cap_denies_chat, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.3",
    journey_saas_platform_matrix::run_saas_resource_concurrent_session_cap_denies_chat
}
matrix_test! {
    e2e_matrix_saas_admin_config_crud_rbac, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.2",
    journey_saas_platform_matrix::run_saas_admin_config_crud_rbac
}
matrix_test! {
    e2e_matrix_saas_admin_grant_revoke_rbac_flow, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.2",
    journey_saas_platform_matrix::run_saas_admin_grant_revoke_rbac_flow
}
matrix_test! {
    e2e_matrix_saas_resource_usage_per_user_isolation, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.3",
    journey_saas_platform_matrix::run_saas_resource_usage_per_user_isolation
}
matrix_test! {
    e2e_matrix_saas_auth_refresh_cycle, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.1",
    journey_saas_platform_matrix::run_saas_auth_refresh_cycle
}
matrix_test! {
    e2e_matrix_saas_session_cross_user_isolation, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.4",
    journey_saas_platform_matrix::run_saas_session_cross_user_isolation
}
matrix_test! {
    e2e_matrix_saas_events_and_audit_cross_user_isolation, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.4",
    journey_saas_platform_matrix::run_saas_events_and_audit_cross_user_isolation
}
matrix_test! {
    e2e_matrix_saas_auth_negative_paths, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.1 auth negatives",
    journey_saas_negative_matrix::run_saas_auth_negative_paths
}
matrix_test! {
    e2e_matrix_saas_resource_governance_negative_paths, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.3 resource negatives",
    journey_saas_negative_matrix::run_saas_resource_governance_negative_paths
}
matrix_test! {
    e2e_matrix_saas_resource_concurrent_cap_recovery, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.3 concurrent cap recovery",
    journey_saas_negative_matrix::run_saas_resource_concurrent_cap_recovery
}
matrix_test! {
    e2e_matrix_saas_auth_logout_and_expired_token, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.1 logout/expired JWT",
    journey_saas_negative_matrix::run_saas_auth_logout_and_expired_token
}
matrix_test! {
    e2e_matrix_saas_resource_limits_extended_fields, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.3 bash/disk limits contract",
    journey_saas_negative_matrix::run_saas_resource_limits_extended_fields
}
matrix_test! {
    e2e_matrix_saas_edge_tool_result_success_path, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §4.2 edge callback success",
    journey_saas_negative_matrix::run_saas_edge_tool_result_success_path
}
matrix_test! {
    e2e_matrix_saas_memoria_proxy_degradation, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.7 Memoria degradation",
    journey_saas_negative_matrix::run_saas_memoria_proxy_degradation
}
matrix_test! {
    e2e_matrix_saas_run_cross_user_isolation, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §4.3 run isolation",
    journey_saas_negative_matrix::run_saas_run_cross_user_isolation
}
matrix_test! {
    e2e_matrix_saas_run_double_pause_conflict, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §4.3 run state conflict",
    journey_saas_negative_matrix::run_saas_run_double_pause_conflict
}
matrix_test! {
    e2e_matrix_saas_edges_status_smoke, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §4.2 edges/status",
    journey_saas_negative_matrix::run_saas_edges_status_smoke
}
current_thread_matrix_test! {
    e2e_matrix_saas_service_edges_status_smoke,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §4.2 service/edges/status auth gate",
    journey_saas_negative_matrix::run_saas_service_edges_status_smoke
}
matrix_test! {
    e2e_matrix_saas_approval_respond_success_path, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §4.2 approval callback",
    journey_saas_negative_matrix::run_saas_approval_respond_success_path
}
matrix_test! {
    e2e_matrix_saas_platform_health_and_auth_me, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.1/§7.4 health+me",
    journey_saas_platform_matrix::run_saas_platform_health_and_auth_me
}
matrix_test! {
    e2e_matrix_saas_auth_refresh_token_rotation, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.1 refresh rotation",
    journey_saas_platform_matrix::run_saas_auth_refresh_token_rotation
}
matrix_test! {
    e2e_matrix_saas_memory_proxy_user_isolation, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.4/§5.7 memory isolation",
    journey_saas_platform_matrix::run_saas_memory_proxy_user_isolation
}
matrix_test! {
    e2e_matrix_saas_models_list_and_key_encryption, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.6 models list+encryption",
    journey_saas_platform_matrix::run_saas_models_list_and_key_encryption
}
matrix_test! {
    e2e_matrix_saas_session_lifecycle_positive, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.1/§6.1 session CRUD",
    journey_saas_platform_matrix::run_saas_session_lifecycle_positive
}
matrix_test! {
    e2e_matrix_saas_resource_usage_increments_after_chat, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.3/§6.6 usage increment",
    journey_saas_platform_matrix::run_saas_resource_usage_increments_after_chat
}
matrix_test! {
    e2e_matrix_saas_run_cancel_cross_user_and_owner, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §4.3 run cancel isolation",
    journey_saas_platform_matrix::run_saas_run_cancel_cross_user_and_owner
}
matrix_test! {
    e2e_matrix_saas_approval_respond_deny_path, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §4.2 approval deny",
    journey_saas_platform_matrix::run_saas_approval_respond_deny_path
}
matrix_test! {
    e2e_matrix_saas_chat_run_pause_resume_positive, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §4.3 pause/resume",
    journey_saas_platform_matrix::run_saas_chat_run_pause_resume_positive
}
matrix_test! {
    e2e_matrix_saas_admin_tokens_rbac_smoke, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.2 admin tokens",
    journey_saas_platform_matrix::run_saas_admin_tokens_rbac_smoke
}
matrix_test! {
    e2e_matrix_saas_auth_register_login_positive, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.1 register/login",
    journey_saas_platform_matrix::run_saas_auth_register_login_positive
}
matrix_test! {
    e2e_matrix_saas_auth_duplicate_email_register, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.1 duplicate email",
    journey_saas_platform_matrix::run_saas_auth_duplicate_email_register
}
matrix_test! {
    e2e_matrix_saas_runs_list_pagination_positive, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §4.3 GET /runs",
    journey_saas_platform_matrix::run_saas_runs_list_pagination_positive
}
matrix_test! {
    e2e_matrix_saas_edge_agent_registration_smoke, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §4.2 edge register",
    journey_saas_platform_matrix::run_saas_edge_agent_registration_smoke
}
matrix_test! {
    e2e_matrix_saas_admin_cleanup_rbac_smoke, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.2 admin cleanup",
    journey_saas_platform_matrix::run_saas_admin_cleanup_rbac_smoke
}
matrix_test! {
    e2e_matrix_saas_admin_audit_rbac_smoke, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.2 admin audit",
    journey_saas_platform_matrix::run_saas_admin_audit_rbac_smoke
}
matrix_test! {
    e2e_matrix_saas_skills_cross_user_isolation, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.4 skills isolation",
    journey_saas_platform_matrix::run_saas_skills_cross_user_isolation
}
matrix_test! {
    e2e_matrix_saas_team_cross_user_isolation, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.4 team isolation",
    journey_saas_platform_matrix::run_saas_team_cross_user_isolation
}
matrix_test! {
    e2e_matrix_saas_session_replay_compare_unavailable_guardrail, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §6.1 replay compare guardrail",
    journey_saas_platform_matrix::run_saas_session_replay_compare_unavailable_guardrail
}
matrix_test! {
    e2e_matrix_saas_session_replay_post_unavailable_guardrail, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §6.1 replay POST guardrail",
    journey_saas_platform_matrix::run_saas_session_replay_post_unavailable_guardrail
}
matrix_test! {
    e2e_matrix_saas_admin_feedback_stats_rbac, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.2 admin feedback stats",
    journey_saas_platform_matrix::run_saas_admin_feedback_stats_rbac
}
matrix_test! {
    e2e_matrix_saas_run_projection_smoke, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §4.3 run projection",
    journey_saas_platform_matrix::run_saas_run_projection_smoke
}
matrix_test! {
    e2e_matrix_saas_session_audit_after_chat_smoke, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.4 session audit smoke",
    journey_saas_platform_matrix::run_saas_session_audit_after_chat_smoke
}
matrix_test! {
    e2e_matrix_saas_platform_snapshot_smoke, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.1 platform snapshot",
    journey_saas_platform_matrix::run_saas_platform_snapshot_smoke
}
matrix_test! {
    e2e_matrix_saas_session_activity_transcript_artifacts_smoke, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.1 session activity/transcript",
    journey_saas_platform_matrix::run_saas_session_activity_transcript_artifacts_smoke
}
matrix_test! {
    e2e_matrix_saas_events_session_after_chat_positive, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.4 events/session positive",
    journey_saas_platform_matrix::run_saas_events_session_after_chat_positive
}
matrix_test! {
    e2e_matrix_saas_delegate_http_boundaries, 2,
    "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §4.3 delegate HTTP",
    journey_saas_platform_matrix::run_saas_delegate_http_boundaries
}
