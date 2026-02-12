"""Global test configuration."""

import pytest


@pytest.fixture(scope="session", autouse=True)
def init_database():
    """Initialize database tables before running tests."""
    # Create all tables using SQLAlchemy
    from api.database import init_db
    init_db()
