"""SkillDataBridge — controlled cross-skill data access.

Skills do NOT import each other's modules or query each other's tables directly.
The bridge validates dependency declarations and table ownership before allowing access.
"""

from __future__ import annotations

import logging
import re
from typing import Any, Callable

from sqlalchemy import text
from sqlalchemy.orm import Session

from core.skills.loader import SkillManifest

logger = logging.getLogger(__name__)

_SQL_IDENTIFIER_RE = re.compile(r"^[a-z][a-z0-9_]*$")


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
        # Cache requesting skill's depends_on
        own = manifest_loader(requesting_skill)
        self._allowed: set[str] = set(own.depends_on)

    def query(
        self,
        target_skill: str,
        table: str,
        filters: dict[str, Any] | None = None,
        limit: int = 100,
    ) -> list[dict[str, Any]]:
        """Read rows from another skill's table."""
        self._validate(target_skill, table)
        where, params = _build_where(filters)
        sql = f"SELECT * FROM {table}{where} LIMIT :_limit"
        params["_limit"] = limit
        logger.info("SkillDataBridge: %s → %s.%s filters=%s", self._skill, target_skill, table, filters)
        rows = self._db.execute(text(sql), params).mappings().all()
        return [dict(r) for r in rows]

    def count(
        self,
        target_skill: str,
        table: str,
        filters: dict[str, Any] | None = None,
    ) -> int:
        """Count rows in another skill's table."""
        self._validate(target_skill, table)
        where, params = _build_where(filters)
        sql = f"SELECT COUNT(*) AS cnt FROM {table}{where}"
        row = self._db.execute(text(sql), params).mappings().one()
        return int(row["cnt"])

    def _validate(self, target_skill: str, table: str) -> None:
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
        # Prevent SQL injection via table name
        if not _SQL_IDENTIFIER_RE.match(table):
            raise ValueError(f"Invalid table name: {table!r}")


def _build_where(filters: dict[str, Any] | None) -> tuple[str, dict[str, Any]]:
    if not filters:
        return "", {}
    clauses = []
    params: dict[str, Any] = {}
    for i, (col, val) in enumerate(filters.items()):
        if not _SQL_IDENTIFIER_RE.match(col):
            raise ValueError(f"Invalid column name in filter: {col!r}")
        param_name = f"_f{i}"
        clauses.append(f"{col} = :{param_name}")
        params[param_name] = val
    return " WHERE " + " AND ".join(clauses), params
