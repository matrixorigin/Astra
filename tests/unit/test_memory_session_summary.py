"""Unit tests for SessionSummarizer."""

from datetime import datetime, timezone, timedelta
from unittest.mock import MagicMock

import pytest

from core.memory.session_summary import SessionSummarizer, _SESSION_SUMMARY_TAG, _INCREMENTAL_TAG
from core.memory.config import MemoryGovernanceConfig
from core.memory.types import MemoryType


@pytest.fixture
def mock_store():
    store = MagicMock()
    store.create.side_effect = lambda m: m
    store.deactivate.return_value = True
    return store


@pytest.fixture
def messages():
    return [
        {"role": "user", "content": "How do I use async in Python?"},
        {"role": "assistant", "content": "You can use async/await with asyncio..."},
        {"role": "user", "content": "Show me an example"},
        {"role": "assistant", "content": "Here is an example using aiohttp..."},
    ]


class TestIncrementalSummary:
    def test_generated_at_turn_threshold(self, mock_store, messages):
        config = MemoryGovernanceConfig(session_summary_turn_threshold=5)
        s = SessionSummarizer(mock_store, config=config)
        result = s.check_and_summarize("u1", "s1", messages, turn_count=5, session_start=datetime.now(timezone.utc))
        assert result is not None
        assert _INCREMENTAL_TAG in result.content
        assert result.session_id == "s1"  # Session-scoped

    def test_not_generated_below_threshold(self, mock_store, messages):
        config = MemoryGovernanceConfig(session_summary_turn_threshold=50)
        s = SessionSummarizer(mock_store, config=config)
        result = s.check_and_summarize("u1", "s1", messages, turn_count=10, session_start=datetime.now(timezone.utc))
        assert result is None

    def test_generated_at_multiple_of_threshold(self, mock_store, messages):
        config = MemoryGovernanceConfig(session_summary_turn_threshold=5)
        s = SessionSummarizer(mock_store, config=config)
        result = s.check_and_summarize("u1", "s1", messages, turn_count=10, session_start=datetime.now(timezone.utc))
        assert result is not None


class TestFullSummary:
    def test_full_summary_cross_session(self, mock_store, messages):
        s = SessionSummarizer(mock_store)
        result = s.generate_full_summary("u1", "s1", messages)
        assert result is not None
        assert result.session_id is None  # Cross-session
        assert _SESSION_SUMMARY_TAG in result.content
        assert result.memory_type == MemoryType.SEMANTIC

    def test_full_supersedes_incrementals(self, mock_store, messages):
        config = MemoryGovernanceConfig(session_summary_turn_threshold=2)
        s = SessionSummarizer(mock_store, config=config)

        # Generate incrementals — second call must have new messages
        s.check_and_summarize("u1", "s1", messages, turn_count=2, session_start=datetime.now(timezone.utc))
        messages2 = messages + [
            {"role": "user", "content": "What about error handling?"},
            {"role": "assistant", "content": "Use try/except blocks..."},
        ]
        s.check_and_summarize("u1", "s1", messages2, turn_count=4, session_start=datetime.now(timezone.utc))
        assert len(s._incremental_ids.get("s1", [])) == 2

        # Full summary supersedes
        s.generate_full_summary("u1", "s1", messages)
        assert mock_store.deactivate.call_count == 2
        assert "s1" not in s._incremental_ids

    def test_empty_messages_returns_none(self, mock_store):
        s = SessionSummarizer(mock_store)
        assert s.generate_full_summary("u1", "s1", []) is None


class TestLLMFallback:
    def test_uses_llm_when_available(self, mock_store, messages):
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {"content": "Summary of async discussion"}
        s = SessionSummarizer(mock_store, llm_client=mock_llm)
        result = s.generate_full_summary("u1", "s1", messages)
        assert "Summary of async discussion" in result.content
        mock_llm.chat_with_tools.assert_called_once()

    def test_falls_back_to_truncation(self, mock_store, messages):
        s = SessionSummarizer(mock_store, llm_client=None)
        result = s.generate_full_summary("u1", "s1", messages)
        assert result is not None
        assert "async" in result.content.lower()

    def test_llm_error_falls_back(self, mock_store, messages):
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.side_effect = Exception("LLM down")
        s = SessionSummarizer(mock_store, llm_client=mock_llm)
        result = s.generate_full_summary("u1", "s1", messages)
        assert result is not None  # Fallback to truncation


class TestEmbedding:
    def test_embed_fn_called(self, mock_store, messages):
        embed_fn = MagicMock(return_value=[0.1] * 1536)
        s = SessionSummarizer(mock_store, embed_fn=embed_fn)
        result = s.generate_full_summary("u1", "s1", messages)
        assert embed_fn.called
