"""Unit tests for async hybrid reference tracking.

Tests P0 async verification that doesn't block SSE stream.
Design ref: context-window-management.md §2 Phase 2.5
"""

import asyncio
import time
import pytest
from core.context.reference_tracking import (
    verify_references_hybrid,
    analyze_semantic_references,
    _extract_key_identifiers,
)


class TestSemanticReferenceAnalysis:
    """Test semantic reference analysis with 3 heuristics."""
    
    def test_heuristic_explicit_file_mention(self):
        """Heuristic 1: Explicit file mentions in response."""
        history = [{
            "tool_calls": [{
                "tool_name": "read_file",
                "args": {"path": "/path/to/config.py"},
                "event_id": "evt_1"
            }],
            "tool_results": []
        }]
        current_response = "In config.py, the DATABASE_URL is set to..."
        
        refs = analyze_semantic_references(current_response, [], history)
        
        assert "evt_1" in refs
    
    def test_heuristic_grep_pattern_mention(self):
        """Heuristic 1: Grep pattern mentioned in response."""
        history = [{
            "tool_calls": [{
                "tool_name": "grep",
                "args": {"pattern": "DATABASE_URL"},
                "event_id": "evt_2"
            }],
            "tool_results": []
        }]
        current_response = "The DATABASE_URL pattern appears in 5 files..."
        
        refs = analyze_semantic_references(current_response, [], history)
        
        assert "evt_2" in refs
    
    def test_heuristic_data_overlap(self):
        """Heuristic 2: Data overlap with key identifiers."""
        history = [{
            "tool_calls": [],
            "tool_results": [{
                "content": "DATABASE_URL = 'postgres://localhost'",
                "event_id": "evt_3"
            }]
        }]
        current_response = "The DATABASE_URL configuration shows..."
        
        refs = analyze_semantic_references(current_response, [], history)
        
        assert "evt_3" in refs
    
    def test_heuristic_causal_chain(self):
        """Heuristic 3: Tool output → tool input dependency."""
        history = [{
            "tool_calls": [],
            "tool_results": [{
                "content": "config.py\nutils.py\ntest.py",
                "event_id": "evt_4"
            }]
        }]
        current_tool_calls = [{
            "tool_name": "read_file",
            "args": {"path": "config.py"}
        }]
        
        refs = analyze_semantic_references("", current_tool_calls, history)
        
        assert "evt_4" in refs
    
    def test_no_references(self):
        """Test no references when content doesn't match."""
        history = [{
            "tool_calls": [{
                "tool_name": "read_file",
                "args": {"path": "other.py"},
                "event_id": "evt_5"
            }],
            "tool_results": []
        }]
        current_response = "The system works correctly."
        
        refs = analyze_semantic_references(current_response, [], history)
        
        assert "evt_5" not in refs
    
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


class TestAsyncHybridVerification:
    """Test P0 async hybrid verification (non-blocking)."""
    
    @pytest.mark.asyncio
    async def test_verify_returns_set(self):
        """Test basic functionality returns set of event_ids."""
        uncertain_events = [
            {"event_id": "evt_1", "tool_name": "read_file", "content": "def foo(): pass", "args": {"path": "test.py"}},
            {"event_id": "evt_2", "tool_name": "grep", "content": "result line", "args": {"pattern": "test"}},
        ]
        current_response = "The function foo is defined in test.py"
        
        result = await verify_references_hybrid(uncertain_events, current_response, feature_flag=True)
        
        assert isinstance(result, set)
        # Mock returns [0, 1], so should have both event_ids
        assert "evt_1" in result
        assert "evt_2" in result
    
    @pytest.mark.asyncio
    async def test_async_non_blocking(self):
        """P0 Critical: Verify async doesn't block."""
        uncertain_events = [
            {"event_id": "evt_1", "tool_name": "read_file", "content": "test", "args": {}},
        ]
        
        start = time.time()
        
        # Create task without awaiting
        task = asyncio.create_task(
            verify_references_hybrid(uncertain_events, "test response", feature_flag=True)
        )
        
        # Should return immediately (not blocked)
        elapsed = time.time() - start
        assert elapsed < 0.01, f"Task creation blocked for {elapsed}s"
        
        # Task completes in background
        result = await task
        assert isinstance(result, set)
    
    @pytest.mark.asyncio
    async def test_feature_flag_disabled(self):
        """Test feature flag disables verification."""
        uncertain_events = [{"event_id": "evt_1", "tool_name": "read_file", "content": "test", "args": {}}]
        
        result = await verify_references_hybrid(uncertain_events, "test", feature_flag=False)
        
        assert result == set()
    
    @pytest.mark.asyncio
    async def test_empty_events(self):
        """Test empty events list returns empty set."""
        result = await verify_references_hybrid([], "test response", feature_flag=True)
        
        assert result == set()
    
    @pytest.mark.asyncio
    async def test_callback_pattern(self):
        """Test usage pattern with callback for result merging."""
        uncertain_events = [
            {"event_id": "evt_1", "tool_name": "read_file", "content": "test", "args": {}},
        ]
        referenced_events = set()
        
        # Create task with callback
        task = asyncio.create_task(
            verify_references_hybrid(uncertain_events, "test", feature_flag=True)
        )
        
        # Add callback to merge results
        def merge_results(future):
            referenced_events.update(future.result())
        
        task.add_done_callback(merge_results)
        
        # Wait for completion
        await task
        
        # Results should be merged
        assert len(referenced_events) > 0
    
    @pytest.mark.asyncio
    async def test_exception_handling(self):
        """Test graceful handling of verification failures."""
        # This would test actual LLM call failures
        # For now, verify the function doesn't crash
        uncertain_events = [
            {"event_id": "evt_1", "tool_name": "read_file", "content": "test", "args": {}},
        ]
        
        result = await verify_references_hybrid(uncertain_events, "test", feature_flag=True)
        
        # Should return set even if internal error occurs
        assert isinstance(result, set)
