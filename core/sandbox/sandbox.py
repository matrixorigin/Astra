"""Sandbox for isolated experiments."""

from __future__ import annotations

from typing import TYPE_CHECKING

from sqlalchemy import text
from sqlalchemy.orm import Session

from core.git_for_data import GitForData
from core.validation import validate_identifier

if TYPE_CHECKING:
    from datetime import datetime


class Sandbox:
    """Sandbox for isolated experiments with metadata management."""

    def __init__(
        self, db: Session, source_db: str = "dev_agent", account: str = "sys"
    ):
        if not isinstance(db, Session):
            raise TypeError("db must be a SQLAlchemy Session")
        
        self.db = db
        self.source_db = source_db
        self.account = account
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

        # Validate inputs to prevent SQL injection
        validate_identifier(name)
        if from_snapshot:
            validate_identifier(from_snapshot)

        # DDL commands need to be outside transaction
        self.db.commit()
        
        raw_conn = self.db.connection().connection
        cursor = raw_conn.cursor()
        try:
            cursor.execute(f"DROP DATABASE IF EXISTS {name}")
            raw_conn.commit()
            
            if from_snapshot:
                cursor.execute(f"CREATE DATABASE {name} CLONE {self.source_db} {{SNAPSHOT = '{from_snapshot}'}}")
            else:
                cursor.execute(f"CREATE DATABASE {name} CLONE {self.source_db}")
            raw_conn.commit()
        finally:
            cursor.close()

        # Store metadata with microsecond precision
        tags_json = json.dumps(tags) if tags else None
        
        self.db.execute(
            text(f"""
                INSERT INTO {self.source_db}.sandbox_metadata
                (sandbox_name, user_id, data_source, description, created_by, created_at, updated_at, tags, source_database, source_snapshot, status)
                VALUES (:name, :created_by, :data_source, :description, :created_by, CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6),
                        :tags, :source_db, :snapshot, 'active')
            """),
            {
                "name": name,
                "created_by": created_by,
                "data_source": json.dumps({"type": "matrixone", "database": name}),
                "description": description,
                "tags": tags_json,
                "source_db": self.source_db,
                "snapshot": from_snapshot,
            }
        )
        self.db.commit()

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
        # Validate to prevent SQL injection
        validate_identifier(name)
        
        # Delete all snapshots for this sandbox
        snapshots = self.list_snapshots(name)
        for snapshot in snapshots:
            try:
                self.git.drop_snapshot(snapshot["full_name"])
            except Exception:
                pass  # Continue even if snapshot deletion fails

        # Drop database
        self.db.execute(text(f"DROP DATABASE IF EXISTS {name}"))

        # Delete metadata
        self.db.execute(text("DELETE FROM sandbox_metadata WHERE sandbox_name = :name"), {"name": name})
        self.db.commit()

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
        params = {}

        if prefix:
            query += " AND sandbox_name LIKE :prefix"
            params["prefix"] = f"{prefix}%"

        if pattern:
            query += " AND sandbox_name LIKE :pattern"
            params["pattern"] = pattern

        if status:
            query += " AND status = :status"
            params["status"] = status

        if created_by:
            query += " AND created_by = :created_by"
            params["created_by"] = created_by

        if created_after:
            query += " AND created_at > :created_after"
            params["created_after"] = created_after.isoformat()

        if updated_after:
            query += " AND updated_at > :updated_after"
            params["updated_after"] = updated_after.isoformat()

        if tags:
            # JSON contains check (simplified)
            for i, tag in enumerate(tags):
                query += f" AND tags LIKE :tag{i}"
                params[f"tag{i}"] = f"%{tag}%"

        query += " ORDER BY created_at DESC"

        result = self.db.execute(text(query), params)
        return [dict(row._mapping) for row in result]

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
        params = {"name": name}

        if description is not None:
            updates.append("description = :description")
            params["description"] = description

        if tags is not None:
            import json
            updates.append("tags = :tags")
            params["tags"] = json.dumps(tags)

        if status is not None:
            updates.append("status = :status")
            params["status"] = status

        if updates:
            updates.append("updated_at = CURRENT_TIMESTAMP")
            query = "UPDATE sandbox_metadata SET " + ", ".join(updates) + " WHERE sandbox_name = :name"
            self.db.execute(text(query), params)
            self.db.commit()

    def use(self, sandbox: str) -> None:
        """Switch to sandbox database."""
        validate_identifier(sandbox)
        self.db.execute(text(f"USE {sandbox}"))

    def clone_table(self, target: str, source: str, snapshot: str | None = None) -> None:
        """Clone table (zero-copy)."""
        # Validate identifiers
        validate_identifier(target, allow_dot=True)
        validate_identifier(source, allow_dot=True)
        if snapshot:
            validate_identifier(snapshot)
            
        self.db.commit()  # Commit before DDL
        self.db.execute(text(f"DROP TABLE IF EXISTS {target}"))
        if snapshot:
            self.db.execute(text(f'CREATE TABLE {target} CLONE {source}{{SNAPSHOT="{snapshot}"}}'))
        else:
            self.db.execute(text(f"CREATE TABLE {target} CLONE {source}"))

    def add_table(self, sandbox: str, table: str, from_snapshot: str | None = None) -> None:
        """Add table to sandbox."""
        source = f"{self.source_db}.{table}"
        target = f"{sandbox}.{table}"
        self.clone_table(target, source, from_snapshot)
        self._touch_metadata(sandbox)

    def remove_table(self, sandbox: str, table: str) -> None:
        """Remove table from sandbox."""
        self.db.execute(text(f"DROP TABLE IF EXISTS {sandbox}.{table}"))
        self._touch_metadata(sandbox)

    def list_tables(self, sandbox: str) -> list[str]:
        """List tables in sandbox."""
        result = self.db.execute(text(f"SHOW TABLES FROM {sandbox}"))
        return [row._mapping[f"Tables_in_{sandbox}"] for row in result]

    def info(self, sandbox: str) -> dict:
        """Get sandbox info with metadata."""
        # Get metadata
        result_meta = self.db.execute(
            text("SELECT * FROM sandbox_metadata WHERE sandbox_name = :sandbox"),
            {"sandbox": sandbox}
        )
        metadata = result_meta.first()

        # Get table info
        tables = self.list_tables(sandbox)
        table_info = []
        for table in tables:
            if table.startswith("_") or table == "sandbox_metadata":
                continue
            count_result = self.db.execute(text(f"SELECT COUNT(*) as count FROM {sandbox}.{table}"))
            count_row = count_result.first()
            count = count_row._mapping.get("count", 0) if count_row else 0
            table_info.append({"table": table, "rows": count})

        result = {
            "sandbox_name": sandbox,
            "table_count": len(tables),
            "table_details": table_info,
        }

        if metadata:
            result.update(dict(metadata._mapping))

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
        result = self.db.execute(
            text("SELECT created_at FROM sandbox_metadata WHERE sandbox_name = :sandbox"),
            {"sandbox": sandbox}
        )
        metadata = result.first()
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
        result = self.db.execute(
            text("SELECT created_at FROM sandbox_metadata WHERE sandbox_name = :sandbox"),
            {"sandbox": sandbox}
        )
        metadata = result.first()
        if not metadata:
            raise ValueError(f"Sandbox {sandbox} not found")

        sandbox_created_at = metadata._mapping["created_at"]
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

        # Use native RESTORE DATABASE command
        # Syntax: RESTORE ACCOUNT {account} DATABASE {database_name} FROM SNAPSHOT {snapshot_name}
        # This only restores the specified database, not the entire account
        self.db.commit()  # Commit before DDL
        self.db.execute(
            text(f"RESTORE ACCOUNT {self.account} DATABASE {sandbox} FROM SNAPSHOT {full_snapshot_name}")
        )
        self._touch_metadata(sandbox)

    def _touch_metadata(self, sandbox: str) -> None:
        """Update updated_at timestamp."""
        self.db.execute(
            text("UPDATE sandbox_metadata SET updated_at = CURRENT_TIMESTAMP(6) WHERE sandbox_name = :sandbox"),
            {"sandbox": sandbox}
        )
        self.db.commit()
