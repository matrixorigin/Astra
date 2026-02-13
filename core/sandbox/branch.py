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
        self.db.commit()  # Commit any pending changes so source is visible
        entity = "database" if is_database else "table"

        # For table branches, add database prefix if not present
        if not is_database:
            if "." not in name:
                name = f"{self.database}.{name}"
            if "." not in source:
                source = f"{self.database}.{source}"

        if snapshot:
            query = f'data branch create {entity} {name} from {source}{{snapshot="{snapshot}"}}'
            self.db.execute(text(query))
        else:
            query = f"data branch create {entity} {name} from {source}"
            self.db.execute(text(query))
        
        self.db.commit()  # Commit the branch creation

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
        self.db.commit()  # Commit before data branch command
        
        # Add database prefix if not present
        if "." not in target:
            target = f"{self.database}.{target}"
        if "." not in source:
            source = f"{self.database}.{source}"
            
        t = f'{target}{{snapshot="{target_snapshot}"}}' if target_snapshot else target
        s = f'{source}{{snapshot="{source_snapshot}"}}' if source_snapshot else source

        if output == "count":
            query = f"data branch diff {t} against {s} output count"
        elif output == "default":
            query = f"data branch diff {t} against {s}"
        else:
            query = f"data branch diff {t} against {s} output file '{output}'"

        result = self.db.execute(text(query))
        return [dict(row._mapping) for row in result]

    def merge(self, source: str, target: str, on_conflict: str = "error") -> None:
        """Merge source into target.

        Args:
            source: Source table
            target: Target table
            on_conflict: Conflict strategy: "error", "skip", or "accept"
        """
        self.db.commit()  # Commit before data branch command
        
        # Add database prefix if not present
        if "." not in source:
            source = f"{self.database}.{source}"
        if "." not in target:
            target = f"{self.database}.{target}"
            
        if on_conflict == "skip":
            query = f"data branch merge {source} into {target} when conflict skip"
        elif on_conflict == "accept":
            query = f"data branch merge {source} into {target} when conflict accept"
        else:
            query = f"data branch merge {source} into {target}"
        
        self.db.execute(text(query))

    def delete(self, name: str, is_database: bool = False) -> None:
        """Delete branch.

        Args:
            name: Branch name
            is_database: True for database branch, False for table branch
        """
        self.db.commit()  # Commit any pending changes
        if is_database:
            query = f"data branch delete database {name}"
        else:
            query = f"data branch delete table {self.database}.{name}"
        
        self.db.execute(text(query))
        self.db.commit()  # Commit the deletion
