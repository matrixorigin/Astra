"""Tests for SkillManager — install/uninstall/upgrade lifecycle + credentials.

v3: All operations on platform DB. No BYOD, no DDL execution.
Uses REAL database for all tests.
"""

import uuid
from datetime import datetime, timezone

import pytest

from api.models import (
    Role,
    SkillDefinition,
    SkillInstallation,
    SkillPermission,
    User,
    UserCredential,
    UserRole,
)
from core.skills.credential_manager import CredentialManager
from core.skills.skill_manager import (
    PermissionDeniedError,
    SkillManager,
    SkillNotFoundError,
    SkillNotInstalledError,
)


@pytest.fixture
def cred_mgr():
    return CredentialManager("test-secret-key-for-unit-tests")


@pytest.fixture
def mgr(db_factory, cred_mgr):
    return SkillManager(db_factory, cred_mgr)


def _uid():
    return str(uuid.uuid4())


def _now():
    return datetime.now(timezone.utc)


def _unique_name(prefix="skill"):
    return f"{prefix}_{uuid.uuid4().hex[:8]}"


def _seed_skill(db, name=None, version="1.0.0", is_public=True):
    name = name or _unique_name()
    defn = SkillDefinition(
        skill_id=_uid(),
        name=name,
        version=version,
        manifest={"tables": [f"sk_{name}_data"]},
        is_active=1,
        is_public=1 if is_public else 0,
        created_at=_now(),
    )
    db.add(defn)
    db.flush()
    return defn


def _seed_permission(db, skill_name, grantee_type, grantee_id):
    perm = SkillPermission(
        permission_id=_uid(),
        skill_name=skill_name,
        grantee_type=grantee_type,
        grantee_id=grantee_id,
        granted_by="admin",
        granted_at=_now(),
    )
    db.add(perm)
    db.flush()
    return perm


# ── get_definition ────────────────────────────────────────────────────────────


class TestGetDefinition:
    def test_found(self, mgr, db_session):
        defn = _seed_skill(db_session)
        assert mgr.get_definition(defn.name) is not None

    def test_not_found(self, mgr):
        assert mgr.get_definition("nonexistent_xyz") is None

    def test_inactive_not_returned(self, mgr, db_session):
        defn = _seed_skill(db_session)
        defn.is_active = 0
        db_session.flush()
        assert mgr.get_definition(defn.name) is None


# ── check_permission ──────────────────────────────────────────────────────────


class TestCheckPermission:
    def test_public_skill_always_allowed(self, mgr, db_session):
        defn = _seed_skill(db_session, is_public=True)
        assert mgr.check_permission("any-user", defn.name) is True

    def test_private_skill_denied_without_grant(self, mgr, db_session):
        defn = _seed_skill(db_session, is_public=False)
        assert mgr.check_permission(_uid(), defn.name) is False

    def test_private_skill_allowed_with_user_grant(self, mgr, db_session):
        defn = _seed_skill(db_session, is_public=False)
        uid = _uid()
        _seed_permission(db_session, defn.name, "user", uid)
        assert mgr.check_permission(uid, defn.name) is True

    def test_private_skill_allowed_with_role_grant(self, mgr, db_session):
        defn = _seed_skill(db_session, is_public=False)
        uid = _uid()
        role_id = _uid()
        db_session.add(User(user_id=uid, username=f"u_{uid[:8]}", email=f"{uid[:8]}@test.com", password_hash="x"))
        db_session.add(Role(role_id=role_id, role_name=f"r_{role_id[:8]}"))
        db_session.flush()
        _seed_permission(db_session, defn.name, "role", role_id)
        db_session.add(UserRole(user_id=uid, role_id=role_id))
        db_session.flush()
        assert mgr.check_permission(uid, defn.name) is True

    def test_nonexistent_skill(self, mgr):
        assert mgr.check_permission(_uid(), "nope_xyz") is False


# ── install ───────────────────────────────────────────────────────────────────


class TestInstall:
    def test_install_records_installation(self, mgr, db_session):
        defn = _seed_skill(db_session)
        inst = mgr.install(_uid(), defn.name)
        assert inst.skill_name == defn.name
        assert inst.status == "installed"
        assert inst.skill_version == "1.0.0"

    def test_install_idempotent(self, mgr, db_session):
        defn = _seed_skill(db_session)
        uid = _uid()
        inst1 = mgr.install(uid, defn.name)
        inst2 = mgr.install(uid, defn.name)
        assert inst1.installation_id == inst2.installation_id

    def test_install_skill_not_found(self, mgr):
        with pytest.raises(SkillNotFoundError):
            mgr.install(_uid(), "nonexistent_xyz")

    def test_install_permission_denied(self, mgr, db_session):
        defn = _seed_skill(db_session, is_public=False)
        with pytest.raises(PermissionDeniedError):
            mgr.install(_uid(), defn.name)


# ── uninstall ─────────────────────────────────────────────────────────────────


class TestUninstall:
    def test_uninstall_marks_and_deletes_creds(self, mgr, db_session):
        defn = _seed_skill(db_session)
        uid = _uid()
        mgr.install(uid, defn.name)
        mgr.save_credential(uid, defn.name, "token", "ghp_xxx")

        mgr.uninstall(uid, defn.name)

        inst = db_session.query(SkillInstallation).filter_by(
            user_id=uid, skill_name=defn.name
        ).first()
        assert inst.status == "uninstalled"
        assert mgr.get_credential(uid, defn.name, "token") is None

    def test_uninstall_not_installed(self, mgr):
        with pytest.raises(SkillNotInstalledError):
            mgr.uninstall(_uid(), "nonexistent_xyz")


# ── upgrade ───────────────────────────────────────────────────────────────────


class TestUpgrade:
    def test_upgrade_bumps_version(self, mgr, db_session):
        defn = _seed_skill(db_session, version="1.0.0")
        uid = _uid()
        mgr.install(uid, defn.name)
        defn.version = "1.1.0"
        db_session.flush()
        inst = mgr.upgrade(uid, defn.name)
        assert inst.skill_version == "1.1.0"

    def test_upgrade_already_latest(self, mgr, db_session):
        defn = _seed_skill(db_session, version="1.0.0")
        uid = _uid()
        mgr.install(uid, defn.name)
        inst = mgr.upgrade(uid, defn.name)
        assert inst.skill_version == "1.0.0"

    def test_upgrade_not_installed(self, mgr, db_session):
        defn = _seed_skill(db_session)
        with pytest.raises(SkillNotInstalledError):
            mgr.upgrade(_uid(), defn.name)


# ── credentials ───────────────────────────────────────────────────────────────


class TestCredentials:
    def test_save_and_get(self, mgr):
        uid = _uid()
        mgr.save_credential(uid, "gh", "token", "ghp_abc123")
        assert mgr.get_credential(uid, "gh", "token") == "ghp_abc123"

    def test_get_nonexistent(self, mgr):
        assert mgr.get_credential(_uid(), "gh", "nope") is None

    def test_update_credential(self, mgr):
        uid = _uid()
        mgr.save_credential(uid, "gh", "token", "old")
        mgr.save_credential(uid, "gh", "token", "new")
        assert mgr.get_credential(uid, "gh", "token") == "new"

    def test_update_sets_rotated_at(self, mgr, db_session):
        uid = _uid()
        mgr.save_credential(uid, "gh", "token", "v1")
        row = db_session.query(UserCredential).filter_by(user_id=uid).first()
        assert row.rotated_at is None
        mgr.save_credential(uid, "gh", "token", "v2")
        db_session.refresh(row)
        assert row.rotated_at is not None

    def test_get_all_credentials(self, mgr):
        uid = _uid()
        mgr.save_credential(uid, "gh", "token", "t1")
        mgr.save_credential(uid, "gh", "webhook_secret", "s1")
        creds = mgr.get_all_credentials(uid, "gh")
        assert creds == {"token": "t1", "webhook_secret": "s1"}

    def test_delete_credential(self, mgr):
        uid = _uid()
        mgr.save_credential(uid, "gh", "token", "val")
        assert mgr.delete_credential(uid, "gh", "token") is True
        assert mgr.get_credential(uid, "gh", "token") is None

    def test_delete_nonexistent(self, mgr):
        assert mgr.delete_credential(_uid(), "gh", "nope") is False

    def test_credentials_isolated_per_user(self, mgr):
        u1, u2 = _uid(), _uid()
        mgr.save_credential(u1, "gh", "token", "user1-token")
        mgr.save_credential(u2, "gh", "token", "user2-token")
        assert mgr.get_credential(u1, "gh", "token") == "user1-token"
        assert mgr.get_credential(u2, "gh", "token") == "user2-token"

    def test_credentials_isolated_per_skill(self, mgr):
        uid = _uid()
        mgr.save_credential(uid, "gh", "token", "gh-token")
        mgr.save_credential(uid, "jira", "token", "jira-token")
        assert mgr.get_credential(uid, "gh", "token") == "gh-token"
        assert mgr.get_credential(uid, "jira", "token") == "jira-token"

    def test_encrypted_value_not_plaintext(self, mgr, db_session):
        uid = _uid()
        mgr.save_credential(uid, "gh", "token", "my-secret-token")
        row = db_session.query(UserCredential).filter_by(user_id=uid).first()
        assert row.value_encrypted != "my-secret-token"
        assert len(row.value_encrypted) > 50


# ── list_installed ────────────────────────────────────────────────────────────


class TestListInstalled:
    def test_empty(self, mgr):
        assert mgr.list_installed(_uid()) == []

    def test_lists_only_installed(self, mgr, db_session):
        defn1 = _seed_skill(db_session)
        defn2 = _seed_skill(db_session)
        uid = _uid()
        mgr.install(uid, defn1.name)
        mgr.install(uid, defn2.name)
        mgr.uninstall(uid, defn2.name)
        installed = mgr.list_installed(uid)
        assert len(installed) == 1
        assert installed[0].skill_name == defn1.name
