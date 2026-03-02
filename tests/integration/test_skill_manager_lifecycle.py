"""Tests for SkillManager — upgrade, rollback, credential CRUD, query methods.

Covers lifecycle operations not hit by enforcement/versioned-deps tests.
Each test uses unique skill_name/user_id to avoid parallel conflicts.
"""

import pytest
import uuid
from datetime import datetime, timezone

from api.models import SkillRegistry, SkillInstallation, UserCredential
from core.skills.skill_manager import (
    SkillManager,
    SkillNotFoundError,
    SkillNotInstalledError,
)
from core.skills.credential_manager import CredentialManager


def _uid(prefix: str = "") -> str:
    return f"{prefix}_{uuid.uuid4().hex}"


def _now():
    return datetime.now(timezone.utc)


@pytest.fixture
def cred_mgr():
    return CredentialManager("test-secret-key-lifecycle")


@pytest.fixture
def mgr(db_factory, cred_mgr):
    return SkillManager(db_factory, cred_mgr)


@pytest.fixture
def skill_env(db_session):
    """Factory fixture: create skills with auto-cleanup.

    Usage:
        name, uid = skill_env.create()
        name2, uid2 = skill_env.create(version="3.0.0")
    """
    created: list[str] = []

    class Env:
        def create(self, version: str = "2.0.0") -> tuple[str, str]:
            name = _uid("skill")
            db_session.add(SkillRegistry(
                skill_id=_uid("sk"), skill_name=name, version=version,
                description="test", manifest={"depends_on": []},
                is_active=True, is_public=True, source="marketplace",
                created_by="admin", created_at=_now(),
            ))
            db_session.commit()
            created.append(name)
            return name, _uid("u")

        @property
        def db(self):
            return db_session

    yield Env()

    for name in created:
        db_session.query(UserCredential).filter_by(skill_name=name).delete()
        db_session.query(SkillInstallation).filter_by(skill_name=name).delete()
        db_session.query(SkillRegistry).filter_by(skill_name=name).delete()
    db_session.commit()


class TestPublicQueries:
    def test_get_definition(self, mgr, skill_env):
        name, _ = skill_env.create()
        defn = mgr.get_definition(name)
        assert defn is not None
        assert defn.skill_name == name

    def test_get_definition_not_found(self, mgr):
        assert mgr.get_definition(_uid("nope")) is None

    def test_get_installation_not_found(self, mgr):
        assert mgr.get_installation(_uid("u"), _uid("s")) is None

    def test_get_installation_found(self, mgr, skill_env):
        name, uid = skill_env.create()
        mgr.install(uid, name)
        inst = mgr.get_installation(uid, name)
        assert inst is not None
        assert inst.status == "installed"

    def test_list_installed_empty(self, mgr):
        assert mgr.list_installed(_uid("nb")) == []

    def test_list_installed(self, mgr, skill_env):
        name, uid = skill_env.create()
        mgr.install(uid, name)
        installed = mgr.list_installed(uid)
        assert any(i.skill_name == name for i in installed)


class TestInstallEdgeCases:
    def test_install_not_found(self, mgr):
        with pytest.raises(SkillNotFoundError):
            mgr.install(_uid("u"), _uid("nope"))

    def test_install_idempotent(self, mgr, skill_env):
        """Installing same skill twice returns existing."""
        name, uid = skill_env.create()
        inst1 = mgr.install(uid, name)
        inst2 = mgr.install(uid, name)
        assert inst1.installation_id == inst2.installation_id


class TestUpgrade:
    def test_upgrade_bumps_version(self, mgr, skill_env):
        name, uid = skill_env.create("2.0.0")
        mgr.install(uid, name)
        defn = skill_env.db.query(SkillRegistry).filter_by(skill_name=name).first()
        defn.version = "3.0.0"
        skill_env.db.commit()

        inst = mgr.upgrade(uid, name)
        assert inst.skill_version == "3.0.0"
        assert inst.previous_version == "2.0.0"

    def test_upgrade_noop_same_version(self, mgr, skill_env):
        name, uid = skill_env.create()
        mgr.install(uid, name)
        inst = mgr.upgrade(uid, name)
        assert inst.skill_version == "2.0.0"

    def test_upgrade_not_installed(self, mgr, skill_env):
        name, uid = skill_env.create()
        with pytest.raises(SkillNotInstalledError):
            mgr.upgrade(uid, name)

    def test_upgrade_skill_not_found(self, mgr, skill_env):
        name, uid = skill_env.create()
        mgr.install(uid, name)
        defn = skill_env.db.query(SkillRegistry).filter_by(skill_name=name).first()
        defn.is_active = 0
        skill_env.db.commit()
        with pytest.raises(SkillNotFoundError):
            mgr.upgrade(uid, name)


class TestRollback:
    def test_rollback_restores_version(self, mgr, skill_env):
        name, uid = skill_env.create("2.0.0")
        mgr.install(uid, name)
        defn = skill_env.db.query(SkillRegistry).filter_by(skill_name=name).first()
        defn.version = "3.0.0"
        skill_env.db.commit()
        mgr.upgrade(uid, name)

        inst = mgr.rollback(uid, name)
        assert inst.skill_version == "2.0.0"
        assert inst.previous_version == "3.0.0"

    def test_rollback_not_installed(self, mgr, skill_env):
        name, uid = skill_env.create()
        with pytest.raises(SkillNotInstalledError):
            mgr.rollback(uid, name)

    def test_rollback_no_previous(self, mgr, skill_env):
        name, uid = skill_env.create()
        mgr.install(uid, name)
        with pytest.raises(SkillNotInstalledError, match="no previous version"):
            mgr.rollback(uid, name)


class TestCredentials:
    def test_save_and_get(self, mgr, skill_env):
        name, uid = skill_env.create()
        mgr.save_credential(uid, name, "api_key", "secret123")
        assert mgr.get_credential(uid, name, "api_key") == "secret123"

    def test_get_nonexistent(self, mgr):
        assert mgr.get_credential(_uid("u"), _uid("s"), "y") is None

    def test_save_overwrites(self, mgr, skill_env):
        name, uid = skill_env.create()
        mgr.save_credential(uid, name, "key", "v1")
        mgr.save_credential(uid, name, "key", "v2")
        assert mgr.get_credential(uid, name, "key") == "v2"

    def test_get_all(self, mgr, skill_env):
        name, uid = skill_env.create()
        mgr.save_credential(uid, name, "a", "1")
        mgr.save_credential(uid, name, "b", "2")
        creds = mgr.get_all_credentials(uid, name)
        assert creds == {"a": "1", "b": "2"}

    def test_get_all_empty(self, mgr):
        assert mgr.get_all_credentials(_uid("u"), _uid("s")) == {}

    def test_delete(self, mgr, skill_env):
        name, uid = skill_env.create()
        mgr.save_credential(uid, name, "key", "val")
        assert mgr.delete_credential(uid, name, "key") is True
        assert mgr.get_credential(uid, name, "key") is None

    def test_delete_nonexistent(self, mgr):
        assert mgr.delete_credential(_uid("u"), _uid("s"), "y") is False

    def test_uninstall_deletes_credentials(self, mgr, skill_env):
        name, uid = skill_env.create()
        mgr.install(uid, name)
        mgr.save_credential(uid, name, "token", "abc")
        mgr.uninstall(uid, name)
        assert mgr.get_credential(uid, name, "token") is None


class TestUninstall:
    def test_uninstall(self, mgr, skill_env):
        name, uid = skill_env.create()
        mgr.install(uid, name)
        mgr.uninstall(uid, name)
        assert mgr.get_installation(uid, name) is None

    def test_uninstall_not_installed(self, mgr, skill_env):
        name, uid = skill_env.create()
        with pytest.raises(SkillNotInstalledError):
            mgr.uninstall(uid, name)


class TestCheckPermission:
    def test_public_skill(self, mgr, skill_env):
        name, _ = skill_env.create()
        assert mgr.check_permission("anyone", name) is True

    def test_nonexistent_skill(self, mgr):
        assert mgr.check_permission(_uid("u"), _uid("nope")) is False
