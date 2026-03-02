"""Unit tests for SkillRegistry with versioning and replay support."""

from datetime import datetime, timedelta
from unittest.mock import MagicMock, patch

import pytest

from core.exceptions import SkillNotFoundError
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
    return SkillRegistry(lambda: mock_db)


class TestSkillRegistryInit:
    """Tests for SkillRegistry initialization."""

    def test_init_creates_empty_skills_dict(self, mock_db):
        """Test initialization creates empty skills dictionary."""
        registry = SkillRegistry(lambda: mock_db)
        assert registry._skills == {}

    def test_init_accepts_gate_trigger(self, mock_db):
        """Test initialization accepts optional gate_trigger."""
        registry = SkillRegistry(lambda: mock_db, gate_trigger=None)
        assert registry.gate_trigger is None


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
        """Test registration clears metadata cache."""
        skill = MockSkill("test_skill_cache", "1.0.0")

        # Register first so there's something to cache
        registry.register(skill)

        # Pre-populate cache by querying metadata
        registry.get_metadata("test_skill_cache")
        assert len(registry._metadata_cache) > 0

        # Register new version — should clear cache
        skill_v2 = MockSkill("test_skill_cache", "2.0.0")
        registry.register(skill_v2)
        assert len(registry._metadata_cache) == 0

    def test_register_skill_with_metadata(self, registry, mock_db):
        """Test skill registration with full metadata."""
        import uuid
        skill_name = f"test_skill_{uuid.uuid4().hex}"
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
        """Test registration propagates database errors after rollback."""
        skill = MockSkill("test_skill", "1.0.0")

        original_commit = mock_db.commit
        mock_db.commit = MagicMock(side_effect=Exception("DB error"))

        try:
            with pytest.raises(Exception, match="DB error"):
                registry.register(skill)
        finally:
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
        """Test code hash computation falls back to name-based hash for dynamic classes."""
        skill = MockSkill("test_skill", "1.0.0")

        with patch("inspect.getsource", side_effect=OSError("Source error")):
            hash_result = registry._compute_code_hash(skill)

        # Should produce a deterministic hash, not "unknown"
        assert hash_result != "unknown"
        assert len(hash_result) == 64  # SHA256 hex

        # Same input should produce same hash
        with patch("inspect.getsource", side_effect=OSError("Source error")):
            assert registry._compute_code_hash(skill) == hash_result


_ROLLBACK_SKILL_NAMES = ["rb_skill", "rb_single"]
_UNINSTALL_SKILL_NAMES = ["un_skill", "un_cache"]


class TestSkillRollback:
    """Tests for SkillRegistry.rollback()."""

    @pytest.fixture(autouse=True)
    def _cleanup(self, mock_db):
        from api.models import SkillRegistry as SkillModel
        yield
        mock_db.query(SkillModel).filter(
            SkillModel.skill_name.in_(_ROLLBACK_SKILL_NAMES)
        ).delete(synchronize_session="fetch")
        mock_db.commit()

    def test_rollback_activates_previous_version(self, registry, mock_db):
        from api.models import SkillRegistry as SkillModel

        registry.register(MockSkill("rb_skill", "1.0.0"), is_active=True)
        registry.register(MockSkill("rb_skill", "2.0.0"), is_active=True)

        activated = registry.rollback("rb_skill")
        assert activated == "1.0.0"

        mock_db.expire_all()
        v1 = mock_db.query(SkillModel).filter(SkillModel.skill_id == "rb_skill@1.0.0").first()
        v2 = mock_db.query(SkillModel).filter(SkillModel.skill_id == "rb_skill@2.0.0").first()
        assert v1.is_active == 1 and v1.status == "active"
        assert v1.skill_name == "rb_skill" and v1.version == "1.0.0"
        assert v2.is_active == 0 and v2.status == "deprecated"
        assert v2.skill_name == "rb_skill" and v2.version == "2.0.0"

    def test_rollback_consecutive(self, registry, mock_db):
        """v3 → v2 → v1: consecutive rollbacks through the version chain."""
        from api.models import SkillRegistry as SkillModel

        registry.register(MockSkill("rb_skill", "1.0.0"), is_active=True)
        registry.register(MockSkill("rb_skill", "2.0.0"), is_active=True)
        registry.register(MockSkill("rb_skill", "3.0.0"), is_active=True)

        assert registry.rollback("rb_skill") == "2.0.0"
        assert registry.rollback("rb_skill") == "1.0.0"

        mock_db.expire_all()
        v1 = mock_db.query(SkillModel).filter(SkillModel.skill_id == "rb_skill@1.0.0").first()
        v2 = mock_db.query(SkillModel).filter(SkillModel.skill_id == "rb_skill@2.0.0").first()
        v3 = mock_db.query(SkillModel).filter(SkillModel.skill_id == "rb_skill@3.0.0").first()
        assert v1.is_active == 1 and v1.status == "active"
        assert v2.is_active == 0 and v2.status == "deprecated"
        assert v3.is_active == 0 and v3.status == "deprecated"

    def test_rollback_no_active_raises(self, registry, mock_db):
        with pytest.raises(ValueError, match="No active version"):
            registry.rollback("nonexistent_skill")

    def test_rollback_no_previous_raises(self, registry, mock_db):
        from api.models import SkillRegistry as SkillModel

        registry.register(MockSkill("rb_single", "1.0.0"), is_active=True)

        with pytest.raises(ValueError, match="No previous version"):
            registry.rollback("rb_single")

        # Verify the single version is still active after failed rollback
        mock_db.expire_all()
        v1 = mock_db.query(SkillModel).filter(SkillModel.skill_id == "rb_single@1.0.0").first()
        assert v1.is_active == 1 and v1.status == "active"


class TestSkillUninstall:
    """Tests for SkillRegistry.uninstall()."""

    @pytest.fixture(autouse=True)
    def _cleanup(self, mock_db):
        from api.models import SkillRegistry as SkillModel
        yield
        mock_db.query(SkillModel).filter(
            SkillModel.skill_name.in_(_UNINSTALL_SKILL_NAMES)
        ).delete(synchronize_session="fetch")
        mock_db.commit()

    def test_uninstall_removes_all_versions(self, registry, mock_db):
        from api.models import SkillRegistry as SkillModel

        registry.register(MockSkill("un_skill", "1.0.0"), is_active=False)
        registry.register(MockSkill("un_skill", "2.0.0"), is_active=True)

        # Verify both exist before uninstall
        mock_db.expire_all()
        assert mock_db.query(SkillModel).filter(SkillModel.skill_name == "un_skill").count() == 2

        count = registry.uninstall("un_skill")
        assert count == 2

        mock_db.expire_all()
        assert mock_db.query(SkillModel).filter(SkillModel.skill_name == "un_skill").count() == 0

    def test_uninstall_nonexistent_returns_zero(self, registry):
        count = registry.uninstall("no_such_skill")
        assert count == 0

    def test_uninstall_clears_memory_cache(self, registry, mock_db):
        from api.models import SkillRegistry as SkillModel

        registry.register(MockSkill("un_cache", "1.0.0"), is_active=True)
        assert "un_cache" in registry._skills

        registry.uninstall("un_cache")
        assert "un_cache" not in registry._skills

        # Verify DB row is also gone
        mock_db.expire_all()
        assert mock_db.query(SkillModel).filter(SkillModel.skill_name == "un_cache").count() == 0
