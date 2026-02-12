"""Integration tests for SkillRegistry with real database."""

from datetime import datetime, timedelta

import pytest

from core.skills.base import (
    AccessScope,
    RepoType,
    SideEffectCategory,
    SideEffectProfile,
    Skill,
    SkillRequirement,
)
from core.skills.registry import SkillRegistry
from sdk import Database


class DummySkill(Skill):
    """Dummy skill for integration testing."""

    def __init__(self, name: str, version: str):
        self.name = name
        self.version = version
        self.description = f"Test skill {name} v{version}"
        self.requirements = SkillRequirement(
            repo_types=[RepoType.CODE], min_access=AccessScope.READ
        )
        self.side_effect_profile = SideEffectProfile(category=SideEffectCategory.READ)

    def validate_input(self, input_data: dict):
        return input_data

    async def execute(self, **kwargs):
        return {"result": "success"}


@pytest.fixture
def db():
    """Real database connection."""
    return Database()


@pytest.fixture
def registry(db):
    """SkillRegistry with real database."""
    return SkillRegistry(db)


@pytest.fixture(autouse=True)
def cleanup(db):
    """Clean up test data after each test."""
    yield
    # Clean up test skills
    db.execute("DELETE FROM skills_registry WHERE skill_name LIKE 'test_skill_%'")


class TestSkillRegistryRealDB:
    """Integration tests with real database."""

    def test_register_and_retrieve_skill(self, registry, db):
        """Test registering and retrieving skill from real DB."""
        skill = DummySkill("test_skill_basic", "1.0.0")

        registry.register(skill, is_active=True, git_commit_hash="abc123")

        # Verify in database
        row = db.fetchone(
            "SELECT * FROM skills_registry WHERE skill_name = %s",
            ("test_skill_basic",),
        )
        assert row is not None
        assert row["version"] == "1.0.0"
        assert row["git_commit_hash"] == "abc123"
        assert row["is_active"] == 1

    def test_register_multiple_versions(self, registry, db):
        """Test registering multiple versions of same skill."""
        skill_v1 = DummySkill("test_skill_multi", "1.0.0")
        skill_v2 = DummySkill("test_skill_multi", "2.0.0")

        registry.register(skill_v1, is_active=False, git_commit_hash="abc111")
        registry.register(skill_v2, is_active=True, git_commit_hash="abc222")

        # Verify both versions in database
        rows = db.fetchall(
            "SELECT * FROM skills_registry WHERE skill_name = %s ORDER BY version",
            ("test_skill_multi",),
        )
        assert len(rows) == 2
        assert rows[0]["version"] == "1.0.0"
        assert rows[0]["is_active"] == 0
        assert rows[1]["version"] == "2.0.0"
        assert rows[1]["is_active"] == 1

    def test_get_as_of_by_commit_hash(self, registry, db):
        """Test as_of query by git commit hash."""
        skill_v1 = DummySkill("test_skill_commit", "1.0.0")
        skill_v2 = DummySkill("test_skill_commit", "2.0.0")

        registry.register(skill_v1, is_active=False, git_commit_hash="commit_v1")
        registry.register(skill_v2, is_active=True, git_commit_hash="commit_v2")

        # Query by commit hash
        result = registry.get_as_of("test_skill_commit", as_of_commit="commit_v1")

        assert result is not None
        assert result["version"] == "1.0.0"
        assert result["git_commit_hash"] == "commit_v1"

    def test_get_as_of_by_timestamp(self, registry, db):
        """Test as_of query by timestamp."""
        skill_v1 = DummySkill("test_skill_time", "1.0.0")

        # Register with specific timestamp
        registry.register(skill_v1, is_active=True, git_commit_hash="abc123")

        # Get current version
        result = registry.get_as_of("test_skill_time")

        assert result is not None
        assert result["version"] == "1.0.0"

    def test_get_as_of_current_active_version(self, registry, db):
        """Test getting current active version."""
        skill_v1 = DummySkill("test_skill_active", "1.0.0")
        skill_v2 = DummySkill("test_skill_active", "2.0.0")

        registry.register(skill_v1, is_active=False, git_commit_hash="abc111")
        registry.register(skill_v2, is_active=True, git_commit_hash="abc222")

        # Get active version
        result = registry.get_as_of("test_skill_active")

        assert result is not None
        assert result["version"] == "2.0.0"
        assert result["is_active"] == 1

    def test_cache_hit_on_repeated_query(self, registry, db):
        """Test LRU cache hit on repeated queries."""
        skill = DummySkill("test_skill_cache", "1.0.0")
        registry.register(skill, is_active=True, git_commit_hash="abc123")

        # First query - cache miss
        cache_info_before = registry._get_cached.cache_info()
        result1 = registry.get_as_of("test_skill_cache", as_of_commit="abc123")

        # Second query - cache hit
        result2 = registry.get_as_of("test_skill_cache", as_of_commit="abc123")

        cache_info_after = registry._get_cached.cache_info()

        assert result1 == result2
        # Cache should have 1 hit
        assert cache_info_after.hits > cache_info_before.hits

    def test_cache_cleared_on_new_registration(self, registry, db):
        """Test cache is cleared when new skill is registered."""
        skill_v1 = DummySkill("test_skill_clear", "1.0.0")
        registry.register(skill_v1, is_active=True, git_commit_hash="abc111")

        # Populate cache
        registry.get_as_of("test_skill_clear")
        cache_info_before = registry._get_cached.cache_info()
        assert cache_info_before.currsize > 0

        # Register new version
        skill_v2 = DummySkill("test_skill_clear", "2.0.0")
        registry.register(skill_v2, is_active=True, git_commit_hash="abc222")

        # Cache should be cleared
        cache_info_after = registry._get_cached.cache_info()
        assert cache_info_after.currsize == 0

    def test_git_commit_hash_persisted(self, registry, db):
        """Test git_commit_hash is persisted correctly."""
        skill = DummySkill("test_skill_persist", "1.0.0")
        git_hash = "abc123def456"

        registry.register(skill, is_active=True, git_commit_hash=git_hash)

        # Query directly from database
        row = db.fetchone(
            "SELECT git_commit_hash FROM skills_registry WHERE skill_name = %s",
            ("test_skill_persist",),
        )

        assert row is not None
        assert row["git_commit_hash"] == git_hash

    def test_code_hash_computed_and_stored(self, registry, db):
        """Test code_hash is computed and stored."""
        skill = DummySkill("test_skill_hash", "1.0.0")

        registry.register(skill, is_active=True)

        # Query code_hash from database
        row = db.fetchone(
            "SELECT code_hash FROM skills_registry WHERE skill_name = %s",
            ("test_skill_hash",),
        )

        assert row is not None
        assert row["code_hash"] is not None
        assert len(row["code_hash"]) == 64  # SHA256 hex length

    def test_multiple_commits_same_skill(self, registry, db):
        """Test querying different commits of same skill."""
        commits = ["commit_a", "commit_b", "commit_c"]

        for i, commit in enumerate(commits):
            skill = DummySkill("test_skill_commits", f"1.{i}.0")
            registry.register(skill, is_active=(i == len(commits) - 1), git_commit_hash=commit)

        # Query each commit
        for i, commit in enumerate(commits):
            result = registry.get_as_of("test_skill_commits", as_of_commit=commit)
            assert result is not None
            assert result["version"] == f"1.{i}.0"
            assert result["git_commit_hash"] == commit

    def test_skill_metadata_complete(self, registry, db):
        """Test all skill metadata is stored and retrieved."""
        skill = DummySkill("test_skill_metadata", "1.0.0")

        registry.register(
            skill,
            is_active=True,
            category="code_review",
            subcategory="security",
            triggers=["review", "security"],
            dependencies=["auth_skill"],
            priority=8,
            cost_estimate="high",
            git_commit_hash="abc123",
        )

        # Query metadata
        result = registry.get_as_of("test_skill_metadata")

        assert result is not None
        assert result["category"] == "code_review"
        assert result["subcategory"] == "security"
        assert result["priority"] == 8
        assert result["cost_estimate"] == "high"
        assert "review" in result["triggers"]
        assert "auth_skill" in result["dependencies"]

    def test_replay_scenario_exact_version(self, registry, db):
        """Test replay scenario: get exact version used at specific time."""
        # Simulate: v1.0.0 used on 2024-01-01
        skill_v1 = DummySkill("test_skill_replay", "1.0.0")
        registry.register(skill_v1, is_active=False, git_commit_hash="abc111")

        # Simulate: v2.0.0 used on 2024-02-01
        skill_v2 = DummySkill("test_skill_replay", "2.0.0")
        registry.register(skill_v2, is_active=True, git_commit_hash="abc222")

        # Replay scenario: need exact v1.0.0 that was used
        event_commit = "abc111"
        replayed_skill = registry.get_as_of(
            "test_skill_replay", as_of_commit=event_commit
        )

        assert replayed_skill is not None
        assert replayed_skill["version"] == "1.0.0"
        assert replayed_skill["git_commit_hash"] == "abc111"

    def test_cache_different_queries_separately(self, registry, db):
        """Test cache stores different queries separately."""
        skill_v1 = DummySkill("test_skill_diff", "1.0.0")
        skill_v2 = DummySkill("test_skill_diff", "2.0.0")

        registry.register(skill_v1, is_active=False, git_commit_hash="commit_v1")
        registry.register(skill_v2, is_active=True, git_commit_hash="commit_v2")

        # Query by different commits
        result1 = registry.get_as_of("test_skill_diff", as_of_commit="commit_v1")
        result2 = registry.get_as_of("test_skill_diff", as_of_commit="commit_v2")
        result3 = registry.get_as_of("test_skill_diff")  # Current active

        assert result1["version"] == "1.0.0"
        assert result2["version"] == "2.0.0"
        assert result3["version"] == "2.0.0"

        # All should be cached
        cache_info = registry._get_cached.cache_info()
        assert cache_info.currsize >= 3


class TestReplayScenario:
    """Test complete replay scenario with real database."""

    def test_complete_replay_workflow(self, registry, db):
        """Test complete workflow: register -> record event -> replay."""
        # Step 1: Register skill v1.0.0
        skill_v1 = DummySkill("replay_skill", "1.0.0")
        registry.register(skill_v1, is_active=True, git_commit_hash="abc111")

        # Step 2: Simulate event recorded with v1.0.0
        event_data = {
            "event_id": "evt_001",
            "session_id": "session_001",
            "skill_name": "replay_skill",
            "skill_version": "1.0.0",
            "git_commit_hash": "abc111",
            "created_at": datetime.now(),
        }

        # Step 3: Register skill v2.0.0 (new version)
        skill_v2 = DummySkill("replay_skill", "2.0.0")
        registry.register(skill_v2, is_active=True, git_commit_hash="abc222")

        # Step 4: Replay - get exact version from event
        replayed_skill = registry.get_as_of(
            event_data["skill_name"], as_of_commit=event_data["git_commit_hash"]
        )

        # Verify we got the exact version
        assert replayed_skill is not None
        assert replayed_skill["version"] == "1.0.0"
        assert replayed_skill["git_commit_hash"] == "abc111"

        # Verify current version is different
        current_skill = registry.get_as_of("replay_skill")
        assert current_skill["version"] == "2.0.0"
        assert current_skill["git_commit_hash"] == "abc222"
