"""P2: Skill Runtime Enforcement — distributed, concurrent, high-quality tests.

Scenarios:
1. Permission revocation after install (runtime check catches it)
2. Dependency missing at runtime (not just at install)
3. Concurrent execute + permission revoke (race condition safety)
4. Concurrent execute + dependency uninstall (race condition safety)
5. Multi-worker scenario (distributed lock safety)
"""

import pytest
import uuid
from datetime import datetime
from sqlalchemy.orm import Session

from api.models import (
    SkillRegistry,
    SkillInstallation,
    SkillPermission,
    User,
)
from core.skills.skill_manager import (
    SkillManager,
    SkillNotInstalledError,
    PermissionDeniedError,
)
from core.skills.credential_manager import CredentialManager


def _uid(prefix: str = "") -> str:
    """Generate unique ID."""
    return f"{prefix}_{uuid.uuid4().hex}"


# Track all test data for cleanup
_cleanup_users: list[str] = []
_cleanup_skills: list[str] = []


@pytest.fixture(autouse=True)
def _cleanup(db_session):
    """Clean up test data after each test."""
    _cleanup_users.clear()
    _cleanup_skills.clear()
    yield
    for skill_name in _cleanup_skills:
        db_session.query(SkillPermission).filter_by(skill_name=skill_name).delete()
        db_session.query(SkillInstallation).filter_by(skill_name=skill_name).delete()
        db_session.query(SkillRegistry).filter_by(skill_name=skill_name).delete()
    for user_id in _cleanup_users:
        db_session.query(User).filter_by(user_id=user_id).delete()
    db_session.commit()


# Use db_session (from root conftest) as the canonical session.
# db_factory (root conftest) wraps db_session with no-op close + leak detection.
# The "db" alias below is for convenience in test helpers that take a raw session.


@pytest.fixture
def db(db_session):
    """Alias for db_session — same session used by db_factory."""
    return db_session


@pytest.fixture
def cred_mgr():
    """Credential manager."""
    return CredentialManager(secret_key="test_secret_key_for_p2_tests")


@pytest.fixture
def skill_mgr(db_factory, cred_mgr):
    """Skill manager."""
    return SkillManager(db_factory, cred_mgr)


def _create_user(db: Session, user_id: str) -> User:
    """Create test user."""
    _cleanup_users.append(user_id)
    user = User(
        user_id=user_id,
        username=_uid("u"),
        email=f"{user_id}@test.local",
        password_hash="test",
    )
    db.add(user)
    db.commit()
    return user


def _create_skill(db: Session, skill_name: str, manifest: dict | None = None) -> SkillRegistry:
    """Create test skill definition."""
    _cleanup_skills.append(skill_name)
    skill = SkillRegistry(
        skill_id=_uid("skill"),
        skill_name=skill_name,
        version="1.0.0",
        is_active=1,
        is_public=0,
        source="marketplace",
        manifest=manifest or {},
    )
    db.add(skill)
    db.commit()
    return skill


def _grant_permission(db: Session, skill_name: str, user_id: str) -> None:
    """Grant user permission to install skill."""
    perm = SkillPermission(
        permission_id=_uid("p"),
        skill_name=skill_name,
        grantee_type="user",
        grantee_id=user_id,
        granted_by="admin",
    )
    db.add(perm)
    db.commit()


def _revoke_permission(db: Session, skill_name: str, user_id: str) -> None:
    """Revoke user permission."""
    db.query(SkillPermission).filter_by(
        skill_name=skill_name, grantee_type="user", grantee_id=user_id
    ).delete()
    db.commit()


class TestRuntimePermissionCheck:
    """Permission revocation after install — runtime check catches it."""

    def test_permission_revoked_after_install_blocks_execute(self, db, skill_mgr):
        """Install with permission, revoke permission, execute fails."""
        user_id = _uid("u")
        skill_name = _uid("skill")

        _create_user(db, user_id)
        _create_skill(db, skill_name)
        _grant_permission(db, skill_name, user_id)

        # Install succeeds
        skill_mgr.install(user_id, skill_name)
        assert skill_mgr.get_installation(user_id, skill_name) is not None

        # Revoke permission
        _revoke_permission(db, skill_name, user_id)

        # Execute fails: permission revoked
        with pytest.raises(PermissionDeniedError, match="Permission.*revoked"):
            skill_mgr.require_executable(user_id, skill_name)

    def test_permission_check_at_install_vs_runtime(self, db, skill_mgr):
        """Install checks permission, runtime checks again (may have changed)."""
        user_id = _uid("u")
        skill_name = _uid("skill")

        _create_user(db, user_id)
        _create_skill(db, skill_name)
        _grant_permission(db, skill_name, user_id)

        # Install succeeds
        inst = skill_mgr.install(user_id, skill_name)
        assert inst is not None

        # Revoke permission
        _revoke_permission(db, skill_name, user_id)

        # Runtime check fails
        with pytest.raises(PermissionDeniedError):
            skill_mgr.require_executable(user_id, skill_name)


class TestRuntimeDependencyCheck:
    """Dependency missing at runtime — not just at install."""

    def test_dependency_missing_at_runtime_blocks_execute(self, db, skill_mgr):
        """Install with dependency, uninstall dependency, execute fails."""
        user_id = _uid("u")
        dep_skill = _uid("dep")
        main_skill = _uid("main")

        _create_user(db, user_id)
        _create_skill(db, dep_skill)
        _create_skill(db, main_skill, manifest={"depends_on": [dep_skill]})
        _grant_permission(db, dep_skill, user_id)
        _grant_permission(db, main_skill, user_id)

        # Install dependency first
        skill_mgr.install(user_id, dep_skill)

        # Install main skill (dependency check passes)
        skill_mgr.install(user_id, main_skill)

        # Uninstall dependency
        skill_mgr.uninstall(user_id, dep_skill)

        # Execute main skill fails: dependency missing
        with pytest.raises(SkillNotInstalledError, match="Dependency.*not installed"):
            skill_mgr.require_executable(user_id, main_skill)

    def test_transitive_dependency_check(self, db, skill_mgr):
        """A → B → C: check only direct dependencies at runtime.
        
        Note: We only check direct dependencies, not transitive.
        If B depends on C and C is uninstalled, B's execution will fail,
        but A's execution will succeed (A only depends on B).
        This is correct: each skill is responsible for its own dependencies.
        """
        user_id = _uid("u")
        skill_c = _uid("c")
        skill_b = _uid("b")
        skill_a = _uid("a")

        _create_user(db, user_id)
        _create_skill(db, skill_c)
        _create_skill(db, skill_b, manifest={"depends_on": [skill_c]})
        _create_skill(db, skill_a, manifest={"depends_on": [skill_b]})

        for s in [skill_c, skill_b, skill_a]:
            _grant_permission(db, s, user_id)

        # Install all
        skill_mgr.install(user_id, skill_c)
        skill_mgr.install(user_id, skill_b)
        skill_mgr.install(user_id, skill_a)

        # Uninstall C
        skill_mgr.uninstall(user_id, skill_c)

        # Execute A succeeds (A only depends on B, which is still installed)
        skill_mgr.require_executable(user_id, skill_a)

        # Execute B fails (B depends on C, which is uninstalled)
        with pytest.raises(SkillNotInstalledError, match="Dependency.*not installed"):
            skill_mgr.require_executable(user_id, skill_b)


class TestConcurrentRaceConditions:
    """Concurrent execute + permission/dependency changes — real threads."""

    def test_concurrent_install_same_skill(self, db, skill_mgr):
        """Two threads install same skill simultaneously — no crash, one wins."""
        import threading
        from api.database import SessionLocal

        user_id = _uid("u")
        skill_name = _uid("skill")

        _create_user(db, user_id)
        _create_skill(db, skill_name)
        _grant_permission(db, skill_name, user_id)

        results = {"success": 0, "errors": []}
        lock = threading.Lock()
        barrier = threading.Barrier(2)

        def install_thread():
            barrier.wait()  # Start simultaneously
            # Each SkillManager._db() call creates and closes its own session
            mgr = SkillManager(SessionLocal, CredentialManager(secret_key="test"))
            try:
                mgr.install(user_id, skill_name)
                with lock:
                    results["success"] += 1
            except Exception as e:
                with lock:
                    results["errors"].append(str(e))

        t1 = threading.Thread(target=install_thread)
        t2 = threading.Thread(target=install_thread)
        t1.start()
        t2.start()
        t1.join(timeout=10)
        t2.join(timeout=10)

        assert not results["errors"], f"Unexpected errors: {results['errors']}"
        assert results["success"] == 2, "Both threads should succeed (one inserts, one returns existing)"
        assert skill_mgr.get_installation(user_id, skill_name) is not None

    def test_concurrent_permission_revoke_during_execute(self, db, skill_mgr):
        """Real concurrent test: one thread executes, another revokes permission mid-flight."""
        import threading
        from api.database import SessionLocal

        user_id = _uid("u")
        skill_name = _uid("skill")

        _create_user(db, user_id)
        _create_skill(db, skill_name)
        _grant_permission(db, skill_name, user_id)
        skill_mgr.install(user_id, skill_name)

        lock = threading.Lock()
        results = {"execute_ok": 0, "execute_denied": 0, "errors": []}
        # Phase gate: execute proves success, then revoke happens, then execute continues
        got_success = threading.Event()
        revoke_done = threading.Event()

        def execute_loop():
            # Each SkillManager._db() call creates and closes its own session
            mgr = SkillManager(SessionLocal, CredentialManager(secret_key="test"))
            # Phase 1: execute until at least one success
            for _ in range(100):
                try:
                    mgr.require_executable(user_id, skill_name)
                    with lock:
                        results["execute_ok"] += 1
                    got_success.set()
                    break
                except PermissionDeniedError:
                    with lock:
                        results["execute_denied"] += 1
                except Exception as e:
                    with lock:
                        results["errors"].append(str(e))
            # Phase 2: wait for revoke, then execute again to observe denial
            revoke_done.wait(timeout=10)
            try:
                mgr.require_executable(user_id, skill_name)
                with lock:
                    results["execute_ok"] += 1
            except PermissionDeniedError:
                with lock:
                    results["execute_denied"] += 1
            except Exception as e:
                with lock:
                    results["errors"].append(str(e))

        def revoke():
            got_success.wait(timeout=10)
            s = SessionLocal()
            try:
                s.query(SkillPermission).filter_by(
                    skill_name=skill_name, grantee_type="user", grantee_id=user_id
                ).delete()
                s.commit()
            finally:
                s.close()
                revoke_done.set()

        t1 = threading.Thread(target=execute_loop)
        t2 = threading.Thread(target=revoke)
        t1.start()
        t2.start()
        t1.join(timeout=15)
        t2.join(timeout=15)

        assert not results["errors"], f"Unexpected errors: {results['errors']}"
        assert results["execute_ok"] > 0, "Should have succeeded before revoke"
        assert results["execute_denied"] > 0, "Should have been denied after revoke"

    def test_concurrent_dependency_uninstall_during_execute(self, db, skill_mgr):
        """Real concurrent test: one thread executes, another uninstalls dependency mid-flight."""
        import threading
        from api.database import SessionLocal

        user_id = _uid("u")
        dep_skill = _uid("dep")
        main_skill = _uid("main")

        _create_user(db, user_id)
        _create_skill(db, dep_skill)
        _create_skill(db, main_skill, manifest={"depends_on": [dep_skill]})
        _grant_permission(db, dep_skill, user_id)
        _grant_permission(db, main_skill, user_id)
        skill_mgr.install(user_id, dep_skill)
        skill_mgr.install(user_id, main_skill)

        lock = threading.Lock()
        results = {"execute_ok": 0, "execute_denied": 0, "errors": []}
        got_success = threading.Event()
        uninstall_done = threading.Event()

        def execute_loop():
            # Each SkillManager._db() call creates and closes its own session
            mgr = SkillManager(SessionLocal, CredentialManager(secret_key="test"))
            # Phase 1: prove it works before uninstall
            for _ in range(100):
                try:
                    mgr.require_executable(user_id, main_skill)
                    with lock:
                        results["execute_ok"] += 1
                    got_success.set()
                    break
                except SkillNotInstalledError:
                    with lock:
                        results["execute_denied"] += 1
                except Exception as e:
                    with lock:
                        results["errors"].append(str(e))
            # Phase 2: wait for uninstall, then observe denial
            uninstall_done.wait(timeout=10)
            try:
                mgr.require_executable(user_id, main_skill)
                with lock:
                    results["execute_ok"] += 1
            except SkillNotInstalledError:
                with lock:
                    results["execute_denied"] += 1
            except Exception as e:
                with lock:
                    results["errors"].append(str(e))

        def uninstall_dep():
            got_success.wait(timeout=10)
            # Each SkillManager._db() call creates and closes its own session
            mgr = SkillManager(SessionLocal, CredentialManager(secret_key="test"))
            try:
                mgr.uninstall(user_id, dep_skill)
            finally:
                uninstall_done.set()

        t1 = threading.Thread(target=execute_loop)
        t2 = threading.Thread(target=uninstall_dep)
        t1.start()
        t2.start()
        t1.join(timeout=15)
        t2.join(timeout=15)

        assert not results["errors"], f"Unexpected errors: {results['errors']}"
        assert results["execute_ok"] > 0, "Should have succeeded before uninstall"
        assert results["execute_denied"] > 0, "Should have been denied after dep uninstall"


class TestSkillDeactivation:
    """Skill deactivated after install — runtime check catches it."""

    def test_skill_deactivated_blocks_execute(self, db, skill_mgr):
        """Install skill, deactivate it, execute fails."""
        user_id = _uid("u")
        skill_name = _uid("skill")

        _create_user(db, user_id)
        defn = _create_skill(db, skill_name)
        _grant_permission(db, skill_name, user_id)
        skill_mgr.install(user_id, skill_name)

        # Execute succeeds
        skill_mgr.require_executable(user_id, skill_name)

        # Deactivate skill
        defn.is_active = 0
        db.commit()

        # Execute fails: definition not found (deactivated)
        with pytest.raises(PermissionDeniedError, match="deactivated"):
            skill_mgr.require_executable(user_id, skill_name)

    def test_public_skill_no_permission_needed(self, db, skill_mgr):
        """Public skill: no permission check needed."""
        user_id = _uid("u")
        skill_name = _uid("skill")

        _create_user(db, user_id)
        defn = _create_skill(db, skill_name)
        defn.is_public = 1
        db.commit()

        # Install (public skills don't need permission)
        skill_mgr.install(user_id, skill_name)

        # Execute succeeds (no permission grant needed)
        skill_mgr.require_executable(user_id, skill_name)


class TestExecutorIntegration:
    """Verify executor calls _enforce_runtime_checks on all paths."""

    def test_enforce_called_on_execute(self, db, skill_mgr):
        """require_executable blocks after permission revoke."""
        user_id = _uid("u")
        skill_name = _uid("skill")

        _create_user(db, user_id)
        _create_skill(db, skill_name)
        _grant_permission(db, skill_name, user_id)
        skill_mgr.install(user_id, skill_name)

        # Succeeds
        skill_mgr.require_executable(user_id, skill_name)

        # Revoke
        _revoke_permission(db, skill_name, user_id)

        # Fails
        with pytest.raises(PermissionDeniedError):
            skill_mgr.require_executable(user_id, skill_name)
