"""Tests for core/skills/data_bridge.py — cross-skill data access."""

from unittest.mock import MagicMock

import pytest

from core.skills.data_bridge import SkillDataBridge, _build_where
from core.skills.loader import SkillManifest


@pytest.fixture
def manifests() -> dict[str, SkillManifest]:
    return {
        "knowledge": SkillManifest(name="knowledge", version="1.0", depends_on=["github"]),
        "github": SkillManifest(name="github", version="1.0", depends_on=[]),
    }


@pytest.fixture
def bridge(manifests):
    db = MagicMock()
    return SkillDataBridge(
        db=db,
        requesting_skill="knowledge",
        manifest_loader=lambda name: manifests[name],
    )


class TestValidation:
    def test_unauthorized_skill_raises(self, bridge):
        with pytest.raises(PermissionError, match="does not declare dependency"):
            bridge.query("jira", "sk_jira_issues")

    def test_wrong_table_prefix_raises(self, bridge):
        with pytest.raises(ValueError, match="does not belong to skill"):
            bridge.query("github", "sk_jira_issues")

    def test_authorized_skill_passes_validation(self, bridge):
        bridge._db.execute.return_value.mappings.return_value.all.return_value = []
        result = bridge.query("github", "sk_github_repos")
        assert result == []

    def test_count_unauthorized_raises(self, bridge):
        with pytest.raises(PermissionError):
            bridge.count("jira", "sk_jira_issues")

    def test_count_wrong_prefix_raises(self, bridge):
        with pytest.raises(ValueError):
            bridge.count("github", "wrong_table")

    def test_table_name_sql_injection_blocked(self, bridge):
        with pytest.raises(ValueError, match="Invalid table name"):
            bridge.query("github", "sk_github_repos; DROP TABLE x")

    def test_unknown_skill_in_loader_raises(self, manifests):
        db = MagicMock()
        with pytest.raises(KeyError):
            SkillDataBridge(db=db, requesting_skill="nonexistent",
                            manifest_loader=lambda name: manifests[name])


class TestQuery:
    def test_query_returns_dicts(self, bridge):
        bridge._db.execute.return_value.mappings.return_value.all.return_value = [
            {"id": "1", "name": "repo1"},
            {"id": "2", "name": "repo2"},
        ]
        result = bridge.query("github", "sk_github_repos")
        assert len(result) == 2
        assert result[0] == {"id": "1", "name": "repo1"}

    def test_query_with_filters(self, bridge):
        bridge._db.execute.return_value.mappings.return_value.all.return_value = []
        bridge.query("github", "sk_github_repos", filters={"owner": "alice"})
        call_args = bridge._db.execute.call_args
        sql_text = str(call_args[0][0])
        assert "owner = :_f0" in sql_text
        assert call_args[0][1]["_f0"] == "alice"

    def test_query_with_limit(self, bridge):
        bridge._db.execute.return_value.mappings.return_value.all.return_value = []
        bridge.query("github", "sk_github_repos", limit=5)
        call_args = bridge._db.execute.call_args
        assert call_args[0][1]["_limit"] == 5

    def test_count_returns_int(self, bridge):
        bridge._db.execute.return_value.mappings.return_value.one.return_value = {"cnt": 42}
        result = bridge.count("github", "sk_github_repos")
        assert result == 42

    def test_count_with_filters(self, bridge):
        bridge._db.execute.return_value.mappings.return_value.one.return_value = {"cnt": 0}
        bridge.count("github", "sk_github_repos", filters={"state": "open"})
        sql_text = str(bridge._db.execute.call_args[0][0])
        assert "COUNT(*)" in sql_text
        assert "state = :_f0" in sql_text


class TestBuildWhere:
    def test_empty_filters(self):
        clause, params = _build_where(None)
        assert clause == ""
        assert params == {}

    def test_single_filter(self):
        clause, params = _build_where({"status": "open"})
        assert clause == " WHERE status = :_f0"
        assert params == {"_f0": "open"}

    def test_multiple_filters(self):
        clause, params = _build_where({"a": 1, "b": 2})
        assert "a = :_f0" in clause
        assert "b = :_f1" in clause
        assert "AND" in clause

    def test_column_name_injection_blocked(self):
        with pytest.raises(ValueError, match="Invalid column name"):
            _build_where({"status; DROP TABLE x--": "open"})
