"""Test transaction management completeness."""

import pytest
from pathlib import Path
import re


class TestTransactionCompleteness:
    """Verify all database operations have proper transaction management."""

    def test_critical_paths_have_transactions(self):
        """Critical database operations must have explicit transaction management."""
        critical_files = [
            "core/llm/client.py",
            "core/git_for_data.py",
            "cli/mo_admin.py",
        ]

        for file_path in critical_files:
            full_path = Path(__file__).parent.parent.parent / file_path
            if not full_path.exists():
                continue

            content = full_path.read_text()

            # Check for INSERT/UPDATE/DELETE operations
            if re.search(r"(INSERT|UPDATE|DELETE)\s+", content, re.IGNORECASE):
                # Must have commit or rollback
                assert "commit()" in content or "rollback()" in content, (
                    f"{file_path} has write operations but no commit/rollback"
                )


class TestTransactionIsolation:
    """Test transaction isolation in concurrent scenarios."""

    def test_concurrent_inserts_with_separate_sessions(self):
        """Concurrent inserts with separate sessions should work."""
        from api.database import SessionLocal
        from sqlalchemy import text
        from uuid_utils import uuid7
        import threading

        errors = []

        def insert_config(thread_id):
            # Each thread gets its own session
            db = SessionLocal()
            try:
                config_id = str(uuid7())
                db.execute(
                    text("""
                    INSERT INTO infra_configs (config_id, key_name, scope_type, scope_user_id, value)
                    VALUES (:id, :key, :scope, :user, :val)
                """),
                    {
                        "id": config_id,
                        "key": f"test_concurrent_{thread_id}",
                        "scope": "global",
                        "user": None,
                        "val": "test",
                    },
                )
                db.commit()

                # Cleanup
                db.execute(
                    text("DELETE FROM infra_configs WHERE config_id = :id"), {"id": config_id}
                )
                db.commit()
            except Exception as e:
                db.rollback()
                errors.append(f"Thread {thread_id}: {e}")
            finally:
                db.close()

        threads = [threading.Thread(target=insert_config, args=(i,)) for i in range(5)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        assert len(errors) == 0, f"Concurrent insert errors: {errors}"
