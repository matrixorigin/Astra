"""Tests for core/skills/resolver.py — dependency resolution, cycles, conflicts."""

import pytest

from core.skills.dependencies import Dependency, DependencyType
from core.skills.resolver import (
    CircularDependencyError,
    DependencyConflictError,
    DependencyResolver,
)


def _dep(name: str, version: str = "*", typ: str = "skill") -> Dependency:
    return Dependency(name=name, version_constraint=version, type=DependencyType(typ))


class TestResolveSuccess:
    def test_no_deps(self):
        r = DependencyResolver()
        result = r.resolve("my_skill", [])
        assert result.success
        assert result.ordered_deps == []

    def test_simple_skill_dep(self):
        r = DependencyResolver(
            available_skills={"base": {"version": "1.0.0", "depends_on": []}},
        )
        result = r.resolve("app", [_dep("base", ">=1.0")])
        assert result.success
        assert len(result.ordered_deps) == 1
        assert result.ordered_deps[0].name == "base"

    def test_tool_dep(self):
        r = DependencyResolver(available_tools={"git_status": "1.5.0"})
        result = r.resolve("app", [_dep("git_status", ">=1.0", "tool")])
        assert result.success

    def test_transitive_deps(self):
        r = DependencyResolver(
            available_skills={
                "a": {"version": "1.0.0", "depends_on": ["b"]},
                "b": {"version": "1.0.0", "depends_on": []},
            },
        )
        result = r.resolve("root", [_dep("a")])
        assert result.success
        names = [d.name for d in result.ordered_deps]
        assert names.index("b") < names.index("a")

    def test_diamond_deps(self):
        r = DependencyResolver(
            available_skills={
                "left": {"version": "1.0.0", "depends_on": ["shared"]},
                "right": {"version": "1.0.0", "depends_on": ["shared"]},
                "shared": {"version": "2.0.0", "depends_on": []},
            },
        )
        result = r.resolve("root", [_dep("left"), _dep("right")])
        assert result.success
        names = [d.name for d in result.ordered_deps]
        assert names.index("shared") < names.index("left")
        assert names.index("shared") < names.index("right")

    def test_wildcard_always_matches(self):
        r = DependencyResolver(
            available_skills={"x": {"version": "99.0.0", "depends_on": []}},
        )
        result = r.resolve("app", [_dep("x", "*")])
        assert result.success


class TestResolveMissing:
    def test_missing_skill(self):
        r = DependencyResolver()
        result = r.resolve("app", [_dep("nonexistent")])
        assert not result.success
        assert "nonexistent" in result.missing

    def test_missing_tool(self):
        r = DependencyResolver()
        result = r.resolve("app", [_dep("no_tool", "*", "tool")])
        assert not result.success
        assert "no_tool" in result.missing


class TestResolveConflicts:
    def test_version_too_low(self):
        r = DependencyResolver(
            available_skills={"lib": {"version": "1.0.0", "depends_on": []}},
        )
        result = r.resolve("app", [_dep("lib", ">=2.0")])
        assert not result.success
        assert len(result.conflicts) == 1
        assert result.conflicts[0].dependency == "lib"
        assert result.conflicts[0].available_version == "1.0.0"

    def test_tool_version_conflict(self):
        r = DependencyResolver(available_tools={"git": "1.0.0"})
        result = r.resolve("app", [_dep("git", ">=2.0", "tool")])
        assert not result.success
        assert result.conflicts[0].dependency == "git"


class TestCycleDetection:
    def test_simple_cycle(self):
        r = DependencyResolver(
            available_skills={
                "a": {"version": "1.0.0", "depends_on": ["b"]},
                "b": {"version": "1.0.0", "depends_on": ["a"]},
            },
        )
        cycle = r.detect_cycles("root", [_dep("a")])
        assert cycle is not None
        assert "a" in cycle
        assert "b" in cycle
        # Cycle must start and end with the same node
        assert cycle[0] == cycle[-1]

    def test_self_dependency(self):
        r = DependencyResolver(
            available_skills={
                "self_ref": {"version": "1.0.0", "depends_on": ["self_ref"]},
            },
        )
        cycle = r.detect_cycles("root", [_dep("self_ref")])
        assert cycle is not None
        assert cycle == ["self_ref", "self_ref"]

    def test_no_cycle(self):
        r = DependencyResolver(
            available_skills={
                "a": {"version": "1.0.0", "depends_on": ["b"]},
                "b": {"version": "1.0.0", "depends_on": []},
            },
        )
        cycle = r.detect_cycles("root", [_dep("a")])
        assert cycle is None

    def test_long_cycle(self):
        r = DependencyResolver(
            available_skills={
                "a": {"version": "1.0.0", "depends_on": ["b"]},
                "b": {"version": "1.0.0", "depends_on": ["c"]},
                "c": {"version": "1.0.0", "depends_on": ["a"]},
            },
        )
        cycle = r.detect_cycles("root", [_dep("a")])
        assert cycle is not None
        assert cycle[0] == cycle[-1]
        # All three nodes must appear in the cycle
        cycle_body = set(cycle[:-1])
        assert cycle_body == {"a", "b", "c"}

    def test_resolve_returns_cycle_field(self):
        """resolve() populates cycle field instead of abusing missing."""
        r = DependencyResolver(
            available_skills={
                "a": {"version": "1.0.0", "depends_on": ["b"]},
                "b": {"version": "1.0.0", "depends_on": ["a"]},
            },
        )
        result = r.resolve("root", [_dep("a")])
        assert not result.success
        assert result.cycle is not None
        assert result.missing == []
        assert result.conflicts == []


class TestUpgradeImpact:
    def test_no_impact(self):
        r = DependencyResolver(
            available_skills={
                "consumer": {"version": "1.0.0", "depends_on": [
                    {"name": "lib", "version": ">=1.0,<3.0", "type": "skill"},
                ]},
                "lib": {"version": "1.0.0", "depends_on": []},
            },
        )
        broken = r.analyze_upgrade_impact("lib", "2.0.0")
        assert broken == []

    def test_breaks_dependent(self):
        r = DependencyResolver(
            available_skills={
                "consumer": {"version": "1.0.0", "depends_on": [
                    {"name": "lib", "version": ">=1.0,<2.0", "type": "skill"},
                ]},
                "lib": {"version": "1.0.0", "depends_on": []},
            },
        )
        broken = r.analyze_upgrade_impact("lib", "2.0.0")
        assert len(broken) == 1
        assert broken[0][0] == "consumer"

    def test_multiple_dependents(self):
        r = DependencyResolver(
            available_skills={
                "a": {"version": "1.0.0", "depends_on": [
                    {"name": "lib", "version": "~=1.0", "type": "skill"},
                ]},
                "b": {"version": "1.0.0", "depends_on": [
                    {"name": "lib", "version": "==1.0.0", "type": "skill"},
                ]},
                "lib": {"version": "1.0.0", "depends_on": []},
            },
        )
        broken = r.analyze_upgrade_impact("lib", "2.0.0")
        names = {b[0] for b in broken}
        assert names == {"a", "b"}


class TestErrorMessages:
    def test_conflict_error_message(self):
        from core.skills.resolver import Conflict
        err = DependencyConflictError([
            Conflict(dependency="lib", required_by=[("app", ">=2.0")], available_version="1.0.0"),
        ])
        assert "lib" in str(err)
        assert "1.0.0" in str(err)

    def test_circular_error_message(self):
        err = CircularDependencyError(["a", "b", "a"])
        assert "a → b → a" in str(err)
