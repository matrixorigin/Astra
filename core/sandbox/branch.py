"""Branch manager for Git-like data workflows using MatrixOne's native data branch."""

import time

from sqlalchemy import text

from core.db_consumer import DbConsumer, DbFactory


class Branch(DbConsumer):
    """Branch manager using MatrixOne's native data branch commands.

    Supports zero-copy branching with automatic LCA tracking,
    three-way diff, and merge with conflict strategies.
    """

    def __init__(self, db_factory: DbFactory, database: str = "dev_agent"):
        super().__init__(db_factory)
        self.database = database

    def _qualify(self, name: str) -> str:
        if "." not in name:
            return f"{self.database}.{name}"
        return name

    def _wait_until_table_visible(
        self,
        qualified_name: str,
        *,
        attempts: int = 10,
        base_delay_s: float = 0.05,
    ) -> None:
        database, table = qualified_name.split(".", 1)
        from api.database import SessionLocal

        last_error: Exception | None = None
        for attempt in range(attempts):
            fresh_db = SessionLocal()
            try:
                rows = fresh_db.execute(text(f"SHOW TABLES FROM {database}")).fetchall()
                values = [next(iter(row._mapping.values())) for row in rows if getattr(row, "_mapping", None)]
                if table in values:
                    return
            except Exception as exc:
                last_error = exc
            finally:
                fresh_db.close()
            time.sleep(base_delay_s * (attempt + 1))
        detail = f": {last_error}" if last_error else ""
        raise RuntimeError(f"Table {qualified_name} was not visible after {attempts} retries{detail}")

    def create(
        self, name: str, source: str, snapshot: str | None = None, is_database: bool = False
    ) -> None:
        """Create branch (zero-copy).

        Uses `data branch create table/database ... from ...`.
        Kernel records LCA for future diff/merge.
        """
        with self._db() as db:
            db.commit()
            entity = "database" if is_database else "table"

            if not is_database:
                name = self._qualify(name)
                source = self._qualify(source)
                self._wait_until_table_visible(source)

            src = f'{source}{{snapshot="{snapshot}"}}' if snapshot else source
            db.execute(text(f"data branch create {entity} {name} from {src}"))
            db.commit()
            if not is_database:
                self._wait_until_table_visible(name)

    def diff(
        self,
        target: str,
        source: str,
        output: str = "default",
        target_snapshot: str | None = None,
        source_snapshot: str | None = None,
    ) -> list[dict]:
        """Diff two tables using native data branch diff.

        Kernel auto-detects LCA for three-way comparison.
        Works with or without snapshots.
        """
        with self._db() as db:
            db.commit()

            t = self._qualify(target)
            s = self._qualify(source)

            if target_snapshot:
                t = f'{t}{{snapshot="{target_snapshot}"}}'
            if source_snapshot:
                s = f'{s}{{snapshot="{source_snapshot}"}}'

            query = f"data branch diff {t} against {s}"
            if output == "count":
                query += " output count"

            result = db.execute(text(query))
            return [dict(row._mapping) for row in result]

    def merge(self, source: str, target: str, on_conflict: str = "skip") -> None:
        """Merge source into target using native data branch merge.

        Args:
            on_conflict: "error" (default, raises on conflict),
                         "skip" (keep target), "accept" (take source)
        """
        with self._db() as db:
            db.commit()

            s = self._qualify(source)
            t = self._qualify(target)

            query = f"data branch merge {s} into {t}"
            if on_conflict in ("skip", "accept"):
                query += f" when conflict {on_conflict}"

            db.execute(text(query))
            db.commit()

    def delete(self, name: str, is_database: bool = False) -> None:
        """Delete branch using native data branch delete.

        Properly cleans up branch metadata in kernel.
        """
        with self._db() as db:
            db.commit()
            entity = "database" if is_database else "table"
            if not is_database:
                name = self._qualify(name)
            db.execute(text(f"data branch delete {entity} {name}"))
            db.commit()
