"""Git for Data operations for MatrixOne.

Provides snapshot, restore, and time-travel capabilities.
"""

from datetime import datetime
from typing import Optional

from sdk.database import Database


class GitForData:
    """Git for Data operations manager.
    
    Provides MatrixOne's snapshot and time-travel capabilities.
    Based on MatrixOne v3.0+ Git for Data features.
    """

    def __init__(self, db: Optional[Database] = None) -> None:
        """Initialize Git for Data manager.
        
        Args:
            db: Database instance. If None, creates a new one.
        """
        self.db = db or Database()

    def create_snapshot(self, snapshot_name: str, account: str = "sys") -> dict:
        """Create a snapshot of the current database state.
        
        Args:
            snapshot_name: Name for the snapshot
            account: Account name (default: sys)
            
        Returns:
            dict: Snapshot metadata with name and timestamp
            
        Example:
            >>> git = GitForData()
            >>> snapshot = git.create_snapshot("before_experiment")
            >>> print(snapshot["snapshot_name"])
        """
        query = f"CREATE SNAPSHOT {snapshot_name} FOR ACCOUNT {account}"
        self.db.execute(query)

        # Get snapshot info
        snapshots = self.list_snapshots()
        snapshot_info = next(
            (s for s in snapshots if s["snapshot_name"] == snapshot_name), None
        )

        return snapshot_info or {"snapshot_name": snapshot_name, "timestamp": None}

    def list_snapshots(self) -> list[dict]:
        """List all available snapshots.
        
        Returns:
            list[dict]: List of snapshots with metadata
        """
        query = "SHOW SNAPSHOTS"
        rows = self.db.fetchall(query)
        return [
            {
                "snapshot_name": row["SNAPSHOT_NAME"],
                "timestamp": row["TIMESTAMP"],
                "snapshot_level": row["SNAPSHOT_LEVEL"],
                "account_name": row["ACCOUNT_NAME"],
                "database_name": row.get("DATABASE_NAME"),
                "table_name": row.get("TABLE_NAME"),
            }
            for row in rows
        ]

    def query_at_snapshot(
        self, query: str, snapshot_name: str, params: Optional[tuple] = None
    ) -> list[dict]:
        """Execute a query at a specific snapshot (time-travel query).
        
        This is a READ-ONLY operation that doesn't affect the current state.
        Uses MatrixOne's {SNAPSHOT = 'name'} syntax.
        
        Args:
            query: SQL query (must be SELECT)
            snapshot_name: Snapshot to query
            params: Optional query parameters
            
        Returns:
            list[dict]: Query results
            
        Example:
            >>> git = GitForData()
            >>> results = git.query_at_snapshot(
            ...     "SELECT * FROM conversation_events WHERE session_id = %s",
            ...     "my_checkpoint",
            ...     ("session_123",)
            ... )
        """
        # Inject snapshot syntax into query
        # Replace FROM table with FROM table {SNAPSHOT = 'name'}
        snapshot_clause = f"{{SNAPSHOT = '{snapshot_name}'}}"
        
        # Simple injection: add after first FROM clause
        # Note: This is a simplified implementation
        # Production code should use proper SQL parsing
        if "FROM" in query.upper():
            parts = query.split()
            result_parts = []
            for i, part in enumerate(parts):
                result_parts.append(part)
                if part.upper() == "FROM" and i + 1 < len(parts):
                    result_parts.append(parts[i + 1])
                    result_parts.append(snapshot_clause)
                    result_parts.extend(parts[i + 2:])
                    break
            modified_query = " ".join(result_parts)
        else:
            modified_query = query
        
        return self.db.fetchall(modified_query, params)

    def restore_from_snapshot(
        self, snapshot_name: str, account: str = "sys"
    ) -> None:
        """Restore database state from a snapshot.
        
        Args:
            snapshot_name: Name of the snapshot to restore
            account: Account name (default: sys)
            
        Warning:
            This operation will restore the entire account state.
            All changes after the snapshot will be lost.
        """
        query = f"RESTORE ACCOUNT {account} FROM SNAPSHOT {snapshot_name}"
        self.db.execute(query)

    def drop_snapshot(self, snapshot_name: str) -> None:
        """Delete a snapshot.
        
        Args:
            snapshot_name: Name of the snapshot to delete
        """
        query = f"DROP SNAPSHOT {snapshot_name}"
        self.db.execute(query)

    def get_snapshot_info(self, snapshot_name: str) -> Optional[dict]:
        """Get information about a specific snapshot.
        
        Args:
            snapshot_name: Name of the snapshot
            
        Returns:
            Optional[dict]: Snapshot metadata if found, None otherwise
        """
        snapshots = self.list_snapshots()
        return next(
            (s for s in snapshots if s["snapshot_name"] == snapshot_name), None
        )

    def create_time_point_sandbox(
        self, snapshot_name: str, description: Optional[str] = None
    ) -> dict:
        """Create a time-point sandbox for experimentation.
        
        This creates a snapshot that can be used for isolated experiments
        without affecting the main database state.
        
        Args:
            snapshot_name: Name for the sandbox snapshot (alphanumeric and underscore only)
            description: Optional description of the sandbox purpose
            
        Returns:
            dict: Sandbox metadata
        """
        # Sanitize snapshot name (remove special characters)
        sanitized_name = "".join(c if c.isalnum() or c == "_" else "_" for c in snapshot_name)
        snapshot = self.create_snapshot(sanitized_name)
        return {
            "snapshot_name": sanitized_name,
            "timestamp": snapshot.get("timestamp"),
            "description": description,
            "type": "sandbox",
        }

    def cleanup_old_snapshots(self, keep_count: int = 10) -> list[str]:
        """Clean up old snapshots, keeping only the most recent ones.
        
        Args:
            keep_count: Number of recent snapshots to keep
            
        Returns:
            list[str]: Names of deleted snapshots
        """
        snapshots = self.list_snapshots()

        # Sort by timestamp (newest first)
        snapshots.sort(
            key=lambda s: s["timestamp"] if s["timestamp"] else "", reverse=True
        )

        deleted = []
        for snapshot in snapshots[keep_count:]:
            snapshot_name = snapshot["snapshot_name"]
            try:
                self.drop_snapshot(snapshot_name)
                deleted.append(snapshot_name)
            except Exception:
                # Skip if deletion fails (e.g., snapshot in use)
                pass

        return deleted
