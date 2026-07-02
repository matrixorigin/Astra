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


if __name__ == "__main__":
    unittest.main()
