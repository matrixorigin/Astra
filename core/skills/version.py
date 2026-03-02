"""Semantic version parsing and constraint matching.

Supports pip-style constraints: >=1.0, <2.0, ~=1.2.3, ==1.0.0, !=1.5.0
Compound constraints: ">=1.0,<2.0"
Wildcard: "*" matches any version.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from functools import total_ordering

_VER_RE = re.compile(r"^(\d+)(?:\.(\d+))?(?:\.(\d+))?$")
_CONSTRAINT_RE = re.compile(r"(~=|>=|<=|!=|==|>|<)\s*(\d+(?:\.\d+){0,2})")


@total_ordering
@dataclass(frozen=True, slots=True)
class Version:
    major: int
    minor: int
    patch: int

    def __str__(self) -> str:
        return f"{self.major}.{self.minor}.{self.patch}"

    def __lt__(self, other: object) -> bool:
        if not isinstance(other, Version):
            return NotImplemented
        return (self.major, self.minor, self.patch) < (other.major, other.minor, other.patch)

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, Version):
            return NotImplemented
        return (self.major, self.minor, self.patch) == (other.major, other.minor, other.patch)


def parse_version(v: str) -> Version:
    """Parse '1.2.3', '1.2', or '1' into a Version."""
    m = _VER_RE.match(v.strip())
    if not m:
        raise ValueError(f"Invalid version: {v!r}")
    return Version(int(m.group(1)), int(m.group(2) or 0), int(m.group(3) or 0))


def _count_segments(v: str) -> int:
    """Count dot-separated segments: '1' → 1, '1.2' → 2, '1.2.3' → 3."""
    return v.strip().count(".") + 1


class VersionConstraint:
    """Evaluate compound version constraints like '>=1.0,<2.0'."""

    def __init__(self, constraint: str):
        self._raw = constraint.strip()
        if self._raw == "*":
            self._checks: list[tuple[str, Version, int]] = []
            return
        parts = _CONSTRAINT_RE.findall(self._raw)
        if not parts:
            raise ValueError(f"Invalid version constraint: {self._raw!r}")
        # Store (operator, parsed_version, segment_count) — segment_count is
        # needed by ~= to distinguish "~=1.2" (2 segments) from "~=1.2.0" (3).
        self._checks = [(op, parse_version(ver), _count_segments(ver)) for op, ver in parts]

    def matches(self, version: str | Version) -> bool:
        v = version if isinstance(version, Version) else parse_version(version)
        if not self._checks:  # wildcard
            return True
        for op, target, segs in self._checks:
            if not self._cmp(op, v, target, segs):
                return False
        return True

    @staticmethod
    def _cmp(op: str, v: Version, t: Version, segments: int) -> bool:
        if op == ">=":
            return v >= t
        if op == "<=":
            return v <= t
        if op == ">":
            return v > t
        if op == "<":
            return v < t
        if op == "==":
            return v == t
        if op == "!=":
            return v != t
        if op == "~=":
            # PEP 440 compatible release.  The upper bound is determined by
            # how many segments the user wrote, NOT by the parsed value:
            #   ~=1.2.3  (3 segments) → >=1.2.3, <1.3.0   (bump minor)
            #   ~=1.2    (2 segments) → >=1.2.0, <2.0.0   (bump major)
            #   ~=1.2.0  (3 segments) → >=1.2.0, <1.3.0   (bump minor)
            if segments >= 3:
                upper = Version(t.major, t.minor + 1, 0)
            else:
                upper = Version(t.major + 1, 0, 0)
            return v >= t and v < upper
        raise ValueError(f"Unknown operator: {op!r}")

    def __repr__(self) -> str:
        return f"VersionConstraint({self._raw!r})"
