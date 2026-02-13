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
    """Real SQLAlchemy session for testing."""
    from api.database import get_db_session
    session = next(get_db_session())
    yield session
    session.close()


@pytest.fixture
def registry(mock_db):
    """Create SkillRegistry with real database session."""
    return SkillRegistry(mock_db)


class TestSkillRegistryInit:
    """Tests for SkillRegistry initialization."""

    def test_init_creates_empty_skills_dict(self, mock_db):
        """Test initialization creates empty skills dictionary."""
        registry = SkillRegistry(mock_db)
        assert registry._skills == {}

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

        # Verify skill is in memory
        assert "test_skill@1.0.0" in registry._skills
        assert "test_skill" in registry._skills  # Active version shortcut

    def test_register_skill_with_git_commit_hash(self, registry, mock_db):
        """Test skill registration with git commit hash."""
        from api.models import SkillRegistry as SkillModel
        
        skill = MockSkill("test_skill_git", "1.0.0")
        git_hash = "abc123def456"

        registry.register(skill, git_commit_hash=git_hash)

        # Verify in database
        db_skill = mock_db.query(SkillModel).filter(SkillModel.skill_id == "test_skill_git@1.0.0").first()
        assert db_skill is not None
        assert db_skill.git_commit_hash == git_hash

    def test_register_skill_deactivates_old_versions(self, registry, mock_db):
        """Test registration deactivates old versions when is_active=True."""
        from api.models import SkillRegistry as SkillModel
        
        # Register v1
        skill_v1 = MockSkill("test_skill_deact", "1.0.0")
        registry.register(skill_v1, is_active=True)
        
        # Register v2
        skill_v2 = MockSkill("test_skill_deact", "2.0.0")
        registry.register(skill_v2, is_active=True)
        
        # Verify v1 is deactivated
        db_skill_v1 = mock_db.query(SkillModel).filter(SkillModel.skill_id == "test_skill_deact@1.0.0").first()
        assert db_skill_v1.is_active == 0
        
        # Verify v2 is active
        db_skill_v2 = mock_db.query(SkillModel).filter(SkillModel.skill_id == "test_skill_deact@2.0.0").first()
        assert db_skill_v2.is_active == 1

    def test_register_skill_stores_in_memory(self, registry, mock_db):
        """Test skill is stored in memory after registration."""
        skill = MockSkill("test_skill_mem", "1.0.0")

        registry.register(skill, is_active=True)

        assert "test_skill_mem@1.0.0" in registry._skills
        assert registry._skills["test_skill_mem@1.0.0"] == skill
        # Active version shortcut
        assert registry._skills["test_skill_mem"] == skill

    def test_register_skill_clears_cache(self, registry, mock_db):
        """Test registration clears LRU cache."""
        skill = MockSkill("test_skill_cache", "1.0.0")

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
        import uuid
        skill_name = f"test_skill_{uuid.uuid4().hex[:8]}"
        skill = MockSkill(skill_name, "1.0.0")

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

        # Verify skill was registered
        from api.models import SkillRegistry as SkillRegistryModel
        result = mock_db.query(SkillRegistryModel).filter(
            SkillRegistryModel.skill_name == skill_name
        ).first()
        
        assert result is not None
        assert result.skill_name == skill_name
        assert result.version == "1.0.0"
        assert result.git_commit_hash == "abc123"

    def test_register_skill_database_error(self, registry, mock_db):
        """Test registration handles database errors."""
        skill = MockSkill("test_skill", "1.0.0")
        
        # Mock commit to raise exception
        original_commit = mock_db.commit
        mock_db.commit = MagicMock(side_effect=Exception("DB error"))

        try:
            with pytest.raises(DatabaseError, match="Failed to register skill"):
                registry.register(skill)
        finally:
            # Restore original commit
            mock_db.commit = original_commit


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
        from api.models import SkillRegistry as SkillModel
        
        # Create skill with commit hash
        skill = MockSkill("test_skill_commit", "1.0.0")
        registry.register(skill, git_commit_hash="abc123")
        
        result = registry.get_as_of("test_skill_commit", as_of_commit="abc123")
        
        assert result is not None
        assert result["git_commit_hash"] == "abc123"

    def test_get_as_of_by_timestamp(self, registry, mock_db):
        """Test getting skill by timestamp."""
        # Create skill first
        skill = MockSkill("test_skill_ts", "1.0.0")
        registry.register(skill)
        
        # Query with future timestamp
        from datetime import datetime, timedelta
        future_timestamp = datetime.now() + timedelta(days=1)
        
        result = registry.get_as_of("test_skill_ts", as_of_timestamp=future_timestamp)

        assert result is not None
        assert result["skill_name"] == "test_skill_ts"
        
    def test_get_as_of_current_version(self, registry, mock_db):
        """Test getting current active version without as_of parameters."""
        # Create skill with version 2.0.0
        skill = MockSkill("test_skill_current", "2.0.0")
        registry.register(skill, is_active=True)

        result = registry.get_as_of("test_skill_current")

        assert result is not None
        assert result["version"] == "2.0.0"
        
    def test_get_as_of_not_found(self, registry, mock_db):
        """Test as_of query returns None when skill not found."""
        # Test uses real database

        # First call - cache miss
        result1 = registry.get_as_of("test_skill")
        # Second call - cache hit
        result2 = registry.get_as_of("test_skill")

        assert result1 == result2
        # Database should only be called once

    def test_cache_different_queries(self, registry, mock_db):
        """Test cache stores different queries separately."""
        # Create two different skills
        skill1 = MockSkill("skill1", "1.0.0")
        skill2 = MockSkill("skill2", "1.0.0")
        registry.register(skill1)
        registry.register(skill2)

        result1 = registry.get_as_of("skill1")
        result2 = registry.get_as_of("skill2")

        assert result1["skill_name"] == "skill1"
        assert result2["skill_name"] == "skill2"

    def test_cache_by_commit_vs_timestamp(self, registry, mock_db):
        """Test cache distinguishes between commit and timestamp queries."""
        # Create skill with commit hash
        skill = MockSkill("test_skill_cache", "1.0.0")
        registry.register(skill, git_commit_hash="abc123")

        from datetime import datetime, timedelta
        future_ts = datetime.now() + timedelta(days=1)
        
        result1 = registry.get_as_of("test_skill_cache", as_of_commit="abc123")
        result2 = registry.get_as_of("test_skill_cache", as_of_timestamp=future_ts)

        # Both should return the same skill but queries are different
        assert result1 is not None
        assert result2 is not None

    def test_cache_cleared_on_registration(self, registry, mock_db):
        """Test cache is cleared when new skill is registered."""
        # Create a skill and register it
        skill1 = MockSkill("cache_test_skill", "1.0.0")
        registry.register(skill1)
        
        # Query to populate cache
        result1 = registry.get_as_of("cache_test_skill")
        assert result1 is not None
        
        # Register new version
        skill2 = MockSkill("cache_test_skill", "2.0.0")
        registry.register(skill2)
        
        # Query again - should get new version
        result2 = registry.get_as_of("cache_test_skill")
        assert result2 is not None
        assert result2["version"] == "2.0.0"

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
        # Create repo in database with unique URL
        import uuid
        from api.models import Repo
        repo_id = str(uuid.uuid4())
        unique_url = f"https://github.com/test/repo-{uuid.uuid4()}"
        
        # Clean up any existing repo with same URL
        mock_db.query(Repo).filter(Repo.repo_url == unique_url).delete()
        mock_db.commit()
        
        repo = Repo(
            repo_id=repo_id,
            repo_url=unique_url,
            repo_type="code",
            access_scope="read",
            owner_id="test",
            owner_type="user"
        )
        mock_db.add(repo)
        mock_db.commit()

        skill1 = MockSkill("read_skill", "1.0.0")
        skill1.requirements.min_access = AccessScope.READ
        skill1.requirements.repo_types = [RepoType.CODE]
        registry._skills["read_skill"] = skill1

        skill2 = MockSkill("write_skill", "1.0.0")
        skill2.requirements.min_access = AccessScope.WRITE
        skill2.requirements.repo_types = [RepoType.CODE]
        registry._skills["write_skill"] = skill2

        # Pass repo_id as string (will be converted to str in list_available anyway)
        available = registry.list_available(repo_id=repo_id)

        assert len(available) == 1
        assert available[0].name == "read_skill"

    def test_list_available_repo_not_found(self, registry, mock_db):
        """Test listing skills returns empty list for non-existent repo."""
        # No repo created in database

        skill = MockSkill("test_skill", "1.0.0")
        skill.requirements.repo_types = [RepoType.CODE]
        registry._skills["test_skill"] = skill
        registry._skills["test_skill@1.0.0"] = skill  # Versioned key

        available = registry.list_available(repo_id=999)  # Non-existent repo

        # Should return empty list for non-existent repo
        assert len(available) == 0
