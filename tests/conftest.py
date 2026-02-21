"""Test configuration and fixtures."""

import os

# Must be set BEFORE any app imports so database.py uses test DB on first import
# os.environ takes priority over .env file in pydantic-settings
os.environ["MATRIXONE_DATABASE"] = os.getenv("TEST_MATRIXONE_DATABASE", "test_dev_agent_v3")

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
    "database": os.getenv("TEST_MATRIXONE_DATABASE", "test_dev_agent_v3")
}


@pytest.fixture(scope="session")
def test_engine():
    """Use the same engine as production, already pointed at test DB via env var."""
    from api import database
    from api.database import init_db

    engine = database.engine
    init_db()
    yield engine
    engine.dispose()


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

