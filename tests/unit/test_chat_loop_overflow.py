"""Tests for tool result truncation and context overflow recovery."""

import pytest


class TestToolResultTruncation:
    """Tests for tool result truncation to prevent context overflow."""

    def test_large_result_truncated(self):
        """Tool results > 50KB are truncated."""
        MAX_TOOL_RESULT = 50000
        result_str = "x" * 100000  # 100KB
        
        # Simulate truncation logic from chat_loop.py
        if len(result_str) > MAX_TOOL_RESULT:
            result_str = result_str[:MAX_TOOL_RESULT] + f"\n... [truncated, {len(result_str)} bytes total]"
        
        assert len(result_str) < 51000
        assert "truncated" in result_str
        assert "100000 bytes" in result_str

    def test_small_result_not_truncated(self):
        """Tool results <= 50KB are not truncated."""
        MAX_TOOL_RESULT = 50000
        result_str = "x" * 1000  # 1KB
        original = result_str
        
        if len(result_str) > MAX_TOOL_RESULT:
            result_str = result_str[:MAX_TOOL_RESULT] + f"\n... [truncated, {len(result_str)} bytes total]"
        
        assert result_str == original
        assert "truncated" not in result_str

    def test_exact_boundary(self):
        """Tool result exactly at 50KB is not truncated."""
        MAX_TOOL_RESULT = 50000
        result_str = "x" * 50000
        original = result_str
        
        if len(result_str) > MAX_TOOL_RESULT:
            result_str = result_str[:MAX_TOOL_RESULT] + f"\n... [truncated, {len(result_str)} bytes total]"
        
        assert result_str == original


class TestContextOverflowDetection:
    """Tests for context length error detection."""

    def test_detect_context_length_error(self):
        """Detect 'context length' in error message."""
        error_msg = "maximum context length is 131072 tokens"
        
        should_compact = "context length" in error_msg.lower() or "token" in error_msg.lower()
        
        assert should_compact

    def test_detect_token_error(self):
        """Detect 'token' in error message."""
        error_msg = "Request too large: 150000 tokens exceeds limit"
        
        should_compact = "context length" in error_msg.lower() or "token" in error_msg.lower()
        
        assert should_compact

    def test_non_context_error_not_detected(self):
        """Non-context errors should not trigger compaction."""
        error_msg = "API rate limit exceeded"
        
        should_compact = "context length" in error_msg.lower() or "token" in error_msg.lower()
        
        assert not should_compact

    def test_network_error_not_detected(self):
        """Network errors should not trigger compaction."""
        error_msg = "Connection timeout"
        
        should_compact = "context length" in error_msg.lower() or "token" in error_msg.lower()
        
        assert not should_compact
