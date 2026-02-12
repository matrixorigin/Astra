"""Unit tests for SkillRegistry with versioning and replay support."""

from datetime import datetime, timedelta
from unittest.mock import MagicMock, patch

import pytest

from core.exceptions import DatabaseError, SkillNotFoundError
from core.skills.base import (
    AccessScope,
    RepoType,
    SideEffectCategory,
    SideEffectProfile,
    Skill,
    SkillRequirement,
)
from core.skills.registry import SkillRegistry


class MockSkill(Skill):
    """Mock skill for testing."""

    def __init__(self, name: str, version: str):
        self.name = name
        self.version = version
        self.description = f"Test skill {name}"
        self.requirements = SkillRequirement(
            repo_types=[RepoType.CODE], min_access=AccessScope.READ
        )
        self.side_effect_profile = SideEffectProfile(category=SideEffectCategory.READ)

    def validate_input(self, input_data: dict):
        return input_data

    async def execute(self, **kwargs):
        return {"result": "success"}


@pytest.fixture
def mock_db():
    """Mock database."""
    db = MagicMock()
    db.fetchone.return_value = None
    db.fetchall.return_value = []
    return db


@pytest.fixture
def registry(mock_db):
    """Create SkillRegistry with mocked database."""
    return SkillRegistry(mock_db)


class TestSkillRegistryInit:
    """Tests for SkillRegistry initialization."""

    def test_init_creates_empty_skills_dict(self, mock_db):
        """Test initialization creates empty skills dictionary."""
        registry = SkillRegistry(mock_db)
        assert registry._skills == {}
        assert registry.db == mock_db

    def test_init_sets_cache_size(self, mock_db):
        """Test initialization sets LRU cache size."""
        registry = SkillRegistry(mock_db)
        assert registry._cache_size == 100


class TestSkillRegistration:
    """Tests for skill registration."""

    def test_register_skill_basic(self, registry, mock_db):
        """Test basic skill registration."""
        skill = MockSkill("test_skill", "1.0.0")

        registry.register(skill)

        # Verify database calls
        assert mock_db.execute.call_count == 2  # UPDATE + INSERT
        insert_call = mock_db.execute.call_args_list[1]
        assert "INSERT INTO skills_registry" in insert_call[0][0]
        assert insert_call[0][1][0] == "test_skill@1.0.0"  # skill_id
        assert insert_call[0][1][1] == "test_skill"  # skill_name
        assert insert_call[0][1][2] == "1.0.0"  # version

    def test_register_skill_with_git_commit_hash(self, registry, mock_db):
        """Test skill registration with git commit hash."""
        skill = MockSkill("test_skill", "1.0.0")
        git_hash = "abc123def456"

        registry.register(skill, git_commit_hash=git_hash)

        insert_call = mock_db.execute.call_args_list[1]
        # git_commit_hash is the 7th parameter (index 6)
        assert insert_call[0][1][6] == git_hash

    def test_register_skill_deactivates_old_versions(self, registry, mock_db):
        """Test registration deactivates old versions when is_active=True."""
        skill = MockSkill("test_skill", "2.0.0")

        registry.register(skill, is_active=True)

        # First call should be UPDATE to deactivate old versions
        update_call = mock_db.execute.call_args_list[0]
        assert "UPDATE skills_registry" in update_call[0][0]
        assert "SET is_active = 0" in update_call[0][0]
        assert update_call[0][1] == ("test_skill",)

    def test_register_skill_stores_in_memory(self, registry, mock_db):
        """Test skill is stored in memory after registration."""
        skill = MockSkill("test_skill", "1.0.0")

        registry.register(skill, is_active=True)

        assert "test_skill@1.0.0" in registry._skills
        assert registry._skills["test_skill@1.0.0"] == skill
        # Active version shortcut
        assert registry._skills["test_skill"] == skill

    def test_register_skill_clears_cache(self, registry, mock_db):
        """Test registration clears LRU cache."""
        skill = MockSkill("test_skill", "1.0.0")

        # Pre-populate cache by calling _get_cached
        registry._get_cached("test_skill")
        cache_info_before = registry._get_cached.cache_info()
        assert cache_info_before.currsize > 0

        # Register skill
        registry.register(skill)

        # Cache should be cleared
        cache_info_after = registry._get_cached.cache_info()
        assert cache_info_after.currsize == 0

    def test_register_skill_with_metadata(self, registry, mock_db):
        """Test skill registration with full metadata."""
        skill = MockSkill("test_skill", "1.0.0")

        registry.register(
            skill,
            is_active=True,
            category="github",
            subcategory="issues",
            triggers=["create", "issue"],
            dependencies=["auth_skill"],
            priority=8,
            cost_estimate="high",
            git_commit_hash="abc123",
        )

        insert_call = mock_db.execute.call_args_list[1]
        params = insert_call[0][1]

        # Verify all parameters
        assert params[8] == "github"  # category
        assert params[9] == "issues"  # subcategory
        assert '"create"' in params[10]  # triggers (JSON)
        assert '"auth_skill"' in params[11]  # dependencies (JSON)
        assert params[12] == 8  # priority
        assert params[13] == "high"  # cost_estimate

    def test_register_skill_database_error(self, registry, mock_db):
        """Test registration handles database errors."""
        skill = MockSkill("test_skill", "1.0.0")
        mock_db.execute.side_effect = Exception("DB error")

        with pytest.raises(DatabaseError, match="Failed to register skill"):
            registry.register(skill)


class TestSkillRetrieval:
    """Tests for skill retrieval."""

    def test_get_skill_by_name_and_version(self, registry):
        """Test getting skill by name and version."""
        skill = MockSkill("test_skill", "1.0.0")
        registry._skills["test_skill@1.0.0"] = skill

        result = registry.get("test_skill", "1.0.0")

        assert result == skill

    def test_get_active_skill_by_name_only(self, registry):
        """Test getting active skill by name only."""
        skill = MockSkill("test_skill", "2.0.0")
        registry._skills["test_skill"] = skill

        result = registry.get("test_skill")

        assert result == skill

    def test_get_skill_not_found_raises_error(self, registry):
        """Test getting non-existent skill raises SkillNotFoundError."""
        with pytest.raises(SkillNotFoundError):
            registry.get("nonexistent_skill")

    def test_get_skill_version_not_found_raises_error(self, registry):
        """Test getting non-existent version raises SkillNotFoundError."""
        with pytest.raises(SkillNotFoundError):
            registry.get("test_skill", "99.0.0")


class TestHistoricalQueries:
    """Tests for as_of historical queries."""

    def test_get_as_of_by_commit_hash(self, registry, mock_db):
        """Test getting skill by git commit hash."""
        mock_db.fetchone.return_value = {
            "skill_id": "test_skill@1.0.0",
            "skill_name": "test_skill",
            "version": "1.0.0",
            "git_commit_hash": "abc123",
        }

        result = registry.get_as_of("test_skill", as_of_commit="abc123")

        assert result is not None
        assert result["git_commit_hash"] == "abc123"
        mock_db.fetchone.assert_called_once()
        call_args = mock_db.fetchone.call_args
        assert "git_commit_hash = %s" in call_args[0][0]
        assert call_args[0][1] == ("test_skill", "abc123")

    def test_get_as_of_by_timestamp(self, registry, mock_db):
        """Test getting skill by timestamp."""
        timestamp = datetime(2024, 1, 1, 12, 0, 0)
        mock_db.fetchone.return_value = {
            "skill_id": "test_skill@1.0.0",
            "skill_name": "test_skill",
            "version": "1.0.0",
            "created_at": timestamp,
        }

        result = registry.get_as_of("test_skill", as_of_timestamp=timestamp)

        assert result is not None
        mock_db.fetchone.assert_called_once()
        call_args = mock_db.fetchone.call_args
        assert "created_at <= %s" in call_args[0][0]

    def test_get_as_of_current_version(self, registry, mock_db):
        """Test getting current active version without as_of parameters."""
        mock_db.fetchone.return_value = {
            "skill_id": "test_skill@2.0.0",
            "skill_name": "test_skill",
            "version": "2.0.0",
            "is_active": 1,
        }

        result = registry.get_as_of("test_skill")

        assert result is not None
        assert result["version"] == "2.0.0"
        call_args = mock_db.fetchone.call_args
        assert "is_active = 1" in call_args[0][0]

    def test_get_as_of_not_found(self, registry, mock_db):
        """Test as_of query returns None when skill not found."""
        mock_db.fetchone.return_value = None

        result = registry.get_as_of("nonexistent_skill", as_of_commit="abc123")

        assert result is None

    def test_get_as_of_handles_database_error(self, registry, mock_db):
        """Test as_of query handles database errors gracefully."""
        mock_db.fetchone.side_effect = Exception("DB error")

        result = registry.get_as_of("test_skill", as_of_commit="abc123")

        assert result is None


class TestLRUCache:
    """Tests for LRU cache functionality."""

    def test_cache_hit_on_repeated_query(self, registry, mock_db):
        """Test cache hit on repeated queries."""
        mock_db.fetchone.return_value = {"skill_name": "test_skill", "version": "1.0.0"}

        # First call - cache miss
        result1 = registry.get_as_of("test_skill")
        # Second call - cache hit
        result2 = registry.get_as_of("test_skill")

        assert result1 == result2
        # Database should only be called once
        assert mock_db.fetchone.call_count == 1

    def test_cache_different_queries(self, registry, mock_db):
        """Test cache stores different queries separately."""
        mock_db.fetchone.side_effect = [
            {"skill_name": "skill1", "version": "1.0.0"},
            {"skill_name": "skill2", "version": "1.0.0"},
        ]

        result1 = registry.get_as_of("skill1")
        result2 = registry.get_as_of("skill2")

        assert result1["skill_name"] == "skill1"
        assert result2["skill_name"] == "skill2"
        assert mock_db.fetchone.call_count == 2

    def test_cache_by_commit_vs_timestamp(self, registry, mock_db):
        """Test cache distinguishes between commit and timestamp queries."""
        mock_db.fetchone.side_effect = [
            {"skill_name": "test_skill", "git_commit_hash": "abc123"},
            {"skill_name": "test_skill", "created_at": "2024-01-01"},
        ]

        result1 = registry.get_as_of("test_skill", as_of_commit="abc123")
        result2 = registry.get_as_of("test_skill", as_of_timestamp=datetime(2024, 1, 1))

        assert result1 != result2
        assert mock_db.fetchone.call_count == 2

    def test_cache_cleared_on_registration(self, registry, mock_db):
        """Test cache is cleared when new skill is registered."""
        # Pre-populate cache
        mock_db.fetchone.return_value = {"skill_name": "test_skill", "version": "1.0.0"}
        registry.get_as_of("test_skill")

        # Register new version
        skill = MockSkill("test_skill", "2.0.0")
        registry.register(skill)

        # Cache should be cleared
        cache_info = registry._get_cached.cache_info()
        assert cache_info.currsize == 0


class TestCodeHash:
    """Tests for code hash computation."""

    def test_compute_code_hash_deterministic(self, registry):
        """Test code hash is deterministic for same skill."""
        skill1 = MockSkill("test_skill", "1.0.0")
        skill2 = MockSkill("test_skill", "1.0.0")

        hash1 = registry._compute_code_hash(skill1)
        hash2 = registry._compute_code_hash(skill2)

        assert hash1 == hash2
        assert len(hash1) == 64  # SHA256 hex length

    def test_compute_code_hash_different_skills(self, registry):
        """Test different skills produce different hashes."""

        class DifferentSkill(Skill):
            def __init__(self):
                self.name = "different"
                self.version = "1.0.0"
                self.description = "Different skill"
                self.requirements = SkillRequirement(
                    repo_types=[RepoType.CODE], min_access=AccessScope.READ
                )
                self.side_effect_profile = SideEffectProfile(category=SideEffectCategory.READ)

            def validate_input(self, input_data: dict):
                return input_data

            async def execute(self, **kwargs):
                return {"different": "result"}

        skill1 = MockSkill("test_skill", "1.0.0")
        skill2 = DifferentSkill()

        hash1 = registry._compute_code_hash(skill1)
        hash2 = registry._compute_code_hash(skill2)

        assert hash1 != hash2

    def test_compute_code_hash_handles_error(self, registry):
        """Test code hash computation handles errors gracefully."""
        skill = MockSkill("test_skill", "1.0.0")

        with patch("inspect.getsource", side_effect=Exception("Source error")):
            hash_result = registry._compute_code_hash(skill)

        assert hash_result == "unknown"


class TestListAvailable:
    """Tests for listing available skills."""

    def test_list_available_filters_by_repo_type(self, registry, mock_db):
        """Test listing skills filters by repository type."""
        mock_db.fetchone.return_value = {"repo_type": "code", "access_scope": "read"}

        skill1 = MockSkill("code_skill", "1.0.0")
        skill1.requirements.repo_types = [RepoType.CODE]
        registry._skills["code_skill"] = skill1

        skill2 = MockSkill("docs_skill", "1.0.0")
        skill2.requirements.repo_types = [RepoType.DOCS]
        registry._skills["docs_skill"] = skill2

        available = registry.list_available(repo_id=1)

        assert len(available) == 1
        assert available[0].name == "code_skill"

    def test_list_available_filters_by_access_scope(self, registry, mock_db):
        """Test listing skills filters by access scope."""
        mock_db.fetchone.return_value = {"repo_type": "code", "access_scope": "read"}

        skill1 = MockSkill("read_skill", "1.0.0")
        skill1.requirements.min_access = AccessScope.READ
        registry._skills["read_skill"] = skill1

        skill2 = MockSkill("write_skill", "1.0.0")
        skill2.requirements.min_access = AccessScope.WRITE
        registry._skills["write_skill"] = skill2

        available = registry.list_available(repo_id=1)

        assert len(available) == 1
        assert available[0].name == "read_skill"

    def test_list_available_repo_not_found(self, registry, mock_db):
        """Test listing skills returns empty list for non-existent repo."""
        mock_db.fetchone.return_value = None

        available = registry.list_available(repo_id=999)

        assert available == []

    def test_list_available_skips_versioned_keys(self, registry, mock_db):
        """Test listing skills skips versioned keys (only checks active)."""
        mock_db.fetchone.return_value = {"repo_type": "code", "access_scope": "read"}

        skill = MockSkill("test_skill", "1.0.0")
        skill.requirements.repo_types = [RepoType.CODE]
        registry._skills["test_skill"] = skill
        registry._skills["test_skill@1.0.0"] = skill  # Versioned key

        available = registry.list_available(repo_id=1)

        # Should only count once (active version)
        assert len(available) == 1
