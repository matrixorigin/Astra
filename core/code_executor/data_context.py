"""DataContext — manages data environment lifecycle for code execution.

Session-scoped. Table-level zero-copy branch. No snapshots needed.
Uses MatrixOne's native `data branch` for create/diff/merge/delete.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum

from sqlalchemy import text
from sqlalchemy.orm import Session

from core.sandbox.branch import Branch


class DataAccessLevel(Enum):
    NONE = "none"       # No database access
    READ = "read"       # Direct source DB with read-only user
    WRITE = "write"     # Session-scoped sandbox with table-level branch


@dataclass
class TableDiff:
    table: str
    rows: list[dict]   # raw diff rows from `data branch diff`


@dataclass
class MergeResult:
    tables_merged: list[str]
    tables_failed: list[str]


# DB user credentials for access control.
_DB_USERS = {
    DataAccessLevel.READ: {"user": "code_exec_ro", "password": "code_exec_ro_pass"},
    DataAccessLevel.WRITE: {"user": "code_exec_rw", "password": "code_exec_rw_pass"},
}


class DataContext:
    """Session-scoped sandbox with table-level zero-copy branch.

    - Creates empty sandbox DB on first use
    - Branches declared tables from source DB (zero-copy, LCA tracked by kernel)
    - diff/merge use native `data branch diff/merge` (three-way, conflict-aware)
    - Cleanup: `data branch delete` per table + DROP DATABASE
    """

    def __init__(
        self,
        db: Session,
        branch: Branch,
        sandbox_name: str,
        source_db: str,
        access: DataAccessLevel,
        db_host: str = "localhost",
        db_port: int = 6001,
    ):
        self.db = db
        self.branch = branch
        self.sandbox_name = sandbox_name
        self.source_db = source_db
        self.access = access
        self.db_host = db_host
        self.db_port = db_port
        self._created = False
        self._branched_tables: set[str] = set()

    @property
    def dsn(self) -> str:
        creds = _DB_USERS.get(self.access)
        if not creds:
            return self.sandbox_name
        return (
            f"mysql+pymysql://{creds['user']}:{creds['password']}"
            f"@{self.db_host}:{self.db_port}/{self.sandbox_name}"
        )

    @property
    def alive(self) -> bool:
        return self._created

    def ensure_created(self) -> None:
        """Create empty sandbox DB (idempotent)."""
        if self._created:
            return
        self.db.commit()
        self.db.execute(text(f"CREATE DATABASE IF NOT EXISTS {self.sandbox_name}"))
        self.db.commit()
        self._grant_permissions()
        self._created = True

    def ensure_tables(self, tables: list[str]) -> None:
        """Branch declared tables into sandbox (zero-copy, idempotent per table).

        Uses `data branch create table` — kernel tracks LCA automatically.
        """
        for table in tables:
            if table not in self._branched_tables:
                self.branch.create(
                    name=f"{self.sandbox_name}.{table}",
                    source=f"{self.source_db}.{table}",
                )
                self._branched_tables.add(table)

    def diff(self, tables: list[str] | None = None) -> list[TableDiff]:
        """Diff sandbox tables against source using native data branch diff.

        Three-way diff with automatic LCA detection by kernel.
        """
        target_tables = tables or list(self._branched_tables)
        diffs: list[TableDiff] = []
        for table in target_tables:
            try:
                rows = self.branch.diff(
                    target=f"{self.sandbox_name}.{table}",
                    source=f"{self.source_db}.{table}",
                )
                if rows:
                    diffs.append(TableDiff(table=table, rows=rows))
            except Exception:
                continue
        return diffs

    def merge(
        self, tables: list[str] | None = None, on_conflict: str = "skip"
    ) -> MergeResult:
        """Merge sandbox changes back to source using native data branch merge."""
        if self.access != DataAccessLevel.WRITE:
            raise RuntimeError("Merge requires WRITE access")

        target_tables = tables or list(self._branched_tables)
        merged: list[str] = []
        failed: list[str] = []
        for table in target_tables:
            try:
                self.branch.merge(
                    source=f"{self.sandbox_name}.{table}",
                    target=f"{self.source_db}.{table}",
                    on_conflict=on_conflict,
                )
                merged.append(table)
            except Exception:
                failed.append(table)
        return MergeResult(tables_merged=merged, tables_failed=failed)

    def destroy(self) -> None:
        """Clean up: data branch delete per table, then DROP DATABASE."""
        if not self._created:
            return
        try:
            # Delete branch metadata per table
            for table in self._branched_tables:
                try:
                    self.branch.delete(f"{self.sandbox_name}.{table}")
                except Exception:
                    pass
            # Drop the sandbox database
            self.db.commit()
            self.db.execute(text(f"DROP DATABASE IF EXISTS {self.sandbox_name}"))
            self.db.commit()
        except Exception:
            pass
        self._created = False
        self._branched_tables.clear()

    def _grant_permissions(self) -> None:
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
            pass
