"""Advanced Sandbox using MatrixOne Git for Data capabilities.

Leverages CLONE, Snapshot, and PITR for efficient isolated experiments.
"""

from datetime import datetime
from typing import Optional

from sdk.database import Database
from sdk.git_for_data import GitForData


class AdvancedSandbox:
    """Advanced sandbox using MatrixOne's Git for Data features.
    
    Provides multiple isolation strategies:
    1. Database Clone - Zero-copy database duplication
    2. Snapshot-based - Point-in-time isolation
    3. PITR-based - Continuous time-travel capability
    """

    def __init__(
        self,
        source_database: str = "dev_agent",
        db: Optional[Database] = None,
    ) -> None:
        """Initialize advanced sandbox.
        
        Args:
            source_database: Source database name
            db: Database instance. If None, creates a new one.
        """
        self.source_database = source_database
        self.db = db or Database()
        self.git = GitForData(db)

    def create_clone_sandbox(
        self, sandbox_name: str, from_snapshot: Optional[str] = None
    ) -> dict:
        """Create a sandbox using database clone (zero-copy).
        
        This is the most efficient method - uses MatrixOne's zero-copy clone.
        
        Args:
            sandbox_name: Name for the sandbox database
            from_snapshot: Optional snapshot to clone from
            
        Returns:
            dict: Sandbox metadata
            
        Example:
            >>> sandbox = AdvancedSandbox()
            >>> sb = sandbox.create_clone_sandbox("exp_sandbox")
            >>> # Work in exp_sandbox database
            >>> sandbox.drop_clone_sandbox("exp_sandbox")
        """
        # Drop if exists
        self.drop_clone_sandbox(sandbox_name)
        
        if from_snapshot:
            # Clone from specific snapshot
            query = f"""
                CREATE DATABASE {sandbox_name} 
                CLONE {self.source_database} 
                {{SNAPSHOT = '{from_snapshot}'}}
            """
        else:
            # Clone current state (zero-copy)
            query = f"CREATE DATABASE {sandbox_name} CLONE {self.source_database}"

        self.db.execute(query)

        return {
            "sandbox_name": sandbox_name,
            "sandbox_type": "clone",
            "source_database": self.source_database,
            "from_snapshot": from_snapshot,
            "created_at": datetime.now(),
        }

    def drop_clone_sandbox(self, sandbox_name: str) -> None:
        """Drop a cloned sandbox database.
        
        Args:
            sandbox_name: Sandbox database name
        """
        query = f"DROP DATABASE IF EXISTS {sandbox_name}"
        self.db.execute(query)

    def list_databases(self) -> list[str]:
        """List all databases (including sandboxes).
        
        Returns:
            list[str]: Database names
        """
        query = "SHOW DATABASES"
        rows = self.db.fetchall(query)
        return [row["Database"] for row in rows]

    def create_pitr_enabled_sandbox(
        self, sandbox_name: str, retention_hours: int = 1
    ) -> dict:
        """Create a sandbox with PITR (Point-in-Time Recovery) enabled.
        
        This allows time-travel within the sandbox.
        
        Args:
            sandbox_name: Sandbox database name
            retention_hours: How long to retain history (hours)
            
        Returns:
            dict: Sandbox metadata with PITR info
        """
        # Create clone sandbox
        sandbox_info = self.create_clone_sandbox(sandbox_name)

        # Enable PITR for the sandbox database
        pitr_name = f"pitr_{sandbox_name}"
        query = f"CREATE PITR {pitr_name} FOR DATABASE {sandbox_name} RANGE {retention_hours} 'h'"
        self.db.execute(query)

        sandbox_info.update(
            {
                "pitr_enabled": True,
                "pitr_name": pitr_name,
                "retention_hours": retention_hours,
            }
        )

        return sandbox_info

    def restore_sandbox_to_timestamp(
        self, sandbox_name: str, pitr_name: str, target_time: str
    ) -> None:
        """Restore a sandbox database to a specific timestamp.
        
        Requires PITR to be enabled on the sandbox.
        
        Args:
            sandbox_name: Sandbox database name
            pitr_name: PITR name
            target_time: Target timestamp (format: 'YYYY-MM-DD HH:MM:SS')
        """
        query = f"""
            RESTORE DATABASE {sandbox_name} 
            FROM PITR {pitr_name} 
            TIMESTAMP '{target_time}'
        """
        self.db.execute(query)

    def query_sandbox_at_snapshot(
        self, sandbox_name: str, table_name: str, snapshot_name: str
    ) -> list[dict]:
        """Query sandbox data at a specific snapshot (read-only).
        
        Args:
            sandbox_name: Sandbox database name
            table_name: Table name
            snapshot_name: Snapshot name
            
        Returns:
            list[dict]: Query results
        """
        query = f"""
            SELECT * FROM {sandbox_name}.{table_name} 
            {{SNAPSHOT = '{snapshot_name}'}}
        """
        return self.db.fetchall(query)

    def run_isolated_experiment(
        self,
        experiment_name: str,
        experiment_fn: callable,
        cleanup: bool = True,
        from_snapshot: Optional[str] = None,
    ) -> dict:
        """Run an experiment in an isolated cloned sandbox.
        
        This is the recommended way to run experiments - uses zero-copy clone.
        
        Args:
            experiment_name: Experiment name
            experiment_fn: Function to execute (receives sandbox_name as parameter)
            cleanup: Whether to cleanup sandbox after experiment
            from_snapshot: Optional snapshot to start from
            
        Returns:
            dict: Experiment results
            
        Example:
            >>> def my_experiment(sandbox_name):
            ...     # Use sandbox_name database for operations
            ...     db.execute(f"USE {sandbox_name}")
            ...     # ... do experiments ...
            ...     return {"status": "success"}
            >>> 
            >>> sandbox = AdvancedSandbox()
            >>> result = sandbox.run_isolated_experiment("test", my_experiment)
        """
        # Generate unique sandbox name
        timestamp = str(int(datetime.now().timestamp()))
        sandbox_name = f"sandbox_{experiment_name}_{timestamp}".lower()

        try:
            # Create clone sandbox (zero-copy, fast)
            sandbox_info = self.create_clone_sandbox(sandbox_name, from_snapshot)

            # Run experiment
            result = experiment_fn(sandbox_name)

            return {
                "experiment_name": experiment_name,
                "sandbox_name": sandbox_name,
                "status": "success",
                "result": result,
                "sandbox_info": sandbox_info,
            }

        except Exception as e:
            return {
                "experiment_name": experiment_name,
                "sandbox_name": sandbox_name,
                "status": "failed",
                "error": str(e),
            }

        finally:
            # Cleanup
            if cleanup:
                self.drop_clone_sandbox(sandbox_name)

    def compare_sandbox_with_main(
        self, sandbox_name: str, table_name: str
    ) -> dict:
        """Compare data between sandbox and main database.
        
        Args:
            sandbox_name: Sandbox database name
            table_name: Table name to compare
            
        Returns:
            dict: Comparison results
        """
        # Count in main database
        main_query = f"SELECT COUNT(*) as count FROM {self.source_database}.{table_name}"
        main_count = self.db.fetchone(main_query)["count"]

        # Count in sandbox
        sandbox_query = f"SELECT COUNT(*) as count FROM {sandbox_name}.{table_name}"
        sandbox_count = self.db.fetchone(sandbox_query)["count"]

        return {
            "table": table_name,
            "main_count": main_count,
            "sandbox_count": sandbox_count,
            "difference": sandbox_count - main_count,
        }

    def create_snapshot_for_sandbox(self, sandbox_name: str, snapshot_name: str) -> dict:
        """Create a snapshot of a sandbox database.
        
        Args:
            sandbox_name: Sandbox database name
            snapshot_name: Snapshot name
            
        Returns:
            dict: Snapshot metadata
        """
        query = f"CREATE SNAPSHOT {snapshot_name} FOR DATABASE {sandbox_name}"
        self.db.execute(query)

        return {
            "snapshot_name": snapshot_name,
            "sandbox_name": sandbox_name,
            "created_at": datetime.now(),
        }

    # ========================================================================
    # Table-level Operations (P0)
    # ========================================================================

    def clone_table_to_sandbox(
        self, sandbox_name: str, table_name: str, new_table_name: Optional[str] = None
    ) -> dict:
        """Clone a specific table from main database to sandbox.
        
        Args:
            sandbox_name: Sandbox database name
            table_name: Table name in main database
            new_table_name: Optional new name in sandbox (default: same name)
            
        Returns:
            dict: Clone operation metadata
        """
        target_name = new_table_name or table_name
        query = f"""
            CREATE TABLE {sandbox_name}.{target_name} 
            CLONE {self.source_database}.{table_name}
        """
        self.db.execute(query)

        return {
            "sandbox_name": sandbox_name,
            "source_table": f"{self.source_database}.{table_name}",
            "target_table": f"{sandbox_name}.{target_name}",
            "operation": "clone_table",
            "created_at": datetime.now(),
        }

    def add_table_to_sandbox(
        self,
        sandbox_name: str,
        table_name: str,
        from_snapshot: Optional[str] = None,
        new_table_name: Optional[str] = None,
    ) -> dict:
        """Add a table to existing sandbox (from snapshot or current state).
        
        Args:
            sandbox_name: Sandbox database name
            table_name: Table name to add
            from_snapshot: Optional snapshot to clone from
            new_table_name: Optional new name in sandbox
            
        Returns:
            dict: Operation metadata
        """
        target_name = new_table_name or table_name

        if from_snapshot:
            query = f"""
                CREATE TABLE {sandbox_name}.{target_name} 
                CLONE {self.source_database}.{table_name}
                {{SNAPSHOT = '{from_snapshot}'}}
            """
        else:
            query = f"""
                CREATE TABLE {sandbox_name}.{target_name} 
                CLONE {self.source_database}.{table_name}
            """

        self.db.execute(query)

        return {
            "sandbox_name": sandbox_name,
            "table_name": target_name,
            "from_snapshot": from_snapshot,
            "operation": "add_table",
            "created_at": datetime.now(),
        }

    def remove_table_from_sandbox(self, sandbox_name: str, table_name: str) -> None:
        """Remove a table from sandbox.
        
        Args:
            sandbox_name: Sandbox database name
            table_name: Table name to remove
        """
        query = f"DROP TABLE IF EXISTS {sandbox_name}.{table_name}"
        self.db.execute(query)

    def list_sandbox_tables(self, sandbox_name: str) -> list[str]:
        """List all tables in a sandbox.
        
        Args:
            sandbox_name: Sandbox database name
            
        Returns:
            list[str]: Table names
        """
        query = f"SHOW TABLES FROM {sandbox_name}"
        rows = self.db.fetchall(query)
        return [row[f"Tables_in_{sandbox_name}"] for row in rows]

    # ========================================================================
    # Sandbox Management (P0)
    # ========================================================================

    def list_sandboxes(
        self, prefix: str = "sandbox_", include_metadata: bool = False
    ) -> list[dict]:
        """List all sandbox databases.
        
        Args:
            prefix: Sandbox name prefix to filter
            include_metadata: Whether to include detailed metadata
            
        Returns:
            list[dict]: Sandbox information
        """
        databases = self.list_databases()
        sandboxes = [db for db in databases if db.startswith(prefix)]

        if not include_metadata:
            return [{"sandbox_name": name} for name in sandboxes]

        # Get detailed info for each sandbox
        result = []
        for sandbox_name in sandboxes:
            info = self.get_sandbox_info(sandbox_name)
            result.append(info)

        return result

    def get_sandbox_info(self, sandbox_name: str) -> dict:
        """Get detailed information about a sandbox.
        
        Args:
            sandbox_name: Sandbox database name
            
        Returns:
            dict: Sandbox metadata
        """
        # Get table count
        tables = self.list_sandbox_tables(sandbox_name)

        # Get row counts for each table
        table_info = []
        for table in tables:
            count_query = f"SELECT COUNT(*) as count FROM {sandbox_name}.{table}"
            count = self.db.fetchone(count_query)["count"]
            table_info.append({"table": table, "row_count": count})

        return {
            "sandbox_name": sandbox_name,
            "table_count": len(tables),
            "tables": table_info,
            "source_database": self.source_database,
        }

    def update_sandbox_metadata(
        self, sandbox_name: str, description: str, tags: Optional[list[str]] = None
    ) -> dict:
        """Update sandbox metadata (description and tags).
        
        Note: This stores metadata in a special metadata table.
        
        Args:
            sandbox_name: Sandbox database name
            description: Sandbox description
            tags: Optional tags
            
        Returns:
            dict: Updated metadata
        """
        # Create metadata table if not exists
        self.db.execute(f"""
            CREATE TABLE IF NOT EXISTS {sandbox_name}._sandbox_metadata (
                meta_key VARCHAR(255),
                meta_value TEXT,
                updated_at TIMESTAMP,
                PRIMARY KEY (meta_key)
            )
        """)

        # Update description
        self.db.execute(
            f"""
            REPLACE INTO {sandbox_name}._sandbox_metadata (meta_key, meta_value, updated_at)
            VALUES ('description', %s, CURRENT_TIMESTAMP)
            """,
            (description,),
        )

        # Update tags
        if tags:
            import json

            tags_json = json.dumps(tags)
            self.db.execute(
                f"""
                REPLACE INTO {sandbox_name}._sandbox_metadata (meta_key, meta_value, updated_at)
                VALUES ('tags', %s, CURRENT_TIMESTAMP)
                """,
                (tags_json,),
            )

        return {
            "sandbox_name": sandbox_name,
            "description": description,
            "tags": tags,
            "updated_at": datetime.now(),
        }

    def get_sandbox_metadata(self, sandbox_name: str) -> dict:
        """Get sandbox metadata.
        
        Args:
            sandbox_name: Sandbox database name
            
        Returns:
            dict: Metadata
        """
        try:
            rows = self.db.fetchall(
                f"SELECT meta_key, meta_value FROM {sandbox_name}._sandbox_metadata"
            )
            metadata = {row["meta_key"]: row["meta_value"] for row in rows}

            # Parse tags if exists
            if "tags" in metadata:
                import json

                metadata["tags"] = json.loads(metadata["tags"])

            return metadata
        except Exception:
            return {}

    # ========================================================================
    # Sandbox History (P0)
    # ========================================================================

    def create_sandbox_checkpoint(
        self, sandbox_name: str, checkpoint_name: str, description: str = ""
    ) -> dict:
        """Create a checkpoint for a sandbox.
        
        Args:
            sandbox_name: Sandbox database name
            checkpoint_name: Checkpoint name
            description: Optional description
            
        Returns:
            dict: Checkpoint metadata
        """
        query = f"CREATE SNAPSHOT {checkpoint_name} FOR DATABASE {sandbox_name}"
        self.db.execute(query)

        # Store checkpoint metadata
        self.db.execute(f"""
            CREATE TABLE IF NOT EXISTS {sandbox_name}._sandbox_checkpoints (
                checkpoint_name VARCHAR(255) PRIMARY KEY,
                description TEXT,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
        """)

        self.db.execute(
            f"""
            INSERT INTO {sandbox_name}._sandbox_checkpoints 
            (checkpoint_name, description) VALUES (%s, %s)
            """,
            (checkpoint_name, description),
        )

        return {
            "sandbox_name": sandbox_name,
            "checkpoint_name": checkpoint_name,
            "description": description,
            "created_at": datetime.now(),
        }

    def list_sandbox_checkpoints(self, sandbox_name: str) -> list[dict]:
        """List all checkpoints for a sandbox.
        
        Args:
            sandbox_name: Sandbox database name
            
        Returns:
            list[dict]: Checkpoint list
        """
        try:
            rows = self.db.fetchall(
                f"""
                SELECT checkpoint_name, description, created_at 
                FROM {sandbox_name}._sandbox_checkpoints
                ORDER BY created_at DESC
                """
            )
            return [dict(row) for row in rows]
        except Exception:
            return []

    def restore_sandbox_to_checkpoint(
        self, sandbox_name: str, checkpoint_name: str
    ) -> None:
        """Restore sandbox to a checkpoint.
        
        Args:
            sandbox_name: Sandbox database name
            checkpoint_name: Checkpoint name
        """
        query = f"RESTORE DATABASE {sandbox_name} FROM SNAPSHOT {checkpoint_name}"
        self.db.execute(query)
