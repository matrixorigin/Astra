"""Branch manager for Git-like data workflows."""

from typing import Optional

from sdk.database import Database


class Branch:
    """Branch manager using MatrixOne's data branch."""

    def __init__(self, database: str = "dev_agent", db: Optional[Database] = None):
        self.database = database
        self.db = db or Database()

    def create(self, name: str, source: str, snapshot: Optional[str] = None, is_database: bool = False) -> None:
        """Create branch.
        
        Args:
            name: Branch name
            source: Source table/database name
            snapshot: Optional snapshot to branch from
            is_database: True for database branch, False for table branch
        """
        entity = "database" if is_database else "table"
        
        if snapshot:
            self.db.execute(f'data branch create {entity} {name} from {source}{{snapshot="{snapshot}"}}')
        else:
            self.db.execute(f"data branch create {entity} {name} from {source}")

    def diff(self, target: str, source: str, output: str = "default", 
             target_snapshot: Optional[str] = None, source_snapshot: Optional[str] = None) -> list[dict]:
        """Diff two tables.
        
        Args:
            target: Target table
            source: Source table
            output: Output mode: "default", "count", or file path
            target_snapshot: Optional snapshot for target
            source_snapshot: Optional snapshot for source
        """
        t = f'{target}{{snapshot="{target_snapshot}"}}' if target_snapshot else target
        s = f'{source}{{snapshot="{source_snapshot}"}}' if source_snapshot else source
        
        if output == "count":
            query = f"data branch diff {t} against {s} output count"
        elif output == "default":
            query = f"data branch diff {t} against {s}"
        else:
            query = f"data branch diff {t} against {s} output file '{output}'"
        
        return self.db.fetchall(query)

    def merge(self, source: str, target: str, on_conflict: str = "error") -> None:
        """Merge source into target.
        
        Args:
            source: Source table
            target: Target table
            on_conflict: Conflict strategy: "error", "skip", or "accept"
        """
        if on_conflict == "skip":
            self.db.execute(f"data branch merge {source} into {target} when conflict skip")
        elif on_conflict == "accept":
            self.db.execute(f"data branch merge {source} into {target} when conflict accept")
        else:
            self.db.execute(f"data branch merge {source} into {target}")

    def delete(self, name: str, is_database: bool = False) -> None:
        """Delete branch.
        
        Args:
            name: Branch name
            is_database: True for database branch, False for table branch
        """
        if is_database:
            self.db.execute(f"data branch delete database {name}")
        else:
            self.db.execute(f"data branch delete table {self.database}.{name}")
