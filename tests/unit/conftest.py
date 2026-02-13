"""Shared test fixtures."""

import pytest
from api.database import get_db_session


@pytest.fixture
def db():
    """Real database session for testing."""
    session = next(get_db_session())
    yield session
    session.close()
