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
from core.db_consumer import DbConsumer, DbFactory


class DataAccessLevel(Enum):
    NONE = "none"  # No database access
    READ = "read"  # Direct source DB with read-only user
    WRITE = "write"  # Session-scoped sandbox with table-level branch


@dataclass
class TableDiff:
    table: str
    rows: list[dict]  # raw diff rows from `data branch diff`


@dataclass
class MergeResult:
    tables_merged: list[str]
    tables_failed: list[str]


class DataContext(DbConsumer):
    """Session-scoped sandbox with table-level zero-copy branch.

    - Creates empty sandbox DB on first use
    - Branches declared tables from source DB (zero-copy, LCA tracked by kernel)
    - diff/merge use native `data branch diff/merge` (three-way, conflict-aware)
    - Cleanup: `data branch delete` per table + DROP DATABASE
    """

    def __init__(
        self,
        db_factory: DbFactory,
        branch: Branch,
        sandbox_name: str,
        source_db: str,
        access: DataAccessLevel,
        session_id: str | None = None,
    ):
        super().__init__(db_factory)
        self.branch = branch
        self.sandbox_name = sandbox_name
        self.source_db = source_db
        self.access = access
        self.session_id = session_id
        self._created = False
        self._branched_tables: set[str] = set()

    @property
    def dsn(self) -> str:
        with self._db() as db:
            url = db.get_bind().url
            return (
                f"mysql+pymysql://{url.username}:{url.password}"
                f"@{url.host}:{url.port}/{self.sandbox_name}"
            )

    @property
    def alive(self) -> bool:
        return self._created

    def ensure_created(self) -> None:
        """Create empty sandbox DB and register metadata (idempotent)."""
        with self._db() as db:
            if self._created:
                return
            db.commit()
            db.execute(text(f"CREATE DATABASE IF NOT EXISTS {self.sandbox_name}"))
            db.commit()
            # Register in infra_sandbox_metadata for cleanup tracking
            try:
                import json

                db.execute(
                    text(f"""
                        INSERT INTO {self.source_db}.infra_sandbox_metadata
                        (sandbox_name, user_id, data_source, description, created_by,
                         created_at, updated_at, tags, source_database, status, session_id)
                        VALUES (:name, 'system', :ds, 'code_executor sandbox', 'system',
                                CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6),
                                NULL, :src, 'active', :sid)
                    """),
                    {
                        "name": self.sandbox_name,
                        "ds": json.dumps({"type": "matrixone", "database": self.sandbox_name}),
                        "src": self.source_db,
                        "sid": self.session_id,
                    },
                )
                db.commit()
            except Exception:
                # Metadata already exists (idempotent) or table missing
                db.rollback()
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

    def merge(self, tables: list[str] | None = None, on_conflict: str = "skip") -> MergeResult:
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
        """Clean up: data branch delete per table, then DROP DATABASE + metadata."""
        with self._db() as db:
            if not self._created:
                return
            try:
                for table in self._branched_tables:
                    try:
                        self.branch.delete(f"{self.sandbox_name}.{table}")
                    except Exception:
                        pass
                db.commit()
                db.execute(text(f"DROP DATABASE IF EXISTS {self.sandbox_name}"))
                db.commit()
                # Clean metadata
                try:
                    db.execute(
                        text(
                            f"DELETE FROM {self.source_db}.infra_sandbox_metadata WHERE sandbox_name = :n"
                        ),
                        {"n": self.sandbox_name},
                    )
                    db.commit()
                except Exception:
                    pass
            except Exception:
                pass
            self._created = False
            self._branched_tables.clear()
