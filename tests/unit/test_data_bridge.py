"""Tests for core/skills/data_bridge.py — cross-skill data access via ORM."""

from unittest.mock import MagicMock, patch

import pytest
from sqlalchemy import Column, Integer, MetaData, String, Table

from core.skills.data_bridge import SkillDataBridge, _apply_filters
from core.skills.loader import SkillManifest


# ── Fixtures ─────────────────────────────────────────────────────

@pytest.fixture
def manifests() -> dict[str, SkillManifest]:
    return {
        "knowledge": SkillManifest(name="knowledge", version="1.0", depends_on=["github"]),
        "github": SkillManifest(name="github", version="1.0", depends_on=[]),
    }


@pytest.fixture
def fake_table():
    """Create a real SQLAlchemy Table object for testing."""
    metadata = MetaData()
    return Table(
        "sk_github_repos", metadata,
        Column("repo_id", String(36), primary_key=True),
        Column("owner", String(100)),
        Column("name", String(100)),
    )


@pytest.fixture
def bridge(manifests, fake_table):
    db = MagicMock()
    with patch("core.skills.data_bridge.Base") as mock_base:
        mock_base.metadata.tables = {"sk_github_repos": fake_table}
        b = SkillDataBridge(
            db=db,
            requesting_skill="knowledge",
            manifest_loader=lambda name: manifests[name],
        )
        # Keep the mock active for later calls
        b._base_tables = mock_base.metadata.tables
    return b


# ── Validation ───────────────────────────────────────────────────

class TestValidation:
    def test_unauthorized_skill_raises(self, bridge):
        with pytest.raises(PermissionError, match="does not declare dependency"):
            bridge.query("jira", "sk_jira_issues")

    def test_wrong_table_prefix_raises(self, bridge):
        with pytest.raises(ValueError, match="does not belong to skill"):
            bridge.query("github", "sk_jira_issues")

    def test_table_not_in_metadata_raises(self, bridge):
        with pytest.raises(ValueError, match="not found in ORM metadata"):
            bridge.query("github", "sk_github_nonexistent")

    def test_unknown_skill_in_loader_raises(self, manifests):
        db = MagicMock()
        with pytest.raises(KeyError):
            SkillDataBridge(db=db, requesting_skill="nonexistent",
                            manifest_loader=lambda name: manifests[name])

    def test_count_unauthorized_raises(self, bridge):
        with pytest.raises(PermissionError):
            bridge.count("jira", "sk_jira_issues")

    def test_count_wrong_prefix_raises(self, bridge):
        with pytest.raises(ValueError):
            bridge.count("github", "wrong_table")


# ── Query ────────────────────────────────────────────────────────

class TestQuery:
    def test_query_returns_dicts(self, bridge, fake_table):
        with patch("core.skills.data_bridge.Base") as mock_base:
            mock_base.metadata.tables = {"sk_github_repos": fake_table}
            bridge._db.execute.return_value.mappings.return_value.all.return_value = [
                {"repo_id": "1", "owner": "alice", "name": "repo1"},
            ]
            result = bridge.query("github", "sk_github_repos")
        assert len(result) == 1
        assert result[0]["owner"] == "alice"

    def test_query_with_filters(self, bridge, fake_table):
        with patch("core.skills.data_bridge.Base") as mock_base:
            mock_base.metadata.tables = {"sk_github_repos": fake_table}
            bridge._db.execute.return_value.mappings.return_value.all.return_value = []
            bridge.query("github", "sk_github_repos", filters={"owner": "alice"})
        # Verify execute was called (ORM builds parameterized query)
        bridge._db.execute.assert_called_once()
        stmt = bridge._db.execute.call_args[0][0]
        sql = str(stmt.compile(compile_kwargs={"literal_binds": False}))
        assert "sk_github_repos.owner = :owner_1" in sql

    def test_query_with_limit(self, bridge, fake_table):
        with patch("core.skills.data_bridge.Base") as mock_base:
            mock_base.metadata.tables = {"sk_github_repos": fake_table}
            bridge._db.execute.return_value.mappings.return_value.all.return_value = []
            bridge.query("github", "sk_github_repos", limit=5)
        stmt = bridge._db.execute.call_args[0][0]
        sql = str(stmt.compile(compile_kwargs={"literal_binds": False}))
        assert "LIMIT" in sql.upper()

    def test_query_invalid_column_raises(self, bridge, fake_table):
        with patch("core.skills.data_bridge.Base") as mock_base:
            mock_base.metadata.tables = {"sk_github_repos": fake_table}
            with pytest.raises(ValueError, match="Column.*not found"):
                bridge.query("github", "sk_github_repos", filters={"nonexistent": "x"})

    def test_count_returns_int(self, bridge, fake_table):
        with patch("core.skills.data_bridge.Base") as mock_base:
            mock_base.metadata.tables = {"sk_github_repos": fake_table}
            bridge._db.execute.return_value.scalar.return_value = 42
            result = bridge.count("github", "sk_github_repos")
        assert result == 42

    def test_count_returns_zero_on_none(self, bridge, fake_table):
        with patch("core.skills.data_bridge.Base") as mock_base:
            mock_base.metadata.tables = {"sk_github_repos": fake_table}
            bridge._db.execute.return_value.scalar.return_value = None
            result = bridge.count("github", "sk_github_repos")
        assert result == 0

    def test_count_with_filters(self, bridge, fake_table):
        with patch("core.skills.data_bridge.Base") as mock_base:
            mock_base.metadata.tables = {"sk_github_repos": fake_table}
            bridge._db.execute.return_value.scalar.return_value = 3
            bridge.count("github", "sk_github_repos", filters={"owner": "bob"})
        stmt = bridge._db.execute.call_args[0][0]
        sql = str(stmt.compile(compile_kwargs={"literal_binds": False}))
        assert "count" in sql.lower()
        assert "owner" in sql


# ── _apply_filters ───────────────────────────────────────────────

class TestApplyFilters:
    def test_no_filters_returns_unchanged(self, fake_table):
        from sqlalchemy import select
        stmt = select(fake_table)
        result = _apply_filters(stmt, fake_table, None)
        assert str(result) == str(stmt)

    def test_empty_dict_returns_unchanged(self, fake_table):
        from sqlalchemy import select
        stmt = select(fake_table)
        result = _apply_filters(stmt, fake_table, {})
        assert str(result) == str(stmt)

    def test_valid_filter_adds_where(self, fake_table):
        from sqlalchemy import select
        stmt = select(fake_table)
        result = _apply_filters(stmt, fake_table, {"owner": "alice"})
        sql = str(result.compile(compile_kwargs={"literal_binds": False}))
        assert "owner" in sql and "WHERE" in sql

    def test_invalid_column_raises(self, fake_table):
        from sqlalchemy import select
        stmt = select(fake_table)
        with pytest.raises(ValueError, match="Column.*not found"):
            _apply_filters(stmt, fake_table, {"bad_col": "x"})

    def test_multiple_filters(self, fake_table):
        from sqlalchemy import select
        stmt = select(fake_table)
        result = _apply_filters(stmt, fake_table, {"owner": "a", "name": "b"})
        sql = str(result.compile(compile_kwargs={"literal_binds": False}))
        assert "owner" in sql and "name" in sql
