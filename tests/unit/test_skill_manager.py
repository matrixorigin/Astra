"""Tests for SkillManager — install/uninstall/upgrade lifecycle + credentials.

Uses REAL database for all tests. No mocking of UserDBPool — DDL actually executes.
"""

import uuid
from datetime import datetime, timezone

import pytest
from sqlalchemy import text

from api.models import (
    SkillDefinition,
    SkillInstallation,
    SkillPermission,
    UserConnection,
    UserCredential,
    UserRole,
)
from core.skills.credential_manager import CredentialManager
from core.skills.skill_manager import (
    ConnectionRequiredError,
    PermissionDeniedError,
    SkillManager,
    SkillNotFoundError,
    SkillNotInstalledError,
)
from core.skills.user_db_pool import UserDBPool


# ── Test BYOD database ───────────────────────────────────────────────────────
# We use the SAME MatrixOne instance as the platform DB but a DIFFERENT database
# to simulate BYOD. Created/dropped per test session.

BYOD_DB_NAME = "test_byod_skill_mgr"


@pytest.fixture(scope="module")
def byod_db_name(test_engine):
    """Create a separate database for BYOD tests, drop after module."""
    with test_engine.connect() as c:
        c.execute(text(f"CREATE DATABASE IF NOT EXISTS `{BYOD_DB_NAME}`"))
        c.execute(text("COMMIT"))
    yield BYOD_DB_NAME
    with test_engine.connect() as c:
        c.execute(text(f"DROP DATABASE IF EXISTS `{BYOD_DB_NAME}`"))
        c.execute(text("COMMIT"))


@pytest.fixture
def cred_mgr():
    return CredentialManager("test-secret-key-for-unit-tests")


@pytest.fixture
def pool():
    p = UserDBPool(pool_size=1, max_overflow=1, pool_recycle=300)
    yield p
    p.close_all()


@pytest.fixture
def mgr(db_session, cred_mgr, pool):
    return SkillManager(db_session, cred_mgr, pool)


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
        manifest={"tables": [f"{name}_data"]},
        is_active=1,
        is_public=1 if is_public else 0,
        created_at=_now(),
    )
    db.add(defn)
    db.flush()
    return defn


def _seed_byod_connection(db, user_id, cred_mgr, byod_db_name):
    """Register a BYOD connection pointing to the test BYOD database."""
    conn = UserConnection(
        connection_id=_uid(),
        user_id=user_id,
        dialect="mysql",
        host="localhost",
        port=6001,
        database=byod_db_name,
        username="root",
        password_encrypted=cred_mgr.encrypt("111"),
        status="active",
        created_at=_now(),
    )
    db.add(conn)
    db.flush()
    return conn


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
        """Role-based permission: user has role, role has skill grant."""
        from api.models import Role, User
        defn = _seed_skill(db_session, is_public=False)
        uid = _uid()
        role_id = _uid()
        # Create user and role (FK constraints)
        db_session.add(User(user_id=uid, username=f"u_{uid[:8]}", email=f"{uid[:8]}@test.com", password_hash="x"))
        db_session.add(Role(role_id=role_id, role_name=f"r_{role_id[:8]}"))
        db_session.flush()
        # Grant skill to role
        _seed_permission(db_session, defn.name, "role", role_id)
        # Assign role to user
        db_session.add(UserRole(user_id=uid, role_id=role_id))
        db_session.flush()
        assert mgr.check_permission(uid, defn.name) is True

    def test_nonexistent_skill(self, mgr):
        assert mgr.check_permission(_uid(), "nope_xyz") is False


# ── install (REAL DB — DDL actually executes) ─────────────────────────────────


class TestInstallRealDB:
    def test_install_creates_tables_on_user_db(self, mgr, db_session, cred_mgr, pool, byod_db_name):
        """Install with DDL — tables actually created on BYOD database."""
        defn = _seed_skill(db_session)
        uid = _uid()
        _seed_byod_connection(db_session, uid, cred_mgr, byod_db_name)

        table_name = f"{defn.name}_data"
        ddls = [
            f"CREATE TABLE IF NOT EXISTS `{table_name}` ("
            "  id VARCHAR(36) PRIMARY KEY,"
            "  value TEXT,"
            "  created_at DATETIME"
            ")"
        ]
        inst = mgr.install(uid, defn.name, table_ddls=ddls)

        assert inst.skill_name == defn.name
        assert inst.status == "installed"
        assert inst.skill_version == "1.0.0"

        # Verify table actually exists on BYOD DB
        from core.skills.skill_manager import _ConnectionWithPassword
        conn = mgr.get_connection(uid)
        conn_pw = _ConnectionWithPassword(conn, cred_mgr)
        user_session = pool.get_session(uid, conn_pw)
        try:
            result = user_session.execute(text(f"SHOW TABLES LIKE '{table_name}'"))
            tables = [r[0] for r in result]
            assert table_name in tables

            # Verify meta table has record
            result = user_session.execute(
                text(f"SELECT skill_name, skill_version FROM {mgr.META_TABLE} WHERE skill_name = :n"),
                {"n": defn.name},
            )
            row = result.fetchone()
            assert row is not None
            assert row[0] == defn.name
            assert row[1] == "1.0.0"
        finally:
            user_session.close()

        # Cleanup
        pool.close_user(uid)

    def test_install_no_ddl(self, mgr, db_session, cred_mgr, byod_db_name):
        defn = _seed_skill(db_session)
        uid = _uid()
        _seed_byod_connection(db_session, uid, cred_mgr, byod_db_name)
        inst = mgr.install(uid, defn.name)
        assert inst.skill_name == defn.name
        assert inst.status == "installed"

    def test_install_idempotent(self, mgr, db_session, cred_mgr, byod_db_name):
        defn = _seed_skill(db_session)
        uid = _uid()
        _seed_byod_connection(db_session, uid, cred_mgr, byod_db_name)
        inst1 = mgr.install(uid, defn.name)
        inst2 = mgr.install(uid, defn.name)
        assert inst1.installation_id == inst2.installation_id

    def test_install_skill_not_found(self, mgr, db_session, cred_mgr, byod_db_name):
        uid = _uid()
        _seed_byod_connection(db_session, uid, cred_mgr, byod_db_name)
        with pytest.raises(SkillNotFoundError):
            mgr.install(uid, "nonexistent_xyz")

    def test_install_permission_denied(self, mgr, db_session, cred_mgr, byod_db_name):
        defn = _seed_skill(db_session, is_public=False)
        uid = _uid()
        _seed_byod_connection(db_session, uid, cred_mgr, byod_db_name)
        with pytest.raises(PermissionDeniedError):
            mgr.install(uid, defn.name)

    def test_install_no_connection(self, mgr, db_session):
        defn = _seed_skill(db_session)
        with pytest.raises(ConnectionRequiredError):
            mgr.install(_uid(), defn.name)


# ── uninstall (REAL DB) ───────────────────────────────────────────────────────


class TestUninstallRealDB:
    def test_uninstall_keep_data(self, mgr, db_session, cred_mgr, byod_db_name):
        defn = _seed_skill(db_session)
        uid = _uid()
        _seed_byod_connection(db_session, uid, cred_mgr, byod_db_name)
        mgr.install(uid, defn.name)
        mgr.save_credential(uid, defn.name, "token", "ghp_xxx")

        mgr.uninstall(uid, defn.name)

        inst = db_session.query(SkillInstallation).filter_by(
            user_id=uid, skill_name=defn.name
        ).first()
        assert inst.status == "uninstalled"
        # Credentials removed
        assert mgr.get_credential(uid, defn.name, "token") is None

    def test_uninstall_drop_tables(self, mgr, db_session, cred_mgr, pool, byod_db_name):
        """Uninstall with drop_tables — tables actually dropped from BYOD."""
        defn = _seed_skill(db_session)
        uid = _uid()
        _seed_byod_connection(db_session, uid, cred_mgr, byod_db_name)

        table_name = f"{defn.name}_data"
        ddls = [
            f"CREATE TABLE IF NOT EXISTS `{table_name}` ("
            "  id VARCHAR(36) PRIMARY KEY"
            ")"
        ]
        mgr.install(uid, defn.name, table_ddls=ddls)

        mgr.uninstall(uid, defn.name, drop_tables=True, table_names=[table_name])

        # Verify table is gone
        from core.skills.skill_manager import _ConnectionWithPassword
        conn = mgr.get_connection(uid)
        # Connection still exists (uninstall doesn't remove connection)
        # But the user is uninstalled, so get_connection still works since
        # connection is per-user not per-skill
        # Actually after uninstall, get_connection still returns the conn
        conn_pw = _ConnectionWithPassword(conn, cred_mgr)
        user_session = pool.get_session(uid, conn_pw)
        try:
            result = user_session.execute(text(f"SHOW TABLES LIKE '{table_name}'"))
            tables = [r[0] for r in result]
            assert table_name not in tables
        finally:
            user_session.close()
            pool.close_user(uid)

    def test_uninstall_not_installed(self, mgr):
        with pytest.raises(SkillNotInstalledError):
            mgr.uninstall(_uid(), "nonexistent_xyz")

    def test_uninstall_drop_tables_no_names_raises(self, mgr, db_session, cred_mgr, byod_db_name):
        """drop_tables=True without table_names should raise ValueError."""
        defn = _seed_skill(db_session)
        uid = _uid()
        _seed_byod_connection(db_session, uid, cred_mgr, byod_db_name)
        mgr.install(uid, defn.name)
        with pytest.raises(ValueError, match="table_names"):
            mgr.uninstall(uid, defn.name, drop_tables=True)


# ── upgrade (REAL DB) ─────────────────────────────────────────────────────────


class TestUpgradeRealDB:
    def test_upgrade_code_only(self, mgr, db_session, cred_mgr, byod_db_name):
        defn = _seed_skill(db_session, version="1.0.0")
        uid = _uid()
        _seed_byod_connection(db_session, uid, cred_mgr, byod_db_name)
        mgr.install(uid, defn.name)

        defn.version = "1.1.0"
        db_session.flush()

        inst = mgr.upgrade(uid, defn.name)
        assert inst.skill_version == "1.1.0"

    def test_upgrade_with_alter(self, mgr, db_session, cred_mgr, pool, byod_db_name):
        """Upgrade with ALTER TABLE — column actually added on BYOD."""
        defn = _seed_skill(db_session, version="1.0.0")
        uid = _uid()
        _seed_byod_connection(db_session, uid, cred_mgr, byod_db_name)

        table_name = f"{defn.name}_data"
        ddls = [
            f"CREATE TABLE IF NOT EXISTS `{table_name}` ("
            "  id VARCHAR(36) PRIMARY KEY,"
            "  value TEXT"
            ")"
        ]
        mgr.install(uid, defn.name, table_ddls=ddls)

        defn.version = "1.1.0"
        db_session.flush()

        inst = mgr.upgrade(
            uid, defn.name,
            alter_ddls=[f"ALTER TABLE `{table_name}` ADD COLUMN stars INT DEFAULT 0"],
        )
        assert inst.skill_version == "1.1.0"

        # Verify column exists
        from core.skills.skill_manager import _ConnectionWithPassword
        conn = mgr.get_connection(uid)
        conn_pw = _ConnectionWithPassword(conn, cred_mgr)
        user_session = pool.get_session(uid, conn_pw)
        try:
            result = user_session.execute(text(f"DESCRIBE `{table_name}`"))
            columns = {r[0] for r in result}
            assert "stars" in columns
        finally:
            user_session.close()
            pool.close_user(uid)

    def test_upgrade_already_latest(self, mgr, db_session, cred_mgr, byod_db_name):
        defn = _seed_skill(db_session, version="1.0.0")
        uid = _uid()
        _seed_byod_connection(db_session, uid, cred_mgr, byod_db_name)
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
        """Verify the stored value is actually encrypted, not plaintext."""
        uid = _uid()
        mgr.save_credential(uid, "gh", "token", "my-secret-token")
        row = db_session.query(UserCredential).filter_by(user_id=uid).first()
        assert row.value_encrypted != "my-secret-token"
        assert len(row.value_encrypted) > 50  # Fernet ciphertext is long


# ── connection CRUD ───────────────────────────────────────────────────────────


class TestConnectionCRUD:
    def test_register_and_get(self, mgr):
        uid = _uid()
        conn = mgr.register_connection(
            uid, dialect="mysql", host="db.example.com", port=3306,
            database="mydb", username="app", password="secret",
        )
        assert conn.user_id == uid
        assert conn.host == "db.example.com"
        assert conn.status == "active"

        fetched = mgr.get_connection(uid)
        assert fetched.connection_id == conn.connection_id

    def test_register_updates_existing(self, mgr, db_session):
        uid = _uid()
        mgr.register_connection(
            uid, dialect="mysql", host="old.host", port=3306,
            database="db", username="u", password="p",
        )
        conn = mgr.register_connection(
            uid, dialect="mysql", host="new.host", port=3307,
            database="db2", username="u2", password="p2",
        )
        assert conn.host == "new.host"
        assert conn.port == 3307
        count = db_session.query(UserConnection).filter_by(user_id=uid).count()
        assert count == 1

    def test_register_password_is_encrypted(self, mgr, db_session, cred_mgr):
        uid = _uid()
        mgr.register_connection(
            uid, dialect="mysql", host="h", port=3306,
            database="d", username="u", password="my-password",
        )
        conn = db_session.query(UserConnection).filter_by(user_id=uid).first()
        assert conn.password_encrypted != "my-password"
        assert cred_mgr.decrypt(conn.password_encrypted) == "my-password"

    def test_get_connection_none(self, mgr):
        assert mgr.get_connection(_uid()) is None

    def test_verify_connection_real(self, mgr, cred_mgr, byod_db_name):
        """Test verify_connection against real BYOD database."""
        uid = _uid()
        mgr.register_connection(
            uid, dialect="mysql", host="localhost", port=6001,
            database=byod_db_name, username="root", password="111",
        )
        assert mgr.verify_connection(uid) is True
        conn = mgr.get_connection(uid)
        assert conn.verified_at is not None
        assert conn.status == "active"

    def test_verify_connection_bad_host(self, mgr):
        uid = _uid()
        mgr.register_connection(
            uid, dialect="mysql", host="nonexistent.invalid", port=9999,
            database="nope", username="u", password="p",
        )
        assert mgr.verify_connection(uid) is False
        conn = mgr.get_connection(uid)
        # status should be "error" but get_connection filters by "active"
        # so we query directly
        from api.models import UserConnection
        row = mgr._db.query(UserConnection).filter_by(user_id=uid).first()
        assert row.status == "error"

    def test_verify_connection_no_connection(self, mgr):
        assert mgr.verify_connection(_uid()) is False


# ── list_installed ────────────────────────────────────────────────────────────


class TestListInstalled:
    def test_empty(self, mgr):
        assert mgr.list_installed(_uid()) == []

    def test_lists_only_installed(self, mgr, db_session, cred_mgr, byod_db_name):
        defn1 = _seed_skill(db_session)
        defn2 = _seed_skill(db_session)
        uid = _uid()
        _seed_byod_connection(db_session, uid, cred_mgr, byod_db_name)
        mgr.install(uid, defn1.name)
        mgr.install(uid, defn2.name)
        mgr.uninstall(uid, defn2.name)

        installed = mgr.list_installed(uid)
        assert len(installed) == 1
        assert installed[0].skill_name == defn1.name
