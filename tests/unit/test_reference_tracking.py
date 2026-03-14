"""Unit tests for reference tracking.

Tests heuristic-based reference detection for history compression.
Design ref: context-window-management.md §2 Phase 2.5
"""

import pytest
from core.context.reference_tracking import (
    analyze_semantic_references,
    _extract_key_identifiers,
)


class TestSemanticReferenceAnalysis:
    """Test semantic reference analysis with 3 heuristics."""

    def test_heuristic_explicit_file_mention_basename(self):
        """Heuristic 1: File basename mentioned in response."""
        history = [
            {
                "tool_calls": [
                    {
                        "tool_name": "read_file",
                        "args": {"path": "/path/to/config.py"},
                        "event_id": "evt_1",
                    }
                ],
                "tool_results": [],
            }
        ]
        current_response = "In config.py, the DATABASE_URL is set to..."

        refs = analyze_semantic_references(current_response, [], history)

        assert "evt_1" in refs

    def test_heuristic_explicit_file_mention_full_path(self):
        """Heuristic 1: Full file path mentioned in response."""
        history = [
            {
                "tool_calls": [
                    {
                        "tool_name": "read_file",
                        "args": {"path": "/app/config.py"},
                        "event_id": "evt_1b",
                    }
                ],
                "tool_results": [],
            }
        ]
        current_response = "The file /app/config.py contains..."

        refs = analyze_semantic_references(current_response, [], history)

        assert "evt_1b" in refs

    def test_heuristic_grep_pattern_mention(self):
        """Heuristic 1: Grep pattern mentioned in response."""
        history = [
            {
                "tool_calls": [
                    {"tool_name": "grep", "args": {"pattern": "DATABASE_URL"}, "event_id": "evt_2"}
                ],
                "tool_results": [],
            }
        ]
        current_response = "The DATABASE_URL pattern appears in 5 files..."

        refs = analyze_semantic_references(current_response, [], history)

        assert "evt_2" in refs

    def test_heuristic_data_overlap(self):
        """Heuristic 2: Data overlap with key identifiers."""
        history = [
            {
                "tool_calls": [],
                "tool_results": [
                    {"content": "DATABASE_URL = 'postgres://localhost'", "event_id": "evt_3"}
                ],
            }
        ]
        current_response = "The DATABASE_URL configuration shows..."

        refs = analyze_semantic_references(current_response, [], history)

        assert "evt_3" in refs

    def test_heuristic_causal_chain(self):
        """Heuristic 3: Tool output → tool input dependency."""
        history = [
            {
                "tool_calls": [],
                "tool_results": [{"content": "config.py\nutils.py\ntest.py", "event_id": "evt_4"}],
            }
        ]
        current_tool_calls = [{"tool_name": "read_file", "args": {"path": "config.py"}}]

        refs = analyze_semantic_references("", current_tool_calls, history)

        assert "evt_4" in refs

    def test_no_references_when_no_match(self):
        """Test no references when content doesn't match."""
        history = [
            {
                "tool_calls": [
                    {"tool_name": "read_file", "args": {"path": "other.py"}, "event_id": "evt_5"}
                ],
                "tool_results": [],
            }
        ]
        current_response = "The system works correctly."

        refs = analyze_semantic_references(current_response, [], history)

        assert "evt_5" not in refs

    def test_empty_response_returns_empty_set(self):
        """Test empty response returns empty set."""
        history = [
            {
                "tool_calls": [
                    {"tool_name": "read_file", "args": {"path": "config.py"}, "event_id": "evt_6"}
                ],
                "tool_results": [],
            }
        ]

        refs = analyze_semantic_references("", [], history)

        assert len(refs) == 0

    def test_missing_event_id_skipped(self):
        """Test tool calls without event_id are skipped."""
        history = [
            {
                "tool_calls": [
                    {
                        "tool_name": "read_file",
                        "args": {"path": "config.py"},
                        # No event_id
                    }
                ],
                "tool_results": [],
            }
        ]
        current_response = "In config.py..."

        refs = analyze_semantic_references(current_response, [], history)

        assert len(refs) == 0

    def test_malformed_history_handled_gracefully(self):
        """Test malformed history doesn't crash."""
        history = [
            None,  # Invalid turn
            {},  # Empty turn
            {"tool_calls": None},  # Invalid tool_calls
        ]
        current_response = "Some response"

        # Should not crash
        refs = analyze_semantic_references(current_response, [], history)

        assert isinstance(refs, set)

    def test_extract_key_identifiers_variables(self):
        """Test extraction of variable names."""
        content = "DATABASE_URL = 'postgres://...'\nAPI_KEY: 'secret'"

        identifiers = _extract_key_identifiers(content)

        assert "DATABASE_URL" in identifiers
        assert "API_KEY" in identifiers

    def test_extract_key_identifiers_functions(self):
        """Test extraction of function names."""
        content = "def foo():\n    pass\nfunction bar() { }"

        identifiers = _extract_key_identifiers(content)

        assert "foo" in identifiers
        assert "bar" in identifiers

    def test_extract_key_identifiers_classes(self):
        """Test extraction of class names."""
        content = "class MyClass:\n    pass"

        identifiers = _extract_key_identifiers(content)

        assert "MyClass" in identifiers
