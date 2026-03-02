"""Tests for core/skills/dependencies.py — typed dependency model."""

import pytest

from core.skills.dependencies import Dependency, DependencyType, parse_depends_on


class TestDependency:
    def test_defaults(self):
        d = Dependency(name="foo")
        assert d.version_constraint == "*"
        assert d.type == DependencyType.SKILL

    def test_from_raw_string(self):
        d = Dependency.from_raw("github")
        assert d.name == "github"
        assert d.version_constraint == "*"
        assert d.type == DependencyType.SKILL

    def test_from_raw_dict_full(self):
        d = Dependency.from_raw({"name": "git", "version": ">=1.0", "type": "tool"})
        assert d.name == "git"
        assert d.version_constraint == ">=1.0"
        assert d.type == DependencyType.TOOL

    def test_from_raw_dict_defaults(self):
        d = Dependency.from_raw({"name": "base"})
        assert d.version_constraint == "*"
        assert d.type == DependencyType.SKILL

    def test_frozen(self):
        d = Dependency(name="x")
        with pytest.raises(AttributeError):
            d.name = "y"  # type: ignore[misc]


class TestParseDependsOn:
    def test_empty(self):
        assert parse_depends_on([]) == []

    def test_old_format(self):
        deps = parse_depends_on(["github", "jira"])
        assert len(deps) == 2
        assert deps[0].name == "github"
        assert deps[1].name == "jira"
        assert all(d.version_constraint == "*" for d in deps)

    def test_new_format(self):
        deps = parse_depends_on([
            {"name": "git", "version": ">=1.0", "type": "tool"},
            {"name": "knowledge", "version": "~=2.1.0", "type": "skill"},
        ])
        assert deps[0].type == DependencyType.TOOL
        assert deps[1].version_constraint == "~=2.1.0"

    def test_mixed_format(self):
        deps = parse_depends_on([
            "github",
            {"name": "git", "version": ">=1.0", "type": "tool"},
        ])
        assert deps[0].name == "github"
        assert deps[0].type == DependencyType.SKILL
        assert deps[1].name == "git"
        assert deps[1].type == DependencyType.TOOL
