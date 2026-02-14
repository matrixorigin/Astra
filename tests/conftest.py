"""Test configuration and fixtures."""

import pytest
from sqlalchemy import create_engine, text
from sqlalchemy.orm import sessionmaker
import os

# Test database configuration - completely separate from production
TEST_DATABASE_CONFIG = {
    "host": os.getenv("TEST_MATRIXONE_HOST", "localhost"),
    "port": int(os.getenv("TEST_MATRIXONE_PORT", "6001")),
    "user": os.getenv("TEST_MATRIXONE_USER", "root"),
    "password": os.getenv("TEST_MATRIXONE_PASSWORD", "111"),
    "database": os.getenv("TEST_MATRIXONE_DATABASE", "test_dev_agent_v3")
}

TEST_DATABASE_URL = f"mysql+pymysql://{TEST_DATABASE_CONFIG['user']}:{TEST_DATABASE_CONFIG['password']}@{TEST_DATABASE_CONFIG['host']}:{TEST_DATABASE_CONFIG['port']}/{TEST_DATABASE_CONFIG['database']}"


@pytest.fixture(scope="session")
def test_engine():
    """Create test database engine with separate MatrixOne instance/database."""
    # Connect without database to create test DB
    base_url = f"mysql+pymysql://{TEST_DATABASE_CONFIG['user']}:{TEST_DATABASE_CONFIG['password']}@{TEST_DATABASE_CONFIG['host']}:{TEST_DATABASE_CONFIG['port']}"
    base_engine = create_engine(base_url)
    
    # Create test database if not exists
    with base_engine.connect() as conn:
        conn.execute(text(f"DROP DATABASE IF EXISTS {TEST_DATABASE_CONFIG['database']}"))
        conn.execute(text(f"CREATE DATABASE {TEST_DATABASE_CONFIG['database']}"))
        conn.commit()
    
    # Connect to test database
    engine = create_engine(TEST_DATABASE_URL, echo=False)
    
    # Create tables in test database
    from api.models import Base
    Base.metadata.create_all(engine)
    
    yield engine


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
    finally:
        # Rollback any uncommitted changes
        session.rollback()
        session.close()


@pytest.fixture(scope="session", autouse=True)
def patch_db_engine(test_engine):
    """Patch database engine to use test database globally."""
    from api import database
    
    # Save originals
    original_engine = database.engine
    original_session_local = database.SessionLocal
    
    # Patch
    database.engine = test_engine
    database.SessionLocal = sessionmaker(autocommit=False, autoflush=False, bind=test_engine)
    
    yield
    
    # Restore (optional, as session ends anyway)
    database.engine = original_engine
    database.SessionLocal = original_session_local


@pytest.fixture(autouse=True)
def override_db_dependency(db_session, monkeypatch):
    """Override get_db_session dependency for all tests."""
    from api.database import get_db_session as original_get_db_session
    
    def mock_get_db_session():
        yield db_session
    
    # Override the dependency in api.database
    from api import database
    monkeypatch.setattr(database, "get_db_session", mock_get_db_session)
    
    # Override in api.dependencies
    import api.dependencies
    monkeypatch.setattr(api.dependencies, "get_db_session", mock_get_db_session)
    
    # Override FastAPI dependency
    from api.main import app
    app.dependency_overrides[original_get_db_session] = lambda: db_session
    
    yield
    
    # Clear overrides
    app.dependency_overrides = {}

