"""Sandbox for isolated experiments."""

from __future__ import annotations
from typing import Optional

from sdk.database import Database
from sdk.git_for_data import GitForData


class Sandbox:
    """Sandbox for isolated experiments."""

    def __init__(self, source_db: str = "dev_agent", db: Optional[Database] = None):
        self.source_db = source_db
        self.db = db or Database()
        self.git = GitForData(self.db)

    def create(self, name: str, from_snapshot: Optional[str] = None) -> None:
        """Create sandbox."""
        self.db.execute(f"DROP DATABASE IF EXISTS {name}")
        
        if from_snapshot:
            self.db.execute(f"CREATE DATABASE {name} CLONE {self.source_db} {{SNAPSHOT = '{from_snapshot}'}}")
        else:
            self.db.execute(f"CREATE DATABASE {name} CLONE {self.source_db}")

    def delete(self, name: str) -> None:
        """Delete sandbox."""
        self.db.execute(f"DROP DATABASE IF EXISTS {name}")

    def list(self, prefix: str = "sandbox_", pattern: Optional[str] = None) -> list[dict]:
        """List sandboxes with optional filtering.
        
        Args:
            prefix: Name prefix filter
            pattern: SQL LIKE pattern (e.g., "%exp%")
            
        Returns:
            list[dict]: Sandbox info with name and table count
        """
        rows = self.db.fetchall("SHOW DATABASES")
        dbs = [row["Database"] for row in rows]
        
        # Filter by prefix
        sandboxes = [db for db in dbs if db.startswith(prefix)]
        
        # Filter by pattern
        if pattern:
            sandboxes = [db for db in sandboxes if pattern.replace("%", "") in db]
        
        # Return with basic info
        result = []
        for name in sandboxes:
            try:
                table_count = len(self.list_tables(name))
                result.append({"name": name, "tables": table_count})
            except Exception:
                result.append({"name": name, "tables": 0})
        
        return result
    
    def use(self, sandbox: str) -> None:
        """Switch to sandbox database.
        
        After calling this, all queries will run in the sandbox.
        Call use(source_db) to switch back to main database.
        
        Args:
            sandbox: Sandbox name
            
        Example:
            >>> sandbox.use("exp1")
            >>> db.execute("SELECT * FROM events")  # Queries exp1.events
            >>> sandbox.use("dev_agent")  # Switch back to main
        """
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
    
    def remove_table(self, sandbox: str, table: str) -> None:
        """Remove table from sandbox."""
        self.db.execute(f"DROP TABLE IF EXISTS {sandbox}.{table}")
    
    def list_tables(self, sandbox: str) -> list[str]:
        """List tables in sandbox."""
        rows = self.db.fetchall(f"SHOW TABLES FROM {sandbox}")
        return [row[f"Tables_in_{sandbox}"] for row in rows]
    
    def info(self, sandbox: str) -> dict:
        """Get sandbox info."""
        tables = self.list_tables(sandbox)
        table_info = []
        for table in tables:
            if table.startswith("_"):
                continue
            count = self.db.fetchone(f"SELECT COUNT(*) as count FROM {sandbox}.{table}")["count"]
            table_info.append({"table": table, "rows": count})
        
        return {
            "name": sandbox,
            "tables": len(tables),
            "details": table_info,
        }
    
    def checkpoint(self, sandbox: str, name: str) -> None:
        """Create checkpoint for sandbox."""
        self.git.create_snapshot(f"{sandbox}_{name}")
    
    def list_checkpoints(self, sandbox: str) -> list[str]:
        """List checkpoints for sandbox."""
        snapshots = self.git.list_snapshots()
        prefix = f"{sandbox}_"
        return [s["snapshot_name"].replace(prefix, "") for s in snapshots if s["snapshot_name"].startswith(prefix)]
    
    def restore(self, sandbox: str, checkpoint: str) -> None:
        """Restore sandbox to checkpoint."""
        snapshot_name = f"{sandbox}_{checkpoint}"
        self.delete(sandbox)
        self.create(sandbox, from_snapshot=snapshot_name)
