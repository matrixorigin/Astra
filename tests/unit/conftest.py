"""Shared test fixtures."""

import pytest


@pytest.fixture
def db(db_session):
    """Database session for testing (uses shared db_session from root conftest)."""
    yield db_session
