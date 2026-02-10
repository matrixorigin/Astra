"""Sandbox for isolated experiments."""

from __future__ import annotations
from datetime import datetime
from typing import Optional

from sdk.database import Database
from sdk.git_for_data import GitForData


class Sandbox:
    """Sandbox for isolated experiments with metadata management."""

    def __init__(
        self, 
        source_db: str = "dev_agent", 
        account: str = "sys",
        db: Optional[Database] = None
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
        tags: Optional[list[str]] = None,
        from_snapshot: Optional[str] = None
    ) -> None:
        """Create sandbox with metadata."""
        import json
        
        self.db.execute(f"DROP DATABASE IF EXISTS {name}")
        
        if from_snapshot:
            self.db.execute(f"CREATE DATABASE {name} CLONE {self.source_db} {{SNAPSHOT = '{from_snapshot}'}}")
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
        """Delete sandbox, its snapshots, and metadata atomically.
        
        Deletion order (for safety):
        1. Delete metadata first (mark as deleted)
        2. Delete snapshots (best effort, continue on error)
        3. Drop database (atomic operation)
        
        If database drop fails, metadata is already deleted, preventing
        the sandbox from being used even if database still exists.
        
        Args:
            name: Sandbox database name
        """
        # Step 1: Delete metadata first (atomic, marks sandbox as deleted)
        # This prevents the sandbox from being used even if later steps fail
        self.db.execute(f"DELETE FROM sandbox_metadata WHERE sandbox_name = '{name}'")
        
        # Step 2: Delete all snapshots (best effort)
        # Continue even if some snapshots fail to delete
        try:
            snapshots = self.git.list_snapshots()
            prefix = f"{name}_"
            for s in snapshots:
                if s["snapshot_name"].startswith(prefix):
                    try:
                        self.git.drop_snapshot(s["snapshot_name"])
                    except Exception as e:
                        # Log but continue - don't fail entire delete for snapshot errors
                        print(f"Warning: Failed to delete snapshot {s['snapshot_name']}: {e}")
        except Exception as e:
            # If listing snapshots fails, continue to database deletion
            print(f"Warning: Failed to list snapshots for cleanup: {e}")
        
        # Step 3: Drop database (atomic operation by MatrixOne)
        # This is the final step - if it fails, sandbox is already marked deleted
        self.db.execute(f"DROP DATABASE IF EXISTS {name}")

    def list(
        self,
        prefix: str = "sandbox_",
        pattern: Optional[str] = None,
        status: Optional[str] = None,
        created_by: Optional[str] = None,
        created_after: Optional[datetime] = None,
        updated_after: Optional[datetime] = None,
        tags: Optional[list[str]] = None,
    ) -> list[dict]:
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
        description: Optional[str] = None,
        tags: Optional[list[str]] = None,
        status: Optional[str] = None,
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
            query = f"UPDATE sandbox_metadata SET {', '.join(updates)} WHERE sandbox_name = '{name}'"
            self.db.execute(query)
    
    def use(self, sandbox: str) -> None:
        """Switch to sandbox database."""
        self.db.execute(f"USE {sandbox}")
    
    def clone_table(self, target: str, source: str, snapshot: Optional[str] = None) -> None:
        """Clone table (zero-copy)."""
        if snapshot:
            self.db.execute(f'CREATE TABLE {target} CLONE {source}{{SNAPSHOT="{snapshot}"}}')
        else:
            self.db.execute(f"CREATE TABLE {target} CLONE {source}")
    
    def add_table(self, sandbox: str, table: str, from_snapshot: Optional[str] = None) -> None:
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
        metadata = self.db.fetchone(f"SELECT * FROM sandbox_metadata WHERE sandbox_name = '{sandbox}'")
        
        # Get table info
        tables = self.list_tables(sandbox)
        table_info = []
        for table in tables:
            if table.startswith("_") or table == "sandbox_metadata":
                continue
            count = self.db.fetchone(f"SELECT COUNT(*) as count FROM {sandbox}.{table}")["count"]
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
        """Create checkpoint for sandbox.
        
        Checkpoint timestamp must not exceed sandbox creation time.
        """
        # Get sandbox creation time
        metadata = self.db.fetchone(f"SELECT created_at FROM sandbox_metadata WHERE sandbox_name = '{sandbox}'")
        if not metadata:
            raise ValueError(f"Sandbox {sandbox} not found")
        
        snapshot_name = f"{sandbox}_{name}"
        self.git.create_snapshot(snapshot_name)
        self._touch_metadata(sandbox)
    
    def list_snapshots(self, sandbox: str) -> list[dict]:
        """List checkpoints for sandbox with timestamps."""
        snapshots = self.git.list_snapshots()
        prefix = f"{sandbox}_"
        
        result = []
        for s in snapshots:
            if s["snapshot_name"].startswith(prefix):
                result.append({
                    "name": s["snapshot_name"].replace(prefix, ""),
                    "full_name": s["snapshot_name"],
                    "created_at": s.get("ts", ""),
                })
        return result
    
    def restore(self, sandbox: str, checkpoint: str) -> None:
        """Restore sandbox to checkpoint using native RESTORE.
        
        Validates that checkpoint timestamp <= sandbox creation time.
        """
        # Get sandbox metadata
        metadata = self.db.fetchone(f"SELECT created_at FROM sandbox_metadata WHERE sandbox_name = '{sandbox}'")
        if not metadata:
            raise ValueError(f"Sandbox {sandbox} not found")
        
        sandbox_created_at = metadata["created_at"]
        snapshot_name = f"{sandbox}_{checkpoint}"
        
        # Get snapshot info
        snapshots = self.git.list_snapshots()
        snapshot_info = None
        for s in snapshots:
            if s["snapshot_name"] == snapshot_name:
                snapshot_info = s
                break
        
        if not snapshot_info:
            raise ValueError(f"Checkpoint {checkpoint} not found")
        
        # Validate checkpoint time <= sandbox creation time
        snapshot_ts = snapshot_info.get("ts", "")
        if snapshot_ts and snapshot_ts > str(sandbox_created_at):
            raise ValueError(
                f"Cannot restore: checkpoint time ({snapshot_ts}) is after sandbox creation time ({sandbox_created_at})"
            )
        
        # Use native RESTORE DATABASE
        # Syntax: RESTORE ACCOUNT {account} DATABASE {sandbox} FROM SNAPSHOT {snapshot}
        self.db.execute(f'RESTORE ACCOUNT {self.account} DATABASE {sandbox} FROM SNAPSHOT {snapshot_name}')
        self._touch_metadata(sandbox)
    
    def _touch_metadata(self, sandbox: str) -> None:
        """Update updated_at timestamp."""
        self.db.execute(f"UPDATE sandbox_metadata SET updated_at = CURRENT_TIMESTAMP(6) WHERE sandbox_name = '{sandbox}'")
