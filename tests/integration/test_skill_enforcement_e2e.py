"""P2: Skill Runtime Enforcement — installation check + dependency resolution.

Tests verify:
1. execute_skill blocks uninstalled marketplace skills
2. execute_skill allows builtin skills (not in skill_definitions)
3. install() checks depends_on
4. require_installed raises for missing installation
"""

import pytest
from unittest.mock import MagicMock

from sqlalchemy import text

from api.models import SkillRegistry
from core.skills.skill_manager import (
    SkillManager,
    SkillNotFoundError,
    SkillNotInstalledError,
    PermissionDeniedError,
)
from core.skills.credential_manager import CredentialManager


def _uuid():
    import uuid
    return str(uuid.uuid4())


def _now():
    from datetime import datetime, timezone
    return datetime.now(timezone.utc)


@pytest.fixture
def cred_mgr():
    return CredentialManager("test-secret-key-for-unit-tests")


@pytest.fixture
def mgr(db_factory, cred_mgr):
    return SkillManager(db_factory, cred_mgr)


@pytest.fixture
def _seed_skill(db_session):
    """Insert a marketplace skill definition + public permission."""
    skill_id = _uuid()
    db_session.add(SkillRegistry(
        skill_id=skill_id,
        skill_name="github",
        version="1.0.0",
        description="GitHub integration",
        manifest={"depends_on": [], "credentials": []},
        is_active=True,
        is_public=True,
        source="marketplace",
        created_by="admin",
        created_at=_now(),
    ))
    db_session.commit()
    yield "github"
    db_session.execute(text("DELETE FROM skill_installations WHERE skill_name = 'github'"))
    db_session.execute(text("DELETE FROM skills_registry WHERE skill_id = :id"), {"id": skill_id})
    db_session.commit()


@pytest.fixture
def _seed_skills_with_dep(db_session):
    """Insert two skills: 'base_skill' and 'dependent_skill' (depends on base_skill)."""
    ids = [_uuid(), _uuid()]
    db_session.add(SkillRegistry(
        skill_id=ids[0], skill_name="base_skill", version="1.0.0",
        description="Base", manifest={"depends_on": []},
        is_active=True, is_public=True, source="marketplace", created_by="admin", created_at=_now(),
    ))
    db_session.add(SkillRegistry(
        skill_id=ids[1], skill_name="dependent_skill", version="1.0.0",
        description="Depends on base", manifest={"depends_on": ["base_skill"]},
        is_active=True, is_public=True, source="marketplace", created_by="admin", created_at=_now(),
    ))
    db_session.commit()
    yield
    db_session.execute(text("DELETE FROM skill_installations WHERE skill_name IN ('base_skill', 'dependent_skill')"))
    for sid in ids:
        db_session.execute(text("DELETE FROM skills_registry WHERE skill_id = :id"), {"id": sid})
    db_session.commit()


# ── SkillManager.require_installed ────────────────────────────────────────


class TestRequireInstalled:

    def test_raises_when_not_installed(self, mgr, _seed_skill):
        with pytest.raises(SkillNotInstalledError, match="not installed"):
            mgr.require_installed("user-1", "github")

    def test_passes_when_installed(self, mgr, _seed_skill):
        mgr.install("user-1", "github")
        mgr.require_installed("user-1", "github")  # should not raise

    def test_raises_after_uninstall(self, mgr, _seed_skill):
        mgr.install("user-1", "github")
        mgr.uninstall("user-1", "github")
        with pytest.raises(SkillNotInstalledError):
            mgr.require_installed("user-1", "github")


# ── Dependency resolution on install ──────────────────────────────────────


class TestDependencyResolution:

    def test_install_fails_without_dependency(self, mgr, _seed_skills_with_dep):
        with pytest.raises(SkillNotInstalledError, match="base_skill.*must be installed"):
            mgr.install("user-1", "dependent_skill")

    def test_install_succeeds_with_dependency(self, mgr, _seed_skills_with_dep):
        mgr.install("user-1", "base_skill")
        inst = mgr.install("user-1", "dependent_skill")
        assert inst.skill_name == "dependent_skill"
        assert inst.status == "installed"


# ── AgentExecutor enforcement ─────────────────────────────────────────────


class TestExecutorEnforcement:

    def test_marketplace_skill_blocked_without_install(self, db_factory, cred_mgr, _seed_skill):
        """execute_skill raises SkillNotInstalledError for uninstalled marketplace skill."""
        from core.agent.executor import AgentExecutor
        from core.skills.registry import SkillRegistry
        from core.skills.builtin import register_builtin_skills
        from core.runtime import create_runtime, IsolationLevel
        from core.code_executor import CodeExecutor

        registry = SkillRegistry(db_factory)
        code_executor = CodeExecutor(
            runtime=create_runtime(min_isolation=IsolationLevel.PROCESS), db_factory=db_factory,
        )
        register_builtin_skills(registry, db_factory, code_executor=code_executor)

        # Register a fake "github" skill in the in-memory registry
        mock_skill = MagicMock()
        mock_skill.name = "github"
        mock_skill.version = "1.0.0"
        mock_skill.description = "test"
        mock_skill.input_schema = type("S", (), {"model_json_schema": classmethod(lambda cls: {})})
        registry._skills["github"] = mock_skill

        skill_mgr = SkillManager(db_factory, cred_mgr)
        executor = AgentExecutor(db_factory, registry, skill_manager=skill_mgr)

        with pytest.raises(SkillNotInstalledError):
            executor.execute_skill(
                skill_name="github",
                params={"user_id": "user-1"},
                session_id="sess-1",
            )

    def test_builtin_skill_allowed_without_install(self, db_factory, cred_mgr):
        """Builtin skills (not in skill_definitions) execute without installation check."""
        from core.agent.executor import AgentExecutor
        from core.skills.registry import SkillRegistry
        from core.skills.builtin import register_builtin_skills
        from core.runtime import create_runtime, IsolationLevel
        from core.code_executor import CodeExecutor

        registry = SkillRegistry(db_factory)
        code_executor = CodeExecutor(
            runtime=create_runtime(min_isolation=IsolationLevel.PROCESS), db_factory=db_factory,
        )
        register_builtin_skills(registry, db_factory, code_executor=code_executor)

        skill_mgr = SkillManager(db_factory, cred_mgr)
        executor = AgentExecutor(db_factory, registry, skill_manager=skill_mgr)

        # execute_code is a builtin skill — should not raise SkillNotInstalledError
        # It may raise other errors (sandbox, runtime, etc.) which are fine
        try:
            executor.execute_skill(
                skill_name="execute_code",
                params={"user_id": "user-1", "code": "print(1)", "language": "python"},
                session_id="sess-1",
            )
        except SkillNotInstalledError:
            pytest.fail("Builtin skill should not require installation")
        except Exception:
            pass  # Other errors (sandbox, runtime) are expected and fine
