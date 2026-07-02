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
        cls.summary = cls.inventory["summary"]

    def test_core_storage_table_count_is_current_baseline(self) -> None:
        core_tables = [
            table
            for table in self.inventory["tables"]
            if table["domain"] == "core_storage"
        ]
        self.assertEqual(
            len(core_tables),
            91,
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

    def test_high_growth_tables_have_retention_metadata(self) -> None:
        high_growth_tables = [
            "agent_events",
            "agent_run_events",
            "run_checkpoints",
            "conversation_log",
            "session_tool_outputs",
            "prompt_request_records",
            "prompt_deltas",
            "session_sync_log",
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

    def test_session_sync_log_is_product_audit_not_dead_table(self) -> None:
        row = self.tables["session_sync_log"]
        self.assertIn("best-effort", row["state_class"])
        self.assertIn("sync_status", row["merge_guidance"])
        self.assertIn("sync_log_days default 30", row["retention_policy"])
        self.assertIn("not exactly rebuildable", row["rebuildability"])

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
