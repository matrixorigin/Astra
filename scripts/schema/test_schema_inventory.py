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

    def test_external_hot_coordination_tables_have_semantic_metadata(self) -> None:
        for table in [
            "agent_message_queue",
            "agent_message_broadcast_delivery",
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

    def test_auto_increment_audit_baseline_excludes_message_queue(self) -> None:
        auto_increment = set(self.summary["auto_increment_tables"])
        self.assertEqual(len(auto_increment), 8)
        self.assertNotIn("agent_message_queue", auto_increment)
        self.assertIn("edge_pending_dispatch", auto_increment)
        self.assertIn("auth_user_roles", auto_increment)
        self.assertIn("mcp_servers", auto_increment)

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

    def test_auto_increment_risk_classifies_remaining_hot_coordination_tables(self) -> None:
        self.assertEqual(
            self.tables["edge_pending_dispatch"]["auto_increment_hotspot_risk"],
            "medium",
        )
        self.assertIn(
            "unique (user_id, request_id)",
            self.tables["edge_pending_dispatch"]["auto_increment_owner_boundary"],
        )

    def test_auto_increment_risk_keeps_auth_admin_tables_low_priority(self) -> None:
        for table in ["auth_user_roles", "auth_external_identities", "mcp_servers"]:
            with self.subTest(table=table):
                self.assertEqual(
                    self.tables[table]["auto_increment_hotspot_risk"],
                    "low",
                )

    def test_inventory_is_json_serializable(self) -> None:
        encoded = json.dumps(self.inventory, sort_keys=True)
        self.assertIn('"schema_sources"', encoded)
        self.assertIn('"tables"', encoded)
        self.assertIn('"classified_table_count"', encoded)
        self.assertIn('"audited_auto_increment_tables"', encoded)


if __name__ == "__main__":
    unittest.main()
