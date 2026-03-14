"""SkillDataBridge — controlled cross-skill data access via ORM.

Skills do NOT import each other's modules or query each other's tables directly.
The bridge validates dependency declarations and table ownership before allowing access.
All queries use SQLAlchemy ORM (Table objects from Base.metadata) — no raw SQL.
"""

from __future__ import annotations

import logging
from typing import Any, Callable

from sqlalchemy import func, select
from sqlalchemy.orm import Session

from api.base import Base
from core.skills.loader import SkillManifest

logger = logging.getLogger(__name__)


class SkillDataBridge:
    """Platform-provided cross-skill data access.

    Injected into skill API constructors. Skills call bridge methods
    instead of importing each other's models or writing raw SQL.
    """

    def __init__(
        self,
        db: Session,
        requesting_skill: str,
        manifest_loader: Callable[[str], SkillManifest],
    ):
        self._db = db
        self._skill = requesting_skill
        self._loader = manifest_loader
        own = manifest_loader(requesting_skill)
        # depends_on is list[Dependency] after loader parses manifest.yaml.
        # Defensive: also handle raw strings for callers that construct
        # SkillManifest directly (e.g. tests) without going through parse_depends_on.
        self._allowed: set[str] = {d.name if hasattr(d, "name") else str(d) for d in own.depends_on}

    def query(
        self,
        target_skill: str,
        table: str,
        filters: dict[str, Any] | None = None,
        limit: int = 100,
    ) -> list[dict[str, Any]]:
        """Read rows from another skill's table."""
        tbl = self._resolve_table(target_skill, table)
        stmt = select(tbl)
        stmt = _apply_filters(stmt, tbl, filters)
        stmt = stmt.limit(limit)
        logger.info(
            "SkillDataBridge: %s → %s.%s filters=%s", self._skill, target_skill, table, filters
        )
        rows = self._db.execute(stmt).mappings().all()
        return [dict(r) for r in rows]

    def count(
        self,
        target_skill: str,
        table: str,
        filters: dict[str, Any] | None = None,
    ) -> int:
        """Count rows in another skill's table."""
        tbl = self._resolve_table(target_skill, table)
        stmt = select(func.count()).select_from(tbl)
        stmt = _apply_filters(stmt, tbl, filters)
        return self._db.execute(stmt).scalar() or 0

    def _resolve_table(self, target_skill: str, table: str):
        """Validate access and return the SQLAlchemy Table object."""
        if target_skill not in self._allowed:
            raise PermissionError(
                f"Skill {self._skill!r} does not declare dependency on {target_skill!r}"
            )
        expected_prefix = f"sk_{target_skill}_"
        if not table.startswith(expected_prefix):
            raise ValueError(
                f"Table {table!r} does not belong to skill {target_skill!r} "
                f"(expected prefix {expected_prefix!r})"
            )
        tbl = Base.metadata.tables.get(table)
        if tbl is None:
            raise ValueError(f"Table {table!r} not found in ORM metadata")
        return tbl


def _apply_filters(stmt, tbl, filters: dict[str, Any] | None):
    """Apply filter dict to a select statement using ORM column objects."""
    if not filters:
        return stmt
    for col_name, val in filters.items():
        col = tbl.c.get(col_name)
        if col is None:
            raise ValueError(f"Column {col_name!r} not found in table {tbl.name!r}")
        stmt = stmt.where(col == val)
    return stmt
