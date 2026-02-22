"""Shared fixtures for selector tests."""

import json
import pytest
from unittest.mock import Mock

from core.skills.self_improving_selector import SelfImprovingSelector


@pytest.fixture
def self_improving(db, mock_llm_selector):
    """Self-improving selector."""
    si = SelfImprovingSelector(db, mock_llm_selector)
    si._ensure_tables()
    return si
