"""Test configuration and fixtures."""

import os
import time

# Must be set BEFORE any app imports so database.py uses test DB on first import
# os.environ takes priority over .env file in pydantic-settings

# Set test encryption key
os.environ["TOKEN_ENCRYPTION_KEY"] = "test-encryption-key-for-unit-tests-only"

# Embedding: use mock provider for fast tests (no model loading)
os.environ.setdefault("EMBEDDING_PROVIDER", "mock")
os.environ.setdefault("EMBEDDING_MODEL", "BAAI/bge-m3")
os.environ.setdefault("EMBEDDING_DIM", "1024")

# Single source of truth for test embedding dimension — use this in all tests
# instead of hardcoding 1024 everywhere.
TEST_EMBEDDING_DIM = int(os.environ.get("EMBEDDING_DIM", "1024"))

# Disable EventPipeline in tests to prevent background DB sessions from leaking
os.environ["EVENT_PIPELINE_ENABLED"] = "false"


# Support parallel testing with worker-specific databases
def get_worker_id():
    """Get pytest-xdist worker ID for database isolation."""
    worker_id = os.getenv("PYTEST_XDIST_WORKER", "master")
    return worker_id


# Set worker-specific database name
base_db_name = os.getenv("TEST_MATRIXONE_DATABASE", "test_dev_agent_v3")
worker_id = get_worker_id()
if worker_id != "master":
    test_db_name = f"{base_db_name}_{worker_id}"
else:
    test_db_name = base_db_name

os.environ["MATRIXONE_DATABASE"] = test_db_name

import pytest
from sqlalchemy.orm import sessionmaker
from core.logging_config import get_logger

logger = get_logger(__name__)

# Test database configuration - completely separate from production
TEST_DATABASE_CONFIG = {
    "host": os.getenv("TEST_MATRIXONE_HOST", "localhost"),
    "port": int(os.getenv("TEST_MATRIXONE_PORT", "6001")),
    "user": os.getenv("TEST_MATRIXONE_USER", "root"),
    "password": os.getenv("TEST_MATRIXONE_PASSWORD", "111"),
    "database": test_db_name,
}


@pytest.fixture(scope="session")
def test_engine():
    """Use the same engine as production, already pointed at test DB via env var."""
    import time
    from api import database
    from api.database import init_db

    engine = database.engine

    # Tighten pool for test: fewer connections, faster recycle.
    # Prevents stale file descriptors when xdist workers dispose/recreate pools.
    engine.pool._recycle = 60
    engine.pool._size = 2
    engine.pool._max_overflow = 3

    for attempt in range(5):
        try:
            init_db()
            # Verify critical tables exist — catch cases where CREATE TABLE
            # silently failed under concurrent DDL pressure from 24 xdist workers.
            from sqlalchemy import inspect as sa_inspect

            tables = set(sa_inspect(engine).get_table_names(schema=engine.url.database))
            required = {
                "agent_events",
                "eval_gate_results",
                "skill_selection_events",
                "skill_selection_learnings",
                "infra_configs",
            }
            missing = required - tables
            if missing:
                raise RuntimeError(f"Tables missing after init_db: {missing}")
            # Ensure microsecond precision on timestamp columns (idempotent).
            # Models now use DateTime6 (MySQL DATETIME(fsp=6)) so new tables are
            # correct.  init_db also runs ALTER for existing tables, but we keep
            # this as a safety net for test DBs that predate the migration.
            from sqlalchemy import text as sa_text

            for ddl in [
                "ALTER TABLE agent_events MODIFY COLUMN created_at DATETIME(6) NOT NULL",
                "ALTER TABLE ctx_snapshots MODIFY COLUMN created_at DATETIME(6) NOT NULL",
                "ALTER TABLE skill_selection_events MODIFY COLUMN created_at DATETIME(6)",
                "ALTER TABLE skills_registry ADD COLUMN tags JSON",
            ]:
                try:
                    with engine.connect() as c:
                        c.execute(sa_text(ddl))
                        c.commit()
                except Exception:
                    pass  # Column already DATETIME(6), or table created with correct type

            # Register edge tool metadata so tests can verify DB state.
            try:
                from api.database import SessionLocal
                from core.skills.catalog import SkillCatalog
                from core.skills.builtin import register_builtin_skills

                catalog = SkillCatalog(SessionLocal)
                register_builtin_skills(catalog, SessionLocal)
            except Exception as e:
                import warnings

                warnings.warn(
                    f"Edge tool registration failed in conftest: {e}",
                    stacklevel=1,
                )
            break
        except Exception:
            if attempt == 4:
                raise
            time.sleep(1.0 + attempt * 0.5)

    # MatrixOne has session-level consistency: DDL executed on connection B is
    # not guaranteed to be visible on connection A that was established before
    # the DDL.  Dispose the pool so every subsequent session gets a fresh
    # connection that can see the newly-created tables.
    engine.dispose()

    yield engine

    # Drain background threads BEFORE dropping the worker DB
    try:
        import logging
        from core.agent.turn_hooks import _bg_threads, _bg_threads_lock, _shutdown_event

        _shutdown_event.set()
        logging.getLogger("httpx").disabled = True
        with _bg_threads_lock:
            threads = list(_bg_threads)
            _bg_threads.clear()
        for t in threads:
            if t.is_alive():
                t.join(timeout=2.0)
    except Exception:
        pass

    # Cleanup worker databases on session end
    _cleanup_worker_databases()
    engine.dispose()


def _cleanup_worker_databases():
    """Clean up worker-specific databases after test session."""
    worker_id = get_worker_id()
    if worker_id == "master":
        return  # Only cleanup from master process

    try:
        import logging
        from matrixone import Client

        # Suppress MatrixOne client logging during teardown (stderr may be closed in xdist workers)
        logging.getLogger("matrixone").setLevel(logging.CRITICAL)
        client = Client(
            host=TEST_DATABASE_CONFIG["host"],
            port=TEST_DATABASE_CONFIG["port"],
            user=TEST_DATABASE_CONFIG["user"],
            password=TEST_DATABASE_CONFIG["password"],
            database="mo_catalog",
        )

        # Clean up this worker's database
        db_name = TEST_DATABASE_CONFIG["database"]
        client.execute(f"DROP DATABASE IF EXISTS `{db_name}`")
        client._engine.dispose()
    except Exception:
        pass  # Ignore cleanup errors


@pytest.fixture(scope="session")
def test_session_factory(test_engine):
    """Create test session factory."""
    # expire_on_commit=False prevents "Could not refresh instance" errors
    # when objects are accessed after commit in parallel tests
    return sessionmaker(bind=test_engine, expire_on_commit=False)


@pytest.fixture
def db_session(test_session_factory):
    """Provide isolated database session for each test."""
    session = test_session_factory()
    real_close = session.close

    # Many integration tests pass `lambda: db_session` into DbConsumer-based
    # services. Those services call `close()` on exit, but for the shared
    # per-test session that would prematurely end the test's view of the DB.
    # Keep close() as a no-op during the test; perform the real close in
    # fixture teardown.
    session.close = lambda: None  # type: ignore[method-assign]
    try:
        yield session
        # Commit if test succeeded (may fail if session is in bad state)
        try:
            session.commit()
        except Exception:
            try:
                session.rollback()
            except Exception:
                pass
    except Exception:
        # Rollback on error
        try:
            session.rollback()
        except Exception:
            pass
        raise
    finally:
        session.close = real_close
        # Expire all to prevent stale data in next test
        try:
            session.expire_all()
        except Exception:
            pass
        # Always close to release locks
        try:
            real_close()
        except Exception:
            pass


@pytest.fixture(scope="session", autouse=True)
def patch_db_engine(test_engine):
    """Ensure database module uses test engine throughout the session.

    Also patches SessionLocal in all router/service modules that imported it
    at module load time (before the test engine was available), so their
    direct `SessionLocal()` calls use the test DB.
    """
    import sys
    from api import database

    test_session_local = sessionmaker(
        autocommit=False, autoflush=False, bind=test_engine, expire_on_commit=False
    )
    original_session_local = database.SessionLocal
    database.engine = test_engine
    database.SessionLocal = test_session_local

    # Patch any already-imported module that holds a local reference to the
    # *original* SessionLocal.  Only replace exact matches to avoid clobbering
    # unrelated attributes that happen to share the name.
    for mod in sys.modules.values():
        if mod is not database and getattr(mod, "SessionLocal", None) is original_session_local:
            try:
                mod.SessionLocal = test_session_local
            except (AttributeError, TypeError):
                pass

    yield


@pytest.fixture(autouse=True)
def override_db_dependency(db_session, monkeypatch):
    """Override get_db_session dependency for all tests."""
    import sys

    from api.database import get_db_session as original_get_db_session

    def mock_get_db_session():
        yield db_session

    # Override the dependency in api.database
    from api import database

    monkeypatch.setattr(database, "get_db_session", mock_get_db_session)

    # Override in api.dependencies (if exists)
    try:
        import api.dependencies

        monkeypatch.setattr(api.dependencies, "get_db_session", mock_get_db_session)
    except (ImportError, AttributeError):
        pass

    # Patch any already-imported module that captured a direct reference via
    # `from api.database import get_db_session`, including test modules.
    patched_modules = []
    for mod in sys.modules.values():
        if getattr(mod, "get_db_session", None) is original_get_db_session:
            try:
                monkeypatch.setattr(mod, "get_db_session", mock_get_db_session)
                patched_modules.append(mod.__name__)
            except (AttributeError, TypeError):
                pass

    # Override FastAPI dependency (only if app is imported)
    try:
        from api.main import app

        app.dependency_overrides[original_get_db_session] = lambda: db_session
    except ImportError:
        pass

    yield

    # Clear overrides
    try:
        from api.main import app

        app.dependency_overrides.pop(original_get_db_session, None)
    except ImportError:
        pass


@pytest.fixture(autouse=True)
def _clear_chat_module_state():
    """Clear module-level global state in chat router for test isolation.

    _session_cache and _shared_llm_client are process-global singletons.
    Without clearing, test A's session data leaks into test B when running
    with pytest -n auto (same process, different tests).

    Only clears if the chat module is already imported — avoids triggering
    a heavy import chain (LLMClient, PromptAssembler, etc.) for tests that
    don't use the chat router at all.
    """
    import sys

    mod = sys.modules.get("api.routers.chat")
    if mod is not None:
        mod._session_cache.clear()
        mod._shared_llm_client = None
        mod._shared_embed_fn = mod._UNSET
        mod._flush_persist_threads()
    yield
    mod = sys.modules.get("api.routers.chat")
    if mod is not None:
        mod._flush_persist_threads()
        mod._session_cache.clear()
        mod._shared_llm_client = None
        mod._shared_embed_fn = mod._UNSET
    # Clean up run_engine tasks to avoid "Task was destroyed but pending" warnings
    engine_mod = sys.modules.get("core.agent.run_engine")
    if engine_mod is not None:
        engine_mod.cleanup_run_tasks()
        engine_mod.cleanup_fan_in_tasks()


# ============================================================================
# Shared SSE / Streaming Test Helpers
# ============================================================================


def parse_sse_events(text: str) -> list[dict]:
    """Parse SSE text into a list of JSON event dicts."""
    import json as _json

    events = []
    for line in text.strip().split("\n"):
        if line.startswith("data: "):
            events.append(_json.loads(line[6:]))
    return events


async def _fake_stream_gen(chunks):
    for c in chunks:
        yield c


def fake_llm_stream(chunks):
    """Return an async generator that yields the given chunks.

    Usage with patch:
        patch("...LLMClient.chat_stream", return_value=fake_llm_stream([...]))
    """
    return _fake_stream_gen(chunks)


def get_auth_headers(
    client,
    db,
    *,
    username="testuser",
    user_id="test_uid",
    email="test@test.com",
    password="password123",
):
    """Create a user (if needed) and return Authorization headers."""
    from api.models import User
    from core.auth.password import hash_password

    user = db.query(User).filter(User.username == username).first()
    if not user:
        user = User(
            user_id=user_id, username=username, email=email, password_hash=hash_password(password)
        )
        db.add(user)
        db.commit()
    last_resp = None
    for attempt in range(10):
        resp = client.post("/auth/login", json={"username": username, "password": password})
        if resp.status_code == 200 and "access_token" in resp.json():
            return {"Authorization": f"Bearer {resp.json()['access_token']}"}
        last_resp = resp
        time.sleep(0.1 * (attempt + 1))
    detail = last_resp.text if last_resp is not None else "no response"
    raise AssertionError(f"Failed to login test user {username}: {detail}")


def flush_persist_threads():
    """Join all background persistence threads. Call before DB assertions."""
    import sys

    mod = sys.modules.get("api.routers.chat")
    if mod is not None:
        mod._flush_persist_threads()


# ============================================================================
# Shared Fixtures for Selector Tests
# ============================================================================


@pytest.fixture
def mock_llm_selector():
    """Mock LLM for selector tests."""
    from unittest.mock import Mock
    import json

    llm = Mock()
    llm.chat = Mock(
        return_value=json.dumps(
            {
                "query_pattern": "review pr",
                "wrong_skills": ["summarize_pr"],
                "correct_skills": ["code_review"],
                "improvement_score": 0.8,
                "evidence": "User feedback",
            }
        )
    )
    llm.chat_with_tools = Mock(return_value={"tool_calls": []})
    return llm


@pytest.fixture
def clean_skill_learning_db(db_session):
    """Clean skill learning tables before test."""
    from api.models import SkillSelectionLearning, GateResult

    db_session.query(SkillSelectionLearning).delete()
    db_session.query(GateResult).filter(GateResult.change_type == "selector").delete()
    db_session.commit()
    yield db_session
    db_session.query(SkillSelectionLearning).delete()
    db_session.query(GateResult).filter(GateResult.change_type == "selector").delete()
    db_session.commit()


@pytest.fixture
def clean_skill_events_db(db_session):
    """Clean skill selection events before test."""
    from api.models import SkillSelectionEvent

    db_session.query(SkillSelectionEvent).delete()
    db_session.commit()
    yield db_session
    db_session.query(SkillSelectionEvent).delete()
    db_session.commit()


# Alias for backward compatibility
@pytest.fixture
def db(db_session):
    """Alias for db_session."""
    return db_session


@pytest.fixture
def db_factory(db_session):
    """Factory that always returns the same test session.

    close() is no-op'd to prevent DbConsumer._db() from closing the shared
    test session. A counter tracks factory vs close calls to detect leaks.
    """
    original_close = db_session.close
    call_count = 0
    close_count = 0

    def _counted_close():
        nonlocal close_count
        close_count += 1

    def factory():
        nonlocal call_count
        call_count += 1
        db_session.close = _counted_close
        return db_session

    yield factory

    db_session.close = original_close
    assert call_count == close_count, (
        f"Session leak: factory called {call_count}x but close called {close_count}x"
    )


def make_run_engine_mock_init():
    """Return a mock __init__ for RunEngine that initialises all required attributes.

    Used by tests that patch RunEngine.__init__ to avoid real DB/event setup.
    """
    from core.db_consumer import DbConsumer
    from core.events.event_logger import EventLogger

    def mock_init(self, db_factory, chat_loop_factory=None):
        DbConsumer.__init__(self, db_factory)
        self._chat_loop_factory = chat_loop_factory
        self._pending_inserts = []
        self._run_event_logger = EventLogger(db_factory)

    return mock_init


@pytest.fixture(autouse=True)
def drain_turn_hooks_bg_threads():
    """Wait for TurnHooks background threads after each test.

    Prevents background threads from outliving the test's DB scope,
    which causes 'no such table' errors in parallel test workers and
    'I/O operation on closed file' logging errors.
    """
    yield
    try:
        from core.agent.turn_hooks import _bg_threads, _bg_threads_lock, _shutdown_event

        _shutdown_event.set()
        with _bg_threads_lock:
            threads = list(_bg_threads)
            _bg_threads.clear()
        for t in threads:
            if t.is_alive():
                t.join(timeout=0.5)
        _shutdown_event.clear()
    except Exception:
        pass
