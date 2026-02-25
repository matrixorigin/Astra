"""Test configuration and fixtures."""

import os

# Must be set BEFORE any app imports so database.py uses test DB on first import
# os.environ takes priority over .env file in pydantic-settings

# Set test encryption key
os.environ["TOKEN_ENCRYPTION_KEY"] = "test-encryption-key-for-unit-tests-only"

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
    for attempt in range(3):
        try:
            init_db()
            break
        except Exception:
            if attempt == 2:
                raise
            time.sleep(0.5)

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
    from api.models import SkillSelectionLearning, SelectorGateResult
    db_session.query(SkillSelectionLearning).delete()
    db_session.query(SelectorGateResult).delete()
    db_session.commit()
    yield db_session
    db_session.query(SkillSelectionLearning).delete()
    db_session.query(SelectorGateResult).delete()
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
