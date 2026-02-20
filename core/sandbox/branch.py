"""Branch manager for Git-like data workflows using MatrixOne's native data branch."""

from sqlalchemy import text
from sqlalchemy.orm import Session

from api.database import get_db_session


class Branch:
    """Branch manager using MatrixOne's native data branch commands.

    Supports zero-copy branching with automatic LCA tracking,
    three-way diff, and merge with conflict strategies.
    """

    def __init__(self, database: str = "dev_agent", db: Session | None = None):
        self.database = database
        self.db = db or next(get_db_session())

    def _qualify(self, name: str) -> str:
        if "." not in name:
            return f"{self.database}.{name}"
        return name

    def create(
        self, name: str, source: str, snapshot: str | None = None, is_database: bool = False
    ) -> None:
        """Create branch (zero-copy).

        Uses `data branch create table/database ... from ...`.
        Kernel records LCA for future diff/merge.
        """
        self.db.commit()
        entity = "database" if is_database else "table"

        if not is_database:
            name = self._qualify(name)
            source = self._qualify(source)

        src = f'{source}{{snapshot="{snapshot}"}}' if snapshot else source
        self.db.execute(text(f"data branch create {entity} {name} from {src}"))
        self.db.commit()

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
        self.db.commit()

        t = self._qualify(target)
        s = self._qualify(source)

        if target_snapshot:
            t = f'{t}{{snapshot="{target_snapshot}"}}'
        if source_snapshot:
            s = f'{s}{{snapshot="{source_snapshot}"}}'

        query = f"data branch diff {t} against {s}"
        if output == "count":
            query += " output count"

        result = self.db.execute(text(query))
        return [dict(row._mapping) for row in result]

    def merge(
        self, source: str, target: str, on_conflict: str = "skip"
    ) -> None:
        """Merge source into target using native data branch merge.

        Args:
            on_conflict: "error" (default, raises on conflict),
                         "skip" (keep target), "accept" (take source)
        """
        self.db.commit()

        s = self._qualify(source)
        t = self._qualify(target)

        query = f"data branch merge {s} into {t}"
        if on_conflict in ("skip", "accept"):
            query += f" when conflict {on_conflict}"

        self.db.execute(text(query))
        self.db.commit()

    def delete(self, name: str, is_database: bool = False) -> None:
        """Delete branch using native data branch delete.

        Properly cleans up branch metadata in kernel.
        """
        self.db.commit()
        entity = "database" if is_database else "table"
        if not is_database:
            name = self._qualify(name)
        self.db.execute(text(f"data branch delete {entity} {name}"))
        self.db.commit()
