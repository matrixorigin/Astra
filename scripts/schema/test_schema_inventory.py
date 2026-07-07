#!/usr/bin/env python3
import json
import sys
import unittest
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPT_DIR))

import schema_inventory  # noqa: E402


class SchemaInventoryTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.inventory = schema_inventory.build_inventory()
        cls.tables = {
            table["table"]: table for table in cls.inventory["tables"]
        }
        cls.p1_5_reviews = {
            review["candidate"]: review
            for review in cls.inventory["p1_5_consolidation_reviews"]
        }
        cls.summary = cls.inventory["summary"]

    def test_core_storage_table_count_is_current_baseline(self) -> None:
        core_tables = [
            table
            for table in self.inventory["tables"]
            if table["domain"] == "core_storage"
        ]
        self.assertEqual(
            len(core_tables),
            90,
            "core storage DDL count changed; update the schema plan and inventory baseline",
        )

    def test_inventory_includes_non_storage_schema_owners(self) -> None:
        expected = {
            "agent_message_queue": "messaging",
            "agent_message_broadcast_delivery": "messaging",
            "resource_limits": "resource_governor",
            "resource_usage": "resource_governor",
            "llm_provider_admission_windows": "runtime_admission",
            "workspace_records": "workspace_records",
            "workspace_cleanup_debts": "workspace_records",
            "config_versions": "config_versions",
        }
        for table, domain in expected.items():
            with self.subTest(table=table):
                self.assertIn(table, self.tables)
                self.assertEqual(self.tables[table]["domain"], domain)

    def test_schema_source_manifest_covers_all_production_ddl_sources(self) -> None:
        discovered = set(schema_inventory.discover_production_ddl_source_paths())
        manifest = {source.path for source in schema_inventory.SCHEMA_SOURCES}
        self.assertEqual(
            discovered,
            manifest,
            "every production Rust src file with CREATE TABLE DDL must be declared in SCHEMA_SOURCES",
        )

    def test_first_batch_tables_have_semantic_metadata(self) -> None:
        first_batch = {
            "agent_events",
            "agent_run_events",
            "agent_sessions",
            "agent_runs",
            "run_checkpoints",
            "run_display_projections",
            "conversation_log",
            "session_transcript_items",
            "transcript_pages",
            "session_history_chunks",
            "session_tool_output_batches",
            "session_tool_outputs",
            "tool_exactly_once_results",
        }
        for table in first_batch:
            with self.subTest(table=table):
                row = self.tables[table]
                for field in [
                    "semantic_owner",
                    "state_class",
                    "primary_query",
                    "retention_policy",
                    "rebuildability",
                    "merge_guidance",
                    "migration_owner",
                    "product_owner",
                ]:
                    self.assertNotEqual(
                        row[field],
                        "unclassified",
                        f"{table}.{field} must be explicit metadata",
                    )

    def test_schema_metadata_locks_key_boundary_decisions(self) -> None:
        self.assertIn(
            "do not merge into agent_run_events",
            self.tables["agent_events"]["merge_guidance"],
        )
        self.assertIn(
            "do not merge into agent_events",
            self.tables["agent_run_events"]["merge_guidance"],
        )
        self.assertIn(
            "not rebuildable safely",
            self.tables["tool_exactly_once_results"]["rebuildability"],
        )
        self.assertIn(
            "large payload",
            self.tables["session_tool_outputs"]["state_class"],
        )
        self.assertIn(
            "owner lease recovery",
            self.tables["agent_runs"]["primary_query"],
        )
        self.assertIn(
            "not rebuildable from events",
            self.tables["agent_runs"]["rebuildability"],
        )
        self.assertIn(
            "do not merge into agent_runs",
            self.tables["run_display_projections"]["merge_guidance"],
        )
        self.assertIn(
            "rebuildable from agent_runs",
            self.tables["run_display_projections"]["rebuildability"],
        )
        self.assertIn(
            "do not merge into agent_runs",
            self.tables["run_checkpoints"]["merge_guidance"],
        )

    def test_run_lifecycle_tables_lock_authority_boundaries(self) -> None:
        sessions = self.tables["agent_sessions"]
        runs = self.tables["agent_runs"]
        checkpoints = self.tables["run_checkpoints"]
        projections = self.tables["run_display_projections"]

        self.assertIn("parent lifecycle row", sessions["retention_policy"])
        self.assertIn("cleanup boundary", sessions["merge_guidance"])
        self.assertIn("session lifecycle aggregate", sessions["state_class"])

        self.assertIn("durable run lifecycle authority", runs["state_class"])
        self.assertIn("pause/cancel", runs["retention_policy"])
        self.assertIn("owner lease generation", runs["rebuildability"])
        self.assertIn("events own replay", runs["merge_guidance"])
        self.assertIn("projections own repairable display state", runs["merge_guidance"])

        self.assertIn("no independent age-based TTL", checkpoints["retention_policy"])
        self.assertIn("checkpoint_json", checkpoints["rebuildability"])
        self.assertIn("typed, idempotent, multi-version", checkpoints["merge_guidance"])

        self.assertIn("derived run display projection", projections["state_class"])
        self.assertIn("clear and rebuild", projections["retention_policy"])
        self.assertIn("repairable display state", projections["merge_guidance"])

    def test_state_task_workspace_tables_have_semantic_metadata(self) -> None:
        second_batch = {
            "config_versions",
            "context_manifests",
            "session_state_revisions",
            "session_device_leases",
            "session_device_lease_events",
            "session_state_items",
            "session_delegations",
            "session_plan_todos",
            "session_todos",
            "session_todo_counters",
            "session_todo_idempotency",
            "data_versioning_checkpoints",
            "sweeper_leases",
            "workspace_records",
            "workspace_cleanup_debts",
        }
        for table in second_batch:
            with self.subTest(table=table):
                row = self.tables[table]
                for field in [
                    "semantic_owner",
                    "state_class",
                    "primary_query",
                    "retention_policy",
                    "rebuildability",
                    "merge_guidance",
                    "migration_owner",
                    "product_owner",
                ]:
                    self.assertNotEqual(
                        row[field],
                        "unclassified",
                        f"{table}.{field} must be explicit metadata",
                    )

    def test_state_task_workspace_metadata_locks_boundary_decisions(self) -> None:
        self.assertIn(
            "parent manifest summary",
            self.tables["context_manifests"]["merge_guidance"],
        )
        self.assertIn(
            "current projection surface",
            self.tables["session_state_items"]["merge_guidance"],
        )
        self.assertIn(
            "session-level revision",
            self.tables["session_state_revisions"]["merge_guidance"],
        )
        self.assertIn(
            "mutable current state",
            self.tables["session_device_leases"]["merge_guidance"],
        )
        self.assertIn(
            "append-only audit",
            self.tables["session_device_lease_events"]["merge_guidance"],
        )
        self.assertIn(
            "do not merge into agent_runs",
            self.tables["session_delegations"]["merge_guidance"],
        )
        self.assertIn(
            "incompatible schema and consumers",
            self.tables["session_plan_todos"]["merge_guidance"],
        )
        self.assertIn(
            "live task scratchpad",
            self.tables["session_todos"]["merge_guidance"],
        )
        self.assertIn(
            "deleted todos still reserve ids",
            self.tables["session_todo_counters"]["merge_guidance"],
        )
        self.assertIn(
            "queried directly",
            self.tables["session_todo_idempotency"]["merge_guidance"],
        )
        self.assertIn(
            "DatabaseDataVersioningService reads and writes",
            self.tables["data_versioning_checkpoints"]["merge_guidance"],
        )
        self.assertIn(
            "cleanup debt can outlive the workspace record",
            self.tables["workspace_cleanup_debts"]["merge_guidance"],
        )
        self.assertIn(
            "shared multi-pod leader election",
            self.tables["sweeper_leases"]["merge_guidance"],
        )

    def test_auth_admin_model_config_tables_have_semantic_metadata(self) -> None:
        config_security_tables = {
            "admin_config",
            "auth_users",
            "auth_roles",
            "auth_refresh_tokens",
            "auth_external_sessions",
            "auth_tokens",
            "auth_audit_logs",
            "infra_llm_models",
            "model_gateways",
            "runtime_llm_trusted_domains",
        }
        for table in config_security_tables:
            with self.subTest(table=table):
                row = self.tables[table]
                for field in [
                    "semantic_owner",
                    "state_class",
                    "primary_query",
                    "retention_policy",
                    "rebuildability",
                    "merge_guidance",
                    "migration_owner",
                    "product_owner",
                ]:
                    self.assertNotEqual(
                        row[field],
                        "unclassified",
                        f"{table}.{field} must be explicit metadata",
                    )

    def test_auth_admin_model_config_metadata_locks_boundary_decisions(self) -> None:
        self.assertIn(
            "fall back to code/default behavior",
            self.tables["admin_config"]["retention_policy"],
        )
        self.assertIn(
            "not the model registry itself",
            self.tables["admin_config"]["merge_guidance"],
        )
        self.assertIn(
            "deactivate via is_active",
            self.tables["auth_users"]["retention_policy"],
        )
        self.assertIn(
            "identity, grants, and sessions",
            self.tables["auth_users"]["merge_guidance"],
        )
        self.assertIn(
            "delete only after dependent auth_user_roles",
            self.tables["auth_roles"]["retention_policy"],
        )
        self.assertIn(
            "ordered bounded batches",
            self.tables["auth_refresh_tokens"]["retention_policy"],
        )
        self.assertIn(
            "high-churn secrets",
            self.tables["auth_refresh_tokens"]["merge_guidance"],
        )
        self.assertIn(
            "encrypted provider session handle",
            self.tables["auth_external_sessions"]["rebuildability"],
        )
        self.assertIn(
            "provider session",
            self.tables["auth_external_sessions"]["merge_guidance"],
        )
        self.assertIn(
            "encrypted_value or secret_ref",
            self.tables["auth_tokens"]["rebuildability"],
        )
        self.assertIn(
            "different trust boundaries",
            self.tables["auth_tokens"]["merge_guidance"],
        )
        self.assertIn(
            "product/security audit table",
            self.tables["auth_audit_logs"]["merge_guidance"],
        )
        self.assertIn(
            "active-model cache invalidation",
            self.tables["infra_llm_models"]["retention_policy"],
        )
        self.assertIn(
            "structured model registry",
            self.tables["infra_llm_models"]["merge_guidance"],
        )
        self.assertIn(
            "disable instead of deleting",
            self.tables["model_gateways"]["retention_policy"],
        )
        self.assertIn(
            "distinct from concrete model credentials",
            self.tables["model_gateways"]["merge_guidance"],
        )
        self.assertIn(
            "host/port trust policy",
            self.tables["runtime_llm_trusted_domains"]["merge_guidance"],
        )

    def test_skill_and_agent_tables_have_semantic_metadata(self) -> None:
        skill_agent_tables = {
            "skills_registry",
            "skill_metrics",
            "skill_selection_events",
            "skill_installations",
            "skill_settings",
            "skill_resource_bindings",
            "skill_user_credentials",
            "user_skill_sources",
            "user_skill_versions",
            "user_skill_evaluations",
            "agent_agents",
            "agent_bindings",
            "agent_tasks",
        }
        for table in skill_agent_tables:
            with self.subTest(table=table):
                row = self.tables[table]
                for field in [
                    "semantic_owner",
                    "state_class",
                    "primary_query",
                    "retention_policy",
                    "rebuildability",
                    "merge_guidance",
                    "migration_owner",
                    "product_owner",
                ]:
                    self.assertNotEqual(
                        row[field],
                        "unclassified",
                        f"{table}.{field} must be explicit metadata",
                    )

    def test_skill_and_agent_metadata_locks_boundary_decisions(self) -> None:
        self.assertIn(
            "shared runtime catalog",
            self.tables["skills_registry"]["merge_guidance"],
        )
        self.assertIn(
            "high-churn projections",
            self.tables["skill_metrics"]["merge_guidance"],
        )
        self.assertIn(
            "readers stop querying this table directly",
            self.tables["skill_selection_events"]["merge_guidance"],
        )
        self.assertIn(
            "user's installed/activated state",
            self.tables["skill_installations"]["merge_guidance"],
        )
        self.assertIn(
            "different secrecy and lookup semantics",
            self.tables["skill_settings"]["merge_guidance"],
        )
        self.assertIn(
            "external resources",
            self.tables["skill_resource_bindings"]["merge_guidance"],
        )
        self.assertIn(
            "encrypted user secrets",
            self.tables["skill_user_credentials"]["merge_guidance"],
        )
        self.assertIn(
            "source owns authoring identity",
            self.tables["user_skill_sources"]["merge_guidance"],
        )
        self.assertIn(
            "authoring/version content",
            self.tables["user_skill_versions"]["merge_guidance"],
        )
        self.assertIn(
            "run-linked review facts",
            self.tables["user_skill_evaluations"]["merge_guidance"],
        )
        self.assertIn(
            "user-owned agent definitions",
            self.tables["agent_agents"]["merge_guidance"],
        )
        self.assertIn(
            "idempotent creation semantics",
            self.tables["agent_bindings"]["merge_guidance"],
        )
        self.assertIn(
            "todos own user scratchpad tasks",
            self.tables["agent_tasks"]["merge_guidance"],
        )

    def test_session_workflow_coordination_tables_have_semantic_metadata(self) -> None:
        session_workflow_tables = {
            "agent_event_edges",
            "session_artifacts",
            "session_artifacts_grants",
            "session_checkpoints",
            "user_preferences",
            "edge_agent_registry",
            "task_leases",
            "plan_templates",
            "plans",
            "plan_step_runs",
            "task_contracts",
            "verification_results",
            "wf_triggers",
            "infra_sandbox_metadata",
            "team_definitions",
            "team_execution_history",
            "team_snapshots",
        }
        for table in session_workflow_tables:
            with self.subTest(table=table):
                row = self.tables[table]
                for field in [
                    "semantic_owner",
                    "state_class",
                    "primary_query",
                    "retention_policy",
                    "rebuildability",
                    "merge_guidance",
                    "migration_owner",
                    "product_owner",
                ]:
                    self.assertNotEqual(
                        row[field],
                        "unclassified",
                        f"{table}.{field} must be explicit metadata",
                    )

    def test_session_workflow_coordination_metadata_locks_boundary_decisions(self) -> None:
        self.assertIn(
            "must be deleted before event rows",
            self.tables["agent_event_edges"]["merge_guidance"],
        )
        self.assertIn(
            "content and retention state",
            self.tables["session_artifacts"]["merge_guidance"],
        )
        self.assertIn(
            "visibility/control-plane facts",
            self.tables["session_artifacts_grants"]["merge_guidance"],
        )
        self.assertIn(
            "session-level restore snapshots",
            self.tables["session_checkpoints"]["merge_guidance"],
        )
        self.assertIn(
            "per-user sync state",
            self.tables["user_preferences"]["merge_guidance"],
        )
        self.assertIn(
            "registry is liveness/capability state",
            self.tables["edge_agent_registry"]["merge_guidance"],
        )
        self.assertIn(
            "must lock before/with task rows",
            self.tables["task_leases"]["merge_guidance"],
        )
        self.assertIn(
            "reusable learned patterns",
            self.tables["plan_templates"]["merge_guidance"],
        )
        self.assertIn(
            "current mutable plan state",
            self.tables["plans"]["merge_guidance"],
        )
        self.assertIn(
            "append-only attempt history",
            self.tables["plan_step_runs"]["merge_guidance"],
        )
        self.assertIn(
            "contracts define expected criteria",
            self.tables["task_contracts"]["merge_guidance"],
        )
        self.assertIn(
            "evidence rows fan out",
            self.tables["verification_results"]["merge_guidance"],
        )
        self.assertIn(
            "separate activation lifecycle",
            self.tables["wf_triggers"]["merge_guidance"],
        )
        self.assertIn(
            "workspace_records track reusable workspaces",
            self.tables["infra_sandbox_metadata"]["merge_guidance"],
        )
        self.assertIn(
            "mutable team config",
            self.tables["team_definitions"]["merge_guidance"],
        )
        self.assertIn(
            "execution history",
            self.tables["team_execution_history"]["merge_guidance"],
        )
        self.assertIn(
            "point-in-time audit records",
            self.tables["team_snapshots"]["merge_guidance"],
        )

    def test_ctx_eval_harness_preview_tables_have_semantic_metadata(self) -> None:
        ctx_eval_harness_preview_tables = {
            "ctx_decision_audits",
            "ctx_snapshots",
            "eval_calibration_assessments",
            "eval_gate_results",
            "eval_quality_assessments",
            "eval_training_datasets",
            "eval_user_feedback",
            "harness_citations",
            "harness_items",
            "harness_runs",
            "harness_skill_drafts",
            "harness_skill_rules",
            "harness_snapshots",
            "llm_provider_admission_pacing",
            "preview_template_registry",
            "raw_ref_scheme_registry",
        }
        for table in ctx_eval_harness_preview_tables:
            with self.subTest(table=table):
                row = self.tables[table]
                for field in [
                    "semantic_owner",
                    "state_class",
                    "primary_query",
                    "retention_policy",
                    "rebuildability",
                    "merge_guidance",
                    "migration_owner",
                    "product_owner",
                ]:
                    self.assertNotEqual(
                        row[field],
                        "unclassified",
                        f"{table}.{field} must be explicit metadata",
                    )

    def test_ctx_eval_harness_preview_metadata_locks_boundary_decisions(self) -> None:
        self.assertIn(
            "avoid polluting session event counts",
            self.tables["harness_snapshots"]["merge_guidance"],
        )
        self.assertIn(
            "product workflow parents",
            self.tables["harness_runs"]["merge_guidance"],
        )
        self.assertIn(
            "higher cardinality",
            self.tables["harness_items"]["merge_guidance"],
        )
        self.assertIn(
            "pre-publication generated skill candidates",
            self.tables["harness_skill_drafts"]["merge_guidance"],
        )
        self.assertIn(
            "fan out from a draft",
            self.tables["harness_skill_rules"]["merge_guidance"],
        )
        self.assertIn(
            "evidence fanout rows",
            self.tables["harness_citations"]["merge_guidance"],
        )
        self.assertIn(
            "not timeline events or manifest item ordering",
            self.tables["ctx_snapshots"]["merge_guidance"],
        )
        self.assertIn(
            "capture model/routing decisions",
            self.tables["ctx_decision_audits"]["merge_guidance"],
        )
        self.assertIn(
            "change-level release decisions",
            self.tables["eval_gate_results"]["merge_guidance"],
        )
        self.assertIn(
            "target-level assessment state",
            self.tables["eval_quality_assessments"]["merge_guidance"],
        )
        self.assertIn(
            "confidence reliability",
            self.tables["eval_calibration_assessments"]["merge_guidance"],
        )
        self.assertIn(
            "materialized training/eval corpora",
            self.tables["eval_training_datasets"]["merge_guidance"],
        )
        self.assertIn(
            "feedback is evaluation input",
            self.tables["eval_user_feedback"]["merge_guidance"],
        )
        self.assertIn(
            "not preview rendering templates",
            self.tables["raw_ref_scheme_registry"]["merge_guidance"],
        )
        self.assertIn(
            "raw ref schemes control resolver",
            self.tables["preview_template_registry"]["merge_guidance"],
        )
        self.assertIn(
            "virtual-time concurrency smoothing",
            self.tables["llm_provider_admission_pacing"]["merge_guidance"],
        )

    def test_high_growth_tables_have_retention_metadata(self) -> None:
        high_growth_tables = [
            "agent_events",
            "agent_run_events",
            "run_checkpoints",
            "conversation_log",
            "session_tool_outputs",
            "prompt_request_records",
            "prompt_deltas",
            "agent_message_queue",
        ]
        for table in high_growth_tables:
            with self.subTest(table=table):
                row = self.tables[table]
                self.assertNotEqual(row["state_class"], "unclassified")
                self.assertNotEqual(row["primary_query"], "unclassified")
                self.assertNotEqual(row["retention_policy"], "unclassified")
                self.assertNotEqual(row["rebuildability"], "unclassified")
                self.assertNotEqual(row["merge_guidance"], "unclassified")

    def test_prompt_delta_retention_is_parent_bound(self) -> None:
        parent = self.tables["prompt_request_records"]
        child = self.tables["prompt_deltas"]
        self.assertIn("parent fact", parent["state_class"])
        self.assertIn("user_id/request_id", parent["primary_query"])
        self.assertIn("session is inactive", parent["retention_policy"])
        self.assertIn("run is terminal", parent["retention_policy"])
        self.assertIn("ordered bounded batches", parent["retention_policy"])
        self.assertIn("after child deltas", parent["retention_policy"])
        self.assertIn("created_at_unix_ms", parent["retention_policy"])
        self.assertIn("MatrixOne", parent["retention_policy"])
        self.assertIn("do not delete child deltas independently", parent["merge_guidance"])
        self.assertIn("prompt_request_records", child["retention_policy"])
        self.assertIn("independent TTL breaks", child["retention_policy"])
        self.assertIn("bounded batches", child["retention_policy"])
        self.assertIn("before prompt_request_records", child["retention_policy"])
        self.assertIn("position/delta_seq", child["primary_query"])

    def test_session_sync_log_is_removed_from_production_schema(self) -> None:
        self.assertNotIn(
            "session_sync_log",
            self.tables,
            "sync audit is tracing-only now; session_sync_log must not return to production DDL",
        )

        storage = (schema_inventory.REPO_ROOT / "crates/services/src/storage.rs").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("CREATE TABLE IF NOT EXISTS session_sync_log", storage)
        self.assertIn("DROP TABLE IF EXISTS session_sync_log", storage)

        state_sync = (
            schema_inventory.REPO_ROOT / "crates/services/src/state_sync.rs"
        ).read_text(encoding="utf-8")
        self.assertIn("SyncAuditWriter", state_sync)
        self.assertIn("Audit is intentionally not persisted to MatrixOne", state_sync)
        self.assertNotIn("session_sync_log", state_sync)

    def test_p1_5_consolidation_reviews_are_evidence_backed(self) -> None:
        expected = {
            "session_sync_log",
            "data_versioning_checkpoints",
            "preview_template_registry + raw_ref_scheme_registry",
            "harness_skill_drafts + harness_skill_rules",
            "team_execution_history + team_snapshots",
        }
        self.assertEqual(set(self.p1_5_reviews), expected)

        for candidate, review in self.p1_5_reviews.items():
            with self.subTest(candidate=candidate):
                self.assertIn(
                    review["decision"],
                    {"keep", "keep_separate", "removed"},
                )
                self.assertGreaterEqual(len(review["current_read_paths"]), 1)
                self.assertGreaterEqual(len(review["current_write_paths"]), 1)
                self.assertGreaterEqual(len(review["test_evidence"]), 1)
                for field in [
                    "user_api_impact",
                    "migration_backfill",
                    "rollback",
                    "rationale",
                ]:
                    self.assertNotEqual(review[field], "")
                    self.assertNotIn("TBD", review[field])

        self.assertIn("tracing-only", self.p1_5_reviews["session_sync_log"]["user_api_impact"])
        self.assertIn(
            "rollback/list",
            self.p1_5_reviews["data_versioning_checkpoints"]["user_api_impact"],
        )
        self.assertIn(
            "access checks",
            self.p1_5_reviews[
                "preview_template_registry + raw_ref_scheme_registry"
            ]["user_api_impact"],
        )
        self.assertIn(
            "distinct cardinality",
            self.p1_5_reviews[
                "harness_skill_drafts + harness_skill_rules"
            ]["user_api_impact"],
        )
        self.assertIn(
            "different resources",
            self.p1_5_reviews[
                "team_execution_history + team_snapshots"
            ]["user_api_impact"],
        )

    def test_p1_5_consolidation_source_evidence_still_exists(self) -> None:
        expectations = {
            "crates/services/src/state_sync.rs": [
                "SyncAuditWriter",
                "Audit is intentionally not persisted to MatrixOne",
            ],
            "crates/services/src/data_versioning.rs": [
                "data_versioning_checkpoints",
                "create_checkpoint",
                "list_checkpoints",
            ],
            "crates/services/src/storage.rs": [
                "preview_template_registry",
                "raw_ref_scheme_registry",
                "INSERT IGNORE INTO raw_ref_scheme_registry",
                "INSERT IGNORE INTO preview_template_registry",
            ],
            "crates/services/src/harness.rs": [
                "harness_skill_drafts",
                "harness_skill_rules",
                "INSERT INTO harness_skill_drafts",
                "INSERT INTO harness_skill_rules",
            ],
            "crates/services/src/team_persistence.rs": [
                "team_execution_history",
                "team_snapshots",
                "record_execution_start",
                "list_executions_page",
                "save_snapshot",
                "list_snapshots_page",
            ],
        }

        for relative_path, needles in expectations.items():
            text = (schema_inventory.REPO_ROOT / relative_path).read_text(encoding="utf-8")
            for needle in needles:
                with self.subTest(path=relative_path, needle=needle):
                    self.assertIn(needle, text)

    def test_agent_message_queue_retention_is_bounded(self) -> None:
        queue = self.tables["agent_message_queue"]
        delivery = self.tables["agent_message_broadcast_delivery"]
        self.assertIn("ordered bounded batches", queue["retention_policy"])
        self.assertIn("orphan broadcast delivery", queue["retention_policy"])
        self.assertIn("not rebuildable for pending messages", queue["rebuildability"])
        self.assertIn("ordered bounded batches", delivery["retention_policy"])
        self.assertIn("consumer-scoped delivery state", delivery["merge_guidance"])

    def test_conversation_log_retention_has_runtime_compaction_boundary(self) -> None:
        row = self.tables["conversation_log"]
        self.assertIn("model context reconstruction", row["retention_policy"])
        self.assertIn("runtime compaction", row["retention_policy"])
        self.assertIn("ordered bounded batches", row["retention_policy"])
        self.assertIn("session hard delete", row["retention_policy"])

    def test_session_high_growth_retention_mentions_bounded_hard_delete(self) -> None:
        for table in [
            "agent_events",
            "agent_run_events",
            "session_tool_output_batches",
            "session_tool_outputs",
        ]:
            with self.subTest(table=table):
                policy = self.tables[table]["retention_policy"]
                self.assertIn("session hard delete", policy)
                self.assertIn("ordered bounded batches", policy)

    def test_replay_and_tool_output_tables_do_not_have_independent_age_ttl(self) -> None:
        run_events = self.tables["agent_run_events"]
        self.assertIn("no independent age-based TTL", run_events["retention_policy"])
        self.assertIn("projection repair", run_events["retention_policy"])
        self.assertIn("not rebuildable", run_events["rebuildability"])

        batches = self.tables["session_tool_output_batches"]
        outputs = self.tables["session_tool_outputs"]
        self.assertIn("no independent age-based TTL", batches["retention_policy"])
        self.assertIn("no independent age-based TTL", outputs["retention_policy"])
        self.assertIn("artifact refs", outputs["retention_policy"])
        self.assertIn("not rebuildable", outputs["rebuildability"])

    def test_external_hot_coordination_tables_have_semantic_metadata(self) -> None:
        for table in [
            "agent_message_queue",
            "agent_message_broadcast_delivery",
            "edge_pending_dispatch",
            "context_manifest_items",
            "resource_limits",
            "resource_usage",
            "llm_provider_admission_windows",
        ]:
            with self.subTest(table=table):
                self.assertNotEqual(self.tables[table]["state_class"], "unclassified")
                self.assertNotEqual(self.tables[table]["primary_query"], "unclassified")

    def test_global_inventory_has_no_duplicate_table_names(self) -> None:
        self.assertEqual({}, self.summary["duplicate_table_names"])
        self.assertEqual(
            self.summary["table_declaration_count"],
            self.summary["unique_table_count"],
        )

    def test_global_inventory_has_no_foreign_key_tables(self) -> None:
        self.assertEqual([], self.summary["foreign_key_tables"])

    def test_auto_increment_audit_baseline_is_empty(self) -> None:
        auto_increment = set(self.summary["auto_increment_tables"])
        self.assertEqual(auto_increment, set())
        self.assertNotIn("agent_message_queue", auto_increment)
        self.assertNotIn("edge_pending_dispatch", auto_increment)
        self.assertNotIn("context_manifest_items", auto_increment)
        self.assertNotIn("session_state_item_events", auto_increment)
        self.assertNotIn("auth_user_roles", auto_increment)
        self.assertNotIn("auth_external_identities", auto_increment)
        self.assertNotIn("mcp_servers", auto_increment)
        self.assertNotIn("mcp_bindings", auto_increment)
        self.assertNotIn("mcp_tools", auto_increment)

    def test_agent_message_queue_uses_message_identity(self) -> None:
        row = self.tables["agent_message_queue"]
        self.assertEqual(row["primary_key"], ["message_id"])
        self.assertEqual(row["auto_increment_columns"], [])
        self.assertEqual(row["auto_increment_hotspot_risk"], "not_applicable")
        self.assertIn("created_at", row["primary_query"])
        self.assertIn("message_id", row["primary_query"])

    def test_broadcast_delivery_uses_consumer_scoped_identity(self) -> None:
        row = self.tables["agent_message_broadcast_delivery"]
        self.assertEqual(row["primary_key"], ["message_id", "consumer_id"])
        self.assertEqual(row["auto_increment_columns"], [])
        self.assertEqual(row["auto_increment_hotspot_risk"], "not_applicable")
        self.assertIn("consumer-scoped delivery state", row["merge_guidance"])

    def test_edge_pending_dispatch_uses_owner_request_identity(self) -> None:
        row = self.tables["edge_pending_dispatch"]
        self.assertEqual(row["primary_key"], ["user_id", "request_id"])
        self.assertEqual(row["auto_increment_columns"], [])
        self.assertEqual(row["auto_increment_hotspot_risk"], "not_applicable")
        self.assertIn("edge poll", row["primary_query"])

    def test_context_manifest_items_uses_manifest_order_identity(self) -> None:
        row = self.tables["context_manifest_items"]
        self.assertEqual(row["primary_key"], ["manifest_id", "item_order"])
        self.assertEqual(row["auto_increment_columns"], [])
        self.assertEqual(row["auto_increment_hotspot_risk"], "not_applicable")
        self.assertIn("manifest-local item", row["primary_query"])

    def test_session_state_item_events_uses_owner_event_identity(self) -> None:
        row = self.tables["session_state_item_events"]
        self.assertEqual(row["primary_key"], ["user_id", "event_id"])
        self.assertEqual(row["auto_increment_columns"], [])
        self.assertEqual(row["auto_increment_hotspot_risk"], "not_applicable")
        self.assertIn("state item audit", row["primary_query"])

    def test_auth_grants_and_external_identities_use_product_identity(self) -> None:
        role_row = self.tables["auth_user_roles"]
        self.assertEqual(role_row["primary_key"], ["user_id", "role_id"])
        self.assertEqual(role_row["auto_increment_columns"], [])
        self.assertEqual(role_row["auto_increment_hotspot_risk"], "not_applicable")
        self.assertIn("many-to-many grant fact", role_row["merge_guidance"])

        identity_row = self.tables["auth_external_identities"]
        self.assertEqual(identity_row["primary_key"], ["provider_id", "external_subject"])
        self.assertEqual(identity_row["auto_increment_columns"], [])
        self.assertEqual(identity_row["auto_increment_hotspot_risk"], "not_applicable")
        self.assertIn("external identity link", identity_row["state_class"])

    def test_mcp_registry_uses_owner_bound_string_identity(self) -> None:
        server_row = self.tables["mcp_servers"]
        self.assertEqual(server_row["primary_key"], ["owner_user_id", "id"])
        self.assertEqual(server_row["auto_increment_columns"], [])
        self.assertIn("server endpoint lifecycle", server_row["merge_guidance"])

        binding_row = self.tables["mcp_bindings"]
        self.assertEqual(binding_row["primary_key"], ["owner_user_id", "id"])
        self.assertEqual(binding_row["auto_increment_columns"], [])
        self.assertIn("encrypted credential", binding_row["rebuildability"])

        tool_row = self.tables["mcp_tools"]
        self.assertEqual(tool_row["primary_key"], ["owner_user_id", "binding_id", "tool_name"])
        self.assertEqual(tool_row["auto_increment_columns"], [])
        self.assertIn("rediscovering tools", tool_row["rebuildability"])

    def test_every_auto_increment_table_has_risk_audit_metadata(self) -> None:
        self.assertEqual([], self.summary["unaudited_auto_increment_tables"])
        for table in self.summary["auto_increment_tables"]:
            with self.subTest(table=table):
                row = self.tables[table]
                for field in [
                    "auto_increment_write_profile",
                    "auto_increment_owner_boundary",
                    "auto_increment_hotspot_risk",
                    "auto_increment_guidance",
                ]:
                    self.assertNotEqual(
                        row[field],
                        "not_applicable",
                        f"{table}.{field} must be explicit for AUTO_INCREMENT audit",
                    )

    def test_inventory_is_json_serializable(self) -> None:
        encoded = json.dumps(self.inventory, sort_keys=True)
        self.assertIn('"schema_sources"', encoded)
        self.assertIn('"tables"', encoded)
        self.assertIn('"classified_table_count"', encoded)
        self.assertIn('"audited_auto_increment_tables"', encoded)


if __name__ == "__main__":
    unittest.main()
