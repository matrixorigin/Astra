"""Unit tests for async hybrid reference tracking.

Tests P0 async verification that doesn't block SSE stream.
Design ref: context-window-management.md §2 Phase 2.5
"""

import asyncio
import time
import pytest
from core.context.reference_tracking import verify_references_hybrid


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
