"""Tests for search tool output limits."""

import pytest
from cli.tools.search import MAX_OUTPUT, MAX_MATCHES


class TestSearchLimits:
    """Verify search output limits are reasonable for context window."""

    def test_max_output_under_context_budget(self):
        """MAX_OUTPUT should be ~30KB to leave room for other context."""
        # 30KB ≈ 7K tokens, leaves room for system prompt + history
        assert MAX_OUTPUT <= 32 * 1024
        assert MAX_OUTPUT >= 20 * 1024

    def test_max_matches_reasonable(self):
        """MAX_MATCHES should be limited to prevent output explosion."""
        assert MAX_MATCHES <= 300
        assert MAX_MATCHES >= 100
