"""Test configuration and fixtures."""

import os

# Must be set BEFORE any app imports so database.py uses test DB on first import
# os.environ takes priority over .env file in pydantic-settings

# Set test encryption key
os.environ["TOKEN_ENCRYPTION_KEY"] = "test-encryption-key-for-unit-tests-only"

# Embedding: use local model with native 384 dimensions for dev/test
os.environ.setdefault("EMBEDDING_PROVIDER", "local")
os.environ.setdefault("EMBEDDING_MODEL", "all-MiniLM-L6-v2")
os.environ.setdefault("EMBEDDING_DIM", "384")

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
    "database": test_db_name
}


@pytest.fixture(scope="session")
def test_engine():
    """Use the same engine as production, already pointed at test DB via env var."""
    import time
    from api import database
    from api.database import init_db

    engine = database.engine
    for attempt in range(5):
        try:
            init_db()
            # Verify critical tables exist — catch cases where CREATE TABLE
            # silently failed under concurrent DDL pressure from 24 xdist workers.
            from sqlalchemy import inspect as sa_inspect
            tables = set(sa_inspect(engine).get_table_names(schema=engine.url.database))
            required = {"agent_events", "eval_gate_results", "skill_selection_events",
                        "skill_selection_learnings", "infra_configs"}
            missing = required - tables
            if missing:
                raise RuntimeError(f"Tables missing after init_db: {missing}")
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
            database="mo_catalog"
        )
        
        # Clean up this worker's database
        db_name = TEST_DATABASE_CONFIG["database"]
        client.execute(f'DROP DATABASE IF EXISTS `{db_name}`')
        client._engine.dispose()
    except Exception:
        pass  # Ignore cleanup errors


@pytest.fixture(scope="session") 
def test_session_factory(test_engine):
    """Create test session factory."""
    return sessionmaker(bind=test_engine)


@pytest.fixture
def db_session(test_session_factory):
    """Provide isolated database session for each test."""
    session = test_session_factory()
    try:
        yield session
        # Commit if test succeeded
        session.commit()
    except Exception:
        # Rollback on error
        session.rollback()
        raise
    finally:
        # Always close to release locks
        session.close()


@pytest.fixture(scope="session", autouse=True)
def patch_db_engine(test_engine):
    """Ensure database module uses test engine throughout the session."""
    from api import database
    database.engine = test_engine
    database.SessionLocal = sessionmaker(autocommit=False, autoflush=False, bind=test_engine)
    yield


@pytest.fixture(autouse=True)
def override_db_dependency(db_session, monkeypatch):
    """Override get_db_session dependency for all tests."""
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


def get_auth_headers(client, db, *, username="testuser", user_id="test_uid",
                     email="test@test.com", password="password123"):
    """Create a user (if needed) and return Authorization headers."""
    from api.models import User
    from core.auth.password import hash_password
    user = db.query(User).filter(User.username == username).first()
    if not user:
        user = User(user_id=user_id, username=username,
                    email=email, password_hash=hash_password(password))
        db.add(user)
        db.commit()
    resp = client.post("/auth/login", json={"username": username, "password": password})
    return {"Authorization": f"Bearer {resp.json()['access_token']}"}


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
    llm.chat = Mock(return_value=json.dumps({
        "query_pattern": "review pr",
        "wrong_skills": ["summarize_pr"],
        "correct_skills": ["code_review"],
        "improvement_score": 0.8,
        "evidence": "User feedback"
    }))
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
