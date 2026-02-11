"""Sandbox for isolated experiments."""

from __future__ import annotations

from typing import TYPE_CHECKING

from sdk.database import Database
from sdk.git_for_data import GitForData

if TYPE_CHECKING:
    from datetime import datetime


class Sandbox:
    """Sandbox for isolated experiments with metadata management."""

    def __init__(
        self, source_db: str = "dev_agent", account: str = "sys", db: Database | None = None
    ):
        self.source_db = source_db
        self.account = account
        self.db = db or Database()
        self.git = GitForData(self.db)

    def create(
        self,
        name: str,
        description: str = "",
        created_by: str = "system",
        tags: list[str] | None = None,
        from_snapshot: str | None = None,
    ) -> None:
        """Create sandbox with metadata."""
        import json

        self.db.execute(f"DROP DATABASE IF EXISTS {name}")

        if from_snapshot:
            self.db.execute(
                f"CREATE DATABASE {name} CLONE {self.source_db} {{SNAPSHOT = '{from_snapshot}'}}"
            )
        else:
            self.db.execute(f"CREATE DATABASE {name} CLONE {self.source_db}")

        # Store metadata with microsecond precision
        tags_json = f"'{json.dumps(tags)}'" if tags else "NULL"
        snapshot_val = f"'{from_snapshot}'" if from_snapshot else "NULL"

        self.db.execute(f"""
            INSERT INTO sandbox_metadata
            (sandbox_name, description, created_by, created_at, updated_at, tags, source_database, source_snapshot, status)
            VALUES ('{name}', '{description}', '{created_by}', CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6),
                    {tags_json}, '{self.source_db}', {snapshot_val}, 'active')
        """)

    def delete(self, name: str) -> None:
        """Delete sandbox and all its snapshots.

        This will:
        1. Drop the sandbox database
        2. Delete all snapshots associated with this sandbox
        3. Remove metadata entry

        Args:
            name: Sandbox database name

        Example:
            >>> sandbox.delete("exp1")
            >>> # Deletes: exp1 database + all exp1_* snapshots + metadata
        """
        # Delete all snapshots for this sandbox
        snapshots = self.list_snapshots(name)
        for snapshot in snapshots:
            try:
                self.git.drop_snapshot(snapshot["full_name"])
            except Exception:
                pass  # Continue even if snapshot deletion fails

        # Drop database
        self.db.execute(f"DROP DATABASE IF EXISTS {name}")

        # Delete metadata
        self.db.execute(f"DELETE FROM sandbox_metadata WHERE sandbox_name = '{name}'")

    def list_sandboxes(
        self,
        prefix: str = "sandbox_",
        pattern: str | None = None,
        status: str | None = None,
        created_by: str | None = None,
        created_after: datetime | None = None,
        updated_after: datetime | None = None,
        tags: list[str] | None = None,
    ) -> list[dict[str, str]]:
        """List sandboxes with filtering.

        Args:
            prefix: Name prefix filter
            pattern: SQL LIKE pattern (e.g., "%exp%")
            status: Filter by status (active, archived, expired)
            created_by: Filter by creator
            created_after: Filter by creation time
            updated_after: Filter by update time
            tags: Filter by tags (any match)

        Returns:
            list[dict]: Sandbox metadata
        """
        query = "SELECT * FROM sandbox_metadata WHERE 1=1"

        if prefix:
            query += f" AND sandbox_name LIKE '{prefix}%'"

        if pattern:
            query += f" AND sandbox_name LIKE '{pattern}'"

        if status:
            query += f" AND status = '{status}'"

        if created_by:
            query += f" AND created_by = '{created_by}'"

        if created_after:
            query += f" AND created_at > '{created_after.isoformat()}'"

        if updated_after:
            query += f" AND updated_at > '{updated_after.isoformat()}'"

        if tags:
            # JSON contains check (simplified)
            for tag in tags:
                query += f" AND tags LIKE '%{tag}%'"

        query += " ORDER BY created_at DESC"

        return self.db.fetchall(query)

    def update(
        self,
        name: str,
        description: str | None = None,
        tags: list[str] | None = None,
        status: str | None = None,
    ) -> None:
        """Update sandbox metadata.

        Args:
            name: Sandbox name
            description: New description
            tags: New tags
            status: New status (active, archived, expired)
        """
        updates = []

        if description is not None:
            updates.append(f"description = '{description}'")

        if tags is not None:
            import json

            tags_json = json.dumps(tags)
            updates.append(f"tags = '{tags_json}'")

        if status is not None:
            updates.append(f"status = '{status}'")

        if updates:
            updates.append("updated_at = CURRENT_TIMESTAMP")
            query = (
                f"UPDATE sandbox_metadata SET {', '.join(updates)} WHERE sandbox_name = '{name}'"
            )
            self.db.execute(query)

    def use(self, sandbox: str) -> None:
        """Switch to sandbox database."""
        self.db.execute(f"USE {sandbox}")

    def clone_table(self, target: str, source: str, snapshot: str | None = None) -> None:
        """Clone table (zero-copy)."""
        if snapshot:
            self.db.execute(f'CREATE TABLE {target} CLONE {source}{{SNAPSHOT="{snapshot}"}}')
        else:
            self.db.execute(f"CREATE TABLE {target} CLONE {source}")

    def add_table(self, sandbox: str, table: str, from_snapshot: str | None = None) -> None:
        """Add table to sandbox."""
        source = f"{self.source_db}.{table}"
        target = f"{sandbox}.{table}"
        self.clone_table(target, source, from_snapshot)
        self._touch_metadata(sandbox)

    def remove_table(self, sandbox: str, table: str) -> None:
        """Remove table from sandbox."""
        self.db.execute(f"DROP TABLE IF EXISTS {sandbox}.{table}")
        self._touch_metadata(sandbox)

    def list_tables(self, sandbox: str) -> list[str]:
        """List tables in sandbox."""
        rows = self.db.fetchall(f"SHOW TABLES FROM {sandbox}")
        return [row[f"Tables_in_{sandbox}"] for row in rows]

    def info(self, sandbox: str) -> dict:
        """Get sandbox info with metadata."""
        # Get metadata
        metadata = self.db.fetchone(
            f"SELECT * FROM sandbox_metadata WHERE sandbox_name = '{sandbox}'"
        )

        # Get table info
        tables = self.list_tables(sandbox)
        table_info = []
        for table in tables:
            if table.startswith("_") or table == "sandbox_metadata":
                continue
            count_row = self.db.fetchone(f"SELECT COUNT(*) as count FROM {sandbox}.{table}")
            count = count_row.get("count", 0) if count_row else 0
            table_info.append({"table": table, "rows": count})

        result = {
            "sandbox_name": sandbox,
            "table_count": len(tables),
            "table_details": table_info,
        }

        if metadata:
            result.update(metadata)

        return result

    def snapshot(self, sandbox: str, name: str, description: str = "") -> None:
        """Create snapshot for sandbox.

        Creates a named snapshot of the current sandbox state for later restore.
        Snapshot name format: {sandbox_name}_{snapshot_name}

        Args:
            sandbox: Sandbox database name
            name: Snapshot name (will be prefixed with sandbox name)
            description: Optional description

        Example:
            >>> sandbox.snapshot("exp1", "before_test")
            >>> # Creates snapshot: exp1_before_test
        """
        # Verify sandbox exists
        metadata = self.db.fetchone(
            f"SELECT created_at FROM sandbox_metadata WHERE sandbox_name = '{sandbox}'"
        )
        if not metadata:
            raise ValueError(f"Sandbox {sandbox} not found")

        # Create snapshot with prefixed name
        snapshot_name = f"{sandbox}_{name}"
        self.git.create_snapshot(snapshot_name)
        self._touch_metadata(sandbox)

    def list_snapshots(self, sandbox: str) -> list[dict]:
        """List all snapshots for a sandbox.

        Args:
            sandbox: Sandbox database name

        Returns:
            list[dict]: Snapshot info with name and timestamp

        Example:
            >>> snapshots = sandbox.list_snapshots("exp1")
            >>> # [{"name": "before_test", "full_name": "exp1_before_test", "created_at": "..."}]
        """
        all_snapshots = self.git.list_snapshots()
        if not all_snapshots:
            return []
        prefix = f"{sandbox}_"

        result = []
        for s in all_snapshots:
            if s["snapshot_name"].startswith(prefix):
                result.append(
                    {
                        "name": s["snapshot_name"].replace(prefix, ""),
                        "full_name": s["snapshot_name"],
                        "created_at": s.get("ts", ""),
                    }
                )
        return result

    def restore(self, sandbox: str, snapshot_name: str) -> None:
        """Restore sandbox to a previous snapshot.

        Uses MatrixOne's native RESTORE DATABASE command to atomically restore
        the sandbox database to a previous snapshot state. This operation:
        - Only affects the specified sandbox database
        - Does not affect main database or other sandboxes
        - Is atomic and instant (no data copy)
        - Validates snapshot timestamp <= sandbox creation time

        Args:
            sandbox: Sandbox database name to restore
            snapshot_name: Snapshot name (without sandbox prefix)

        Raises:
            ValueError: If sandbox or snapshot not found
            ValueError: If snapshot timestamp > sandbox creation time

        Example:
            >>> sandbox.snapshot("exp1", "before_test")
            >>> # ... make changes ...
            >>> sandbox.restore("exp1", "before_test")  # Restore to snapshot
        """
        # Get sandbox metadata
        metadata = self.db.fetchone(
            f"SELECT created_at FROM sandbox_metadata WHERE sandbox_name = '{sandbox}'"
        )
        if not metadata:
            raise ValueError(f"Sandbox {sandbox} not found")

        sandbox_created_at = metadata["created_at"]
        full_snapshot_name = f"{sandbox}_{snapshot_name}"

        # Get snapshot info
        snapshots = self.git.list_snapshots()
        snapshot_info = None
        for s in snapshots:
            if s["snapshot_name"] == full_snapshot_name:
                snapshot_info = s
                break

        if not snapshot_info:
            raise ValueError(f"Snapshot {snapshot_name} not found for sandbox {sandbox}")

        # Validate snapshot time <= sandbox creation time
        # This prevents restoring to a snapshot created after the sandbox
        snapshot_ts = snapshot_info.get("ts", "")
        if snapshot_ts and snapshot_ts > str(sandbox_created_at):
            raise ValueError(
                f"Cannot restore: snapshot time ({snapshot_ts}) is after sandbox creation time ({sandbox_created_at})"
            )

        # Use native RESTORE DATABASE command
        # Syntax: RESTORE ACCOUNT {account} DATABASE {database_name} FROM SNAPSHOT {snapshot_name}
        # This only restores the specified database, not the entire account
        self.db.execute(
            f"RESTORE ACCOUNT {self.account} DATABASE {sandbox} FROM SNAPSHOT {full_snapshot_name}"
        )
        self._touch_metadata(sandbox)

    def _touch_metadata(self, sandbox: str) -> None:
        """Update updated_at timestamp."""
        self.db.execute(
            f"UPDATE sandbox_metadata SET updated_at = CURRENT_TIMESTAMP(6) WHERE sandbox_name = '{sandbox}'"
        )
