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

    def test_auto_increment_audit_is_not_legacy_three_table_claim(self) -> None:
        auto_increment = set(self.summary["auto_increment_tables"])
        self.assertGreaterEqual(len(auto_increment), 9)
        self.assertIn("agent_message_queue", auto_increment)
        self.assertIn("edge_pending_dispatch", auto_increment)
        self.assertIn("auth_user_roles", auto_increment)
        self.assertIn("mcp_servers", auto_increment)

    def test_inventory_is_json_serializable(self) -> None:
        encoded = json.dumps(self.inventory, sort_keys=True)
        self.assertIn('"schema_sources"', encoded)
        self.assertIn('"tables"', encoded)
        self.assertIn('"classified_table_count"', encoded)


if __name__ == "__main__":
    unittest.main()
