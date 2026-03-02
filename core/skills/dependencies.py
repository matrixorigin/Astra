"""Typed dependency model for skills and tools.

Supports both old format (list[str]) and new format (list[Dependency]).
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from typing import Any


class DependencyType(str, Enum):
    SKILL = "skill"
    TOOL = "tool"


@dataclass(frozen=True, slots=True)
class Dependency:
    """A single dependency with version constraint."""

    name: str
    version_constraint: str = "*"  # default: any version
    type: DependencyType = DependencyType.SKILL

    @classmethod
    def from_raw(cls, raw: str | dict[str, Any]) -> Dependency:
        """Parse from old format (str) or new format (dict)."""
        if isinstance(raw, str):
            return cls(name=raw)
        return cls(
            name=raw["name"],
            version_constraint=raw.get("version", "*"),
            type=DependencyType(raw.get("type", "skill")),
        )


def parse_depends_on(raw: list[str | dict[str, Any]]) -> list[Dependency]:
    """Convert a raw depends_on list to typed Dependency objects."""
    return [Dependency.from_raw(item) for item in raw]
