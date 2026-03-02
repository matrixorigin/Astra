"""Tests for versioned dependency resolution in SkillManager.install().

Covers the new code paths added by the dependency versioning feature:
- install() with new-format depends_on (dict with version/type)
- Version conflict rejection at install time
- Missing versioned dependency rejection
- Circular dependency rejection at install time
- Tool dependency validation
- Backward compat: old string format still works through resolver
"""

import pytest
import uuid
from datetime import datetime, timezone

from api.models import SkillRegistry
from core.skills.skill_manager import (
    SkillManager,
    SkillNotFoundError,
    SkillNotInstalledError,
)
from core.skills.resolver import CircularDependencyError, DependencyConflictError
from core.skills.credential_manager import CredentialManager


def _uid(prefix: str = "") -> str:
    return f"{prefix}_{uuid.uuid4().hex}"


def _now():
    return datetime.now(timezone.utc)


@pytest.fixture
def cred_mgr():
    return CredentialManager("test-secret-key-versioned-deps")


@pytest.fixture
def mgr(db_factory, cred_mgr):
    return SkillManager(db_factory, cred_mgr)


def _add_skill(db, name, version="1.0.0", depends_on=None, source="marketplace"):
    from api.models import SkillRegistry
    db.add(SkillRegistry(
        skill_id=_uid("sk"), skill_name=name, version=version,
        description="test", manifest={"depends_on": depends_on or []},
        is_active=True, is_public=True, source=source,
        created_by="admin", created_at=_now(),
    ))


def _cleanup(db, *names):
    from api.models import SkillInstallation, SkillRegistry
    for n in names:
        db.query(SkillInstallation).filter_by(skill_name=n).delete()
        db.query(SkillRegistry).filter_by(skill_name=n).delete()
    db.commit()


class TestVersionedInstall:
    """install() with new-format versioned dependencies."""

    def test_install_compatible_version(self, mgr, db_session):
        """base is 2.0.0, consumer needs >=1.0,<3.0 → success."""
        base = _uid("base")
        consumer = _uid("consumer")
        _add_skill(db_session, base, "2.0.0")
        _add_skill(db_session, consumer, depends_on=[
            {"name": base, "version": ">=1.0,<3.0", "type": "skill"},
        ])
        db_session.commit()
        try:
            mgr.install("user-1", base)
            inst = mgr.install("user-1", consumer)
            assert inst.skill_name == consumer
            assert inst.status == "installed"
        finally:
            _cleanup(db_session, consumer, base)

    def test_install_rejects_version_conflict(self, mgr, db_session):
        """base is 2.0.0, strict needs ==1.0.0 → conflict."""
        base = _uid("base")
        strict = _uid("strict")
        _add_skill(db_session, base, "2.0.0")
        _add_skill(db_session, strict, depends_on=[
            {"name": base, "version": "==1.0.0", "type": "skill"},
        ])
        db_session.commit()
        try:
            mgr.install("user-1", base)
            with pytest.raises(DependencyConflictError):
                mgr.install("user-1", strict)
        finally:
            _cleanup(db_session, strict, base)

    def test_install_rejects_missing_versioned_dep(self, mgr, db_session):
        """Skill depends on nonexistent → missing error."""
        orphan = _uid("orphan")
        _add_skill(db_session, orphan, depends_on=[
            {"name": "nonexistent_lib", "version": ">=1.0", "type": "skill"},
        ])
        db_session.commit()
        try:
            with pytest.raises(SkillNotFoundError, match="Missing dependencies"):
                mgr.install("user-1", orphan)
        finally:
            _cleanup(db_session, orphan)

    def test_install_still_requires_dep_installed_for_user(self, mgr, db_session):
        """Even if version is compatible, dep must be installed for user."""
        base = _uid("base")
        consumer = _uid("consumer")
        _add_skill(db_session, base, "1.0.0")
        _add_skill(db_session, consumer, depends_on=[
            {"name": base, "version": ">=1.0", "type": "skill"},
        ])
        db_session.commit()
        try:
            # Don't install base first
            with pytest.raises(SkillNotInstalledError, match=f"{base}.*must be installed"):
                mgr.install("user-1", consumer)
        finally:
            _cleanup(db_session, consumer, base)


class TestCircularDependency:
    """install() rejects circular dependencies."""

    def test_install_rejects_cycle(self, mgr, db_session):
        """A depends on B, B depends on A → CircularDependencyError.

        The cycle is detected from the full registry graph, so even the
        first install of either skill will fail.
        """
        a = _uid("a")
        b = _uid("b")
        _add_skill(db_session, a, depends_on=[
            {"name": b, "version": "*", "type": "skill"},
        ])
        _add_skill(db_session, b, depends_on=[
            {"name": a, "version": "*", "type": "skill"},
        ])
        db_session.commit()
        try:
            with pytest.raises(CircularDependencyError):
                mgr.install("user-1", a)
        finally:
            _cleanup(db_session, a, b)


class TestToolDependency:
    """install() with tool-type dependencies."""

    def test_tool_dep_no_user_install_needed(self, mgr, db_session):
        """Tool deps only need registry presence, not user installation.

        install() checks `dep.type == DependencyType.SKILL` before requiring
        user installation — tool deps skip that gate.
        """
        tool = _uid("tool")
        skill = _uid("skill")
        _add_skill(db_session, tool, "1.5.0", source="edge_tool")
        _add_skill(db_session, skill, depends_on=[
            {"name": tool, "version": ">=1.0", "type": "tool"},
        ])
        db_session.commit()
        try:
            # Do NOT install tool for the user
            inst = mgr.install("user-1", skill)
            assert inst.status == "installed"
        finally:
            _cleanup(db_session, skill, tool)

    def test_tool_not_in_registry_reports_missing(self, mgr, db_session):
        """If tool is not in registry at all, resolver reports it as missing."""
        skill = _uid("skill")
        _add_skill(db_session, skill, depends_on=[
            {"name": "ghost_tool", "version": ">=1.0", "type": "tool"},
        ])
        db_session.commit()
        try:
            with pytest.raises(SkillNotFoundError, match="Missing dependencies"):
                mgr.install("user-1", skill)
        finally:
            _cleanup(db_session, skill)


class TestBackwardCompat:
    """Old string-format depends_on still works through the resolver."""

    def test_old_format_deps_still_resolve(self, mgr, db_session):
        """depends_on: ["base"] (old format) still works."""
        base = _uid("base")
        consumer = _uid("consumer")
        _add_skill(db_session, base)
        _add_skill(db_session, consumer, depends_on=[base])
        db_session.commit()
        try:
            mgr.install("user-1", base)
            inst = mgr.install("user-1", consumer)
            assert inst.status == "installed"
        finally:
            _cleanup(db_session, consumer, base)
