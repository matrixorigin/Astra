"""Dependency resolver — validate, order, and detect conflicts/cycles.

Used by SkillManager.install() to ensure all dependencies are satisfiable
before installation proceeds.
"""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass, field
from typing import Any

from core.skills.dependencies import Dependency, DependencyType, parse_depends_on
from core.skills.version import VersionConstraint, parse_version


@dataclass
class Conflict:
    """A version conflict: multiple skills require incompatible versions."""

    dependency: str
    required_by: list[tuple[str, str]]  # [(requirer_name, constraint)]
    available_version: str


@dataclass
class ResolveResult:
    success: bool
    ordered_deps: list[Dependency] = field(default_factory=list)
    conflicts: list[Conflict] = field(default_factory=list)
    missing: list[str] = field(default_factory=list)
    cycle: list[str] | None = None


class CircularDependencyError(Exception):
    def __init__(self, cycle: list[str]):
        self.cycle = cycle
        super().__init__(f"Circular dependency: {' → '.join(cycle)}")


class DependencyConflictError(Exception):
    def __init__(self, conflicts: list[Conflict]):
        self.conflicts = conflicts
        lines = []
        for c in conflicts:
            reqs = ", ".join(f"{r} requires {v}" for r, v in c.required_by)
            lines.append(f"  {c.dependency} (available {c.available_version}): {reqs}")
        super().__init__("Version conflicts:\n" + "\n".join(lines))


class DependencyResolver:
    """Resolve and validate skill/tool dependency trees.

    Parameters
    ----------
    available_skills : dict mapping skill name → dict with at least 'version' and 'depends_on'.
    available_tools  : dict mapping tool name → version string.
    """

    def __init__(
        self,
        available_skills: dict[str, dict[str, Any]] | None = None,
        available_tools: dict[str, str] | None = None,
    ):
        self._skills = available_skills or {}
        self._tools = available_tools or {}
        # Pre-parse depends_on once; reused by cycle detection, BFS, and topo sort.
        self._parsed_deps: dict[str, list[Dependency]] = {}
        for name, info in self._skills.items():
            raw = info.get("depends_on", [])
            self._parsed_deps[name] = parse_depends_on(raw) if raw else []

    def _get_skill_deps(self, name: str) -> list[Dependency]:
        return self._parsed_deps.get(name, [])

    def resolve(self, skill_name: str, depends_on: list[Dependency]) -> ResolveResult:
        """Resolve all dependencies for a skill. Returns ordered list or failure details."""

        # 1. Cycle detection first — version checks on a cyclic graph are unreliable
        cycle = self._detect_cycle(skill_name, depends_on)
        if cycle:
            return ResolveResult(success=False, cycle=cycle)

        # 2. Collect all constraints per dependency (BFS for transitive)
        missing: list[str] = []
        constraint_map: dict[str, list[tuple[str, str]]] = {}
        visited: set[str] = set()
        queue: deque[tuple[str, list[Dependency]]] = deque([(skill_name, depends_on)])

        while queue:
            requirer, deps = queue.popleft()
            for dep in deps:
                constraint_map.setdefault(dep.name, []).append((requirer, dep.version_constraint))
                if dep.name in visited:
                    continue
                visited.add(dep.name)

                if dep.type == DependencyType.TOOL:
                    if dep.name not in self._tools:
                        missing.append(dep.name)
                else:
                    if dep.name not in self._skills:
                        missing.append(dep.name)
                    else:
                        queue.append((dep.name, self._get_skill_deps(dep.name)))

        if missing:
            return ResolveResult(success=False, missing=missing)

        # 3. Check version compatibility
        conflicts: list[Conflict] = []
        for dep_name, requirements in constraint_map.items():
            available_ver = self._get_version(dep_name)
            for requirer, constraint_str in requirements:
                try:
                    c = VersionConstraint(constraint_str)
                    if not c.matches(available_ver):
                        conflicts.append(Conflict(
                            dependency=dep_name,
                            required_by=requirements,
                            available_version=available_ver,
                        ))
                        break  # one conflict per dep is enough
                except ValueError:
                    conflicts.append(Conflict(
                        dependency=dep_name,
                        required_by=[(requirer, constraint_str)],
                        available_version=available_ver,
                    ))
                    break

        if conflicts:
            return ResolveResult(success=False, conflicts=conflicts)

        # 4. Topological sort
        ordered = self._topological_sort(skill_name, depends_on)
        return ResolveResult(success=True, ordered_deps=ordered)

    def detect_cycles(self, skill_name: str, depends_on: list[Dependency]) -> list[str] | None:
        """Public API: return cycle path or None."""
        return self._detect_cycle(skill_name, depends_on)

    def _detect_cycle(self, root: str, deps: list[Dependency]) -> list[str] | None:
        """DFS cycle detection on skill-only edges. Returns cycle path or None.

        Only skill→skill edges can form cycles; tool deps are leaf nodes
        (tools never declare their own depends_on).
        """
        adj: dict[str, list[str]] = {root: [d.name for d in deps if d.type == DependencyType.SKILL]}
        for name in self._skills:
            adj[name] = [d.name for d in self._get_skill_deps(name) if d.type == DependencyType.SKILL]

        WHITE, GRAY, BLACK = 0, 1, 2
        color: dict[str, int] = {n: WHITE for n in adj}
        # path tracks the current DFS stack for accurate cycle reconstruction
        path: list[str] = []

        def dfs(u: str) -> list[str] | None:
            color[u] = GRAY
            path.append(u)
            for v in adj.get(u, []):
                if v not in color:
                    # Node not in adj (e.g. missing skill) — skip, resolve()
                    # will report it as missing later.
                    color[v] = WHITE
                if color[v] == GRAY:
                    idx = path.index(v)
                    return path[idx:] + [v]
                if color[v] == WHITE:
                    result = dfs(v)
                    if result:
                        return result
            path.pop()
            color[u] = BLACK
            return None

        return dfs(root)

    def _topological_sort(self, root: str, deps: list[Dependency]) -> list[Dependency]:
        """Return dependencies in install order (leaves first) using Kahn's algorithm."""
        # Collect all deps (excluding root)
        all_deps: dict[str, Dependency] = {}
        queue: deque[tuple[str, list[Dependency]]] = deque([(root, deps)])
        visited: set[str] = set()

        while queue:
            _, dep_list = queue.popleft()
            for d in dep_list:
                if d.name in visited:
                    continue
                visited.add(d.name)
                all_deps[d.name] = d
                if d.type == DependencyType.SKILL and d.name in self._skills:
                    queue.append((d.name, self._get_skill_deps(d.name)))

        if not all_deps:
            return []

        # Build adjacency for Kahn's: edge cd→name means cd must come before name
        adj: dict[str, list[str]] = {n: [] for n in all_deps}
        in_deg: dict[str, int] = {n: 0 for n in all_deps}
        for name in all_deps:
            if all_deps[name].type == DependencyType.SKILL and name in self._skills:
                for cd in self._get_skill_deps(name):
                    if cd.name in adj:
                        adj[cd.name].append(name)
                        in_deg[name] += 1

        q = sorted(n for n, d in in_deg.items() if d == 0)
        ordered: list[Dependency] = []
        while q:
            n = q.pop(0)
            ordered.append(all_deps[n])
            for nb in adj.get(n, []):
                in_deg[nb] -= 1
                if in_deg[nb] == 0:
                    q.append(nb)
            q.sort()  # deterministic order

        return ordered

    def _get_version(self, name: str) -> str:
        if name in self._tools:
            return self._tools[name]
        if name in self._skills:
            return str(self._skills[name].get("version", "0.0.0"))
        return "0.0.0"

    def analyze_upgrade_impact(
        self, skill_name: str, new_version: str
    ) -> list[tuple[str, str]]:
        """Return list of (dependent_skill, constraint) that would break with new_version."""
        broken: list[tuple[str, str]] = []
        v = parse_version(new_version)
        for name, deps in self._parsed_deps.items():
            for d in deps:
                if d.name == skill_name:
                    try:
                        c = VersionConstraint(d.version_constraint)
                        if not c.matches(v):
                            broken.append((name, d.version_constraint))
                    except ValueError:
                        broken.append((name, d.version_constraint))
        return broken
