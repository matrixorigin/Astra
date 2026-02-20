"""DataContext — manages data environment lifecycle for code execution."""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum

from sqlalchemy import text
from sqlalchemy.orm import Session

from core.sandbox.sandbox import Sandbox


class DataAccessLevel(Enum):
    NONE = "none"       # No database access
    READ = "read"       # Clone with read-only queries
    WRITE = "write"     # Clone with read-write + auto-checkpoint


class DataContextScope(Enum):
    EXECUTION = "execution"  # Destroyed after single execution
    SESSION = "session"      # Persists across executions within session


@dataclass
class TableDiff:
    table: str
    added: int
    removed: int
    modified: int


@dataclass
class MergeResult:
    tables_merged: list[str]
    rows_applied: int


# DB user credentials for access control.
# In production, these are created once during deployment.
# READ user has SELECT only; WRITE user has full DML.
_DB_USERS = {
    DataAccessLevel.READ: {
        "user": "code_exec_ro",
        "password": "code_exec_ro_pass",
    },
    DataAccessLevel.WRITE: {
        "user": "code_exec_rw",
        "password": "code_exec_rw_pass",
    },
}


class DataContext:
    """Wraps Sandbox for code execution lifecycle.

    Provides DSN for the sandbox DB, checkpoint/restore, diff/merge.
    Access control is enforced at the DB user level (not AST).
    """

    def __init__(
        self,
        db: Session,
        sandbox: Sandbox,
        sandbox_name: str,
        access: DataAccessLevel,
        scope: DataContextScope,
        db_host: str = "localhost",
        db_port: int = 6001,
    ):
        self.db = db
        self.sandbox = sandbox
        self.sandbox_name = sandbox_name
        self.access = access
        self.scope = scope
        self.db_host = db_host
        self.db_port = db_port
        self._created = False
        self._checkpoint_name: str | None = None

    @property
    def dsn(self) -> str:
        """Connection string for the sandbox DB with access-appropriate user.

        READ access → code_exec_ro user (SELECT only)
        WRITE access → code_exec_rw user (full DML)
        """
        creds = _DB_USERS.get(self.access)
        if not creds:
            return self.sandbox_name
        return (
            f"mysql+pymysql://{creds['user']}:{creds['password']}"
            f"@{self.db_host}:{self.db_port}/{self.sandbox_name}"
        )

    @property
    def alive(self) -> bool:
        """Whether the sandbox DB still exists."""
        if not self._created:
            return False
        try:
            self.sandbox.info(self.sandbox_name)
            return True
        except Exception:
            return False

    def ensure_created(self) -> None:
        """Create sandbox DB if not yet created (idempotent).

        Also grants appropriate permissions to the access-level DB user.
        """
        if self._created:
            return
        self.sandbox.create(
            name=self.sandbox_name,
            description=f"code_exec ({self.access.value}/{self.scope.value})",
            created_by="code_executor",
        )
        self._grant_permissions()
        self._created = True

    def _grant_permissions(self) -> None:
        """Grant DB-level permissions based on access level."""
        try:
            if self.access == DataAccessLevel.READ:
                self.db.execute(text(
                    f"GRANT SELECT ON {self.sandbox_name}.* TO 'code_exec_ro'@'%'"
                ))
            elif self.access == DataAccessLevel.WRITE:
                self.db.execute(text(
                    f"GRANT SELECT, INSERT, UPDATE, DELETE, CREATE, DROP "
                    f"ON {self.sandbox_name}.* TO 'code_exec_rw'@'%'"
                ))
            self.db.commit()
        except Exception:
            pass  # User may not exist yet (dev mode) — fail open for MVP

    def checkpoint(self, name: str = "pre_exec") -> None:
        """SNAPSHOT current state. Only valid for WRITE access."""
        if self.access != DataAccessLevel.WRITE:
            raise RuntimeError("Checkpoint requires WRITE access")
        # Drop previous checkpoint if exists (keep only latest)
        if self._checkpoint_name and self._checkpoint_name != name:
            try:
                self.sandbox.git.drop_snapshot(f"{self.sandbox_name}_{self._checkpoint_name}")
            except Exception:
                pass
        self.sandbox.snapshot(self.sandbox_name, name)
        self._checkpoint_name = name

    def restore(self, name: str = "pre_exec") -> None:
        """RESTORE to checkpoint. Atomic rollback."""
        self.sandbox.restore(self.sandbox_name, name)

    def diff(self, tables: list[str] | None = None) -> list[TableDiff]:
        """Compare sandbox vs source using snapshot-based comparison."""
        if not self._checkpoint_name:
            return []

        target_tables = tables or self.sandbox.list_tables(self.sandbox_name)
        diffs: list[TableDiff] = []

        for table in target_tables:
            if table.startswith("_") or table == "sandbox_metadata":
                continue
            try:
                # Rows in sandbox but not in snapshot (added/modified)
                added = self.sandbox.git.diff(
                    f"{self.sandbox_name}.{table}",
                    f"{self.sandbox_name}.{table}",
                    source_snapshot=f"{self.sandbox_name}_{self._checkpoint_name}",
                    output="count",
                )
                # Rows in snapshot but not in sandbox (removed/modified)
                removed = self.sandbox.git.diff(
                    f"{self.sandbox_name}.{table}",
                    f"{self.sandbox_name}.{table}",
                    target_snapshot=f"{self.sandbox_name}_{self._checkpoint_name}",
                    output="count",
                )
                added_count = added[0].get("count", 0) if added else 0
                removed_count = removed[0].get("count", 0) if removed else 0

                if added_count > 0 or removed_count > 0:
                    diffs.append(TableDiff(
                        table=table,
                        added=added_count,
                        removed=removed_count,
                        modified=0,  # Precise modified count requires PK-based join
                    ))
            except Exception:
                continue  # Table may not exist in snapshot

        return diffs

    def merge(self, tables: list[str] | None = None) -> MergeResult:
        """Apply sandbox changes back to source DB.

        Uses Branch.merge() for each changed table. Only valid for WRITE access.
        This is the MERGE step of the Data PR workflow (CLONE → EXECUTE → DIFF → MERGE).
        """
        if self.access != DataAccessLevel.WRITE:
            raise RuntimeError("Merge requires WRITE access")

        diffs = self.diff(tables)
        if not diffs:
            return MergeResult(tables_merged=[], rows_applied=0)

        merged: list[str] = []
        total_rows = 0

        for d in diffs:
            try:
                source_table = f"{self.sandbox_name}.{d.table}"
                target_table = f"{self.sandbox.source_db}.{d.table}"
                self.sandbox.git.merge(source_table, target_table, on_conflict="accept")
                merged.append(d.table)
                total_rows += d.added + d.removed
            except Exception:
                continue

        return MergeResult(tables_merged=merged, rows_applied=total_rows)

    def destroy(self) -> None:
        """DROP sandbox database. Idempotent."""
        if not self._created:
            return
        try:
            self.sandbox.delete(self.sandbox_name)
        except Exception:
            pass
        self._created = False
