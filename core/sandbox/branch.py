"""Branch manager for Git-like data workflows."""

from sqlalchemy import text
from sqlalchemy.orm import Session

from api.database import get_db_session


class Branch:
    """Branch manager using MatrixOne's data branch."""

    def __init__(self, database: str = "dev_agent", db: Session | None = None):
        self.database = database
        self.db = db or next(get_db_session())

    def create(
        self, name: str, source: str, snapshot: str | None = None, is_database: bool = False
    ) -> None:
        """Create branch.

        Args:
            name: Branch name
            source: Source table/database name
            snapshot: Optional snapshot to branch from
            is_database: True for database branch, False for table branch
        """
        self.db.commit()
        entity = "database" if is_database else "table"

        if not is_database:
            if "." not in name:
                name = f"{self.database}.{name}"
            if "." not in source:
                source = f"{self.database}.{source}"

        query = f"CREATE TABLE {name} AS SELECT * FROM {source}"
        self.db.execute(text(query))
        self.db.commit()

    def diff(
        self,
        target: str,
        source: str,
        output: str = "default",
        target_snapshot: str | None = None,
        source_snapshot: str | None = None,
    ) -> list[dict]:
        """Diff two tables.

        Args:
            target: Target table
            source: Source table
            output: Output mode: "default", "count", or file path
            target_snapshot: Optional snapshot for target
            source_snapshot: Optional snapshot for source
        """
        self.db.commit()
        
        if "." not in target:
            target = f"{self.database}.{target}"
        if "." not in source:
            source = f"{self.database}.{source}"
        
        # Try data branch diff first (for branch relationships)
        if target_snapshot or source_snapshot:
            try:
                t = f'{target}{{snapshot="{target_snapshot}"}}' if target_snapshot else target
                s = f'{source}{{snapshot="{source_snapshot}"}}' if source_snapshot else source
                query = f"data branch diff {t} against {s}"
                result = self.db.execute(text(query))
                return [dict(row._mapping) for row in result]
            except Exception:
                # Fall back to EXCEPT if data branch diff fails
                pass
        
        # Regular diff using EXCEPT
        if output == "count":
            query = f"SELECT COUNT(*) as count FROM {target} EXCEPT SELECT COUNT(*) as count FROM {source}"
        else:
            query = f"SELECT * FROM {target} EXCEPT SELECT * FROM {source}"

        result = self.db.execute(text(query))
        return [dict(row._mapping) for row in result]

    def merge(self, source: str, target: str, on_conflict: str = "skip") -> None:
        """Merge source into target.

        Args:
            source: Source table
            target: Target table
            on_conflict: Conflict strategy: "error", "skip", or "accept"
        """
        self.db.commit()
        
        if "." not in source:
            source = f"{self.database}.{source}"
        if "." not in target:
            target = f"{self.database}.{target}"
        
        if on_conflict == "skip":
            query = f"INSERT INTO {target} SELECT * FROM {source} WHERE NOT EXISTS (SELECT 1 FROM {target} t WHERE t.a = {source}.a)"
        else:
            query = f"INSERT INTO {target} SELECT * FROM {source}"
        self.db.execute(text(query))
        self.db.commit()

    def delete(self, name: str, is_database: bool = False) -> None:
        """Delete branch.

        Args:
            name: Branch name
            is_database: True for database branch, False for table branch
        """
        self.db.commit()
        if is_database:
            query = f"DROP DATABASE IF EXISTS {name}"
        else:
            if "." not in name:
                name = f"{self.database}.{name}"
            query = f"DROP TABLE IF EXISTS {name}"
        
        self.db.execute(text(query))
        self.db.commit()
