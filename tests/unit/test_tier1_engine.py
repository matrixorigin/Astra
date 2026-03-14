"""Tests for Tier 1 parallel engine (mocked LLM)."""

import asyncio
import json
import pytest
from unittest.mock import MagicMock, patch

from core.context.intent_routing import Tier1Engine, Tier1Result, RoutingResult


def _mock_llm_response(content: str):
    """Create a mock LLMResponse."""
    resp = MagicMock()
    resp.content = content
    return resp


class TestTier1Classify:
    @pytest.mark.asyncio
    async def test_classify_returns_valid_intent(self):
        engine = Tier1Engine(db_factory=MagicMock())
        mock_resp = _mock_llm_response('{"intent": "preference", "confidence": 0.9}')
        with patch.object(
            engine, "_llm_call", return_value='{"intent": "preference", "confidence": 0.9}'
        ):
            result = await engine._classify("remember I use vim", None)
        assert result.intent == "preference"
        assert result.confidence == 0.9
        assert result.tier == 1
        assert result.matched_by == "llm"

    @pytest.mark.asyncio
    async def test_classify_invalid_json_falls_back(self):
        engine = Tier1Engine(db_factory=MagicMock())
        with patch.object(engine, "_llm_call", return_value="not json"):
            result = await engine._classify("hello", None)
        assert result.intent == "question"
        assert result.confidence == 0.5

    @pytest.mark.asyncio
    async def test_classify_unknown_intent_falls_back(self):
        engine = Tier1Engine(db_factory=MagicMock())
        with patch.object(
            engine, "_llm_call", return_value='{"intent": "unknown", "confidence": 0.8}'
        ):
            result = await engine._classify("hello", None)
        assert result.intent == "question"


class TestTier1Compress:
    @pytest.mark.asyncio
    async def test_compress_returns_text(self):
        engine = Tier1Engine(db_factory=MagicMock())
        with patch.object(engine, "_llm_call", return_value="compressed text"):
            result = await engine._compress("long memory text " * 50)
        assert result == "compressed text"


class TestTier1PruneTools:
    @pytest.mark.asyncio
    async def test_prune_returns_subset(self):
        engine = Tier1Engine(db_factory=MagicMock())
        tools = ["read_file", "bash", "grep", "git_status", "list_prs"]
        with patch.object(engine, "_llm_call", return_value='["list_prs"]'):
            result = await engine._prune_tools("show my PRs", tools)
        assert result == ["list_prs"]

    @pytest.mark.asyncio
    async def test_prune_invalid_json_keeps_all(self):
        engine = Tier1Engine(db_factory=MagicMock())
        tools = ["read_file", "bash"]
        with patch.object(engine, "_llm_call", return_value="not json"):
            result = await engine._prune_tools("hello", tools)
        assert result == tools

    @pytest.mark.asyncio
    async def test_prune_filters_unknown_tools(self):
        engine = Tier1Engine(db_factory=MagicMock())
        tools = ["read_file", "bash"]
        with patch.object(engine, "_llm_call", return_value='["read_file", "unknown_tool"]'):
            result = await engine._prune_tools("read a file", tools)
        assert result == ["read_file"]


class TestTier1Parallel:
    @pytest.mark.asyncio
    async def test_parallel_all_succeed(self):
        engine = Tier1Engine(db_factory=MagicMock())

        async def mock_classify(q, h):
            return RoutingResult(intent="command", confidence=0.9, tier=1, matched_by="llm")

        async def mock_compress(t):
            return "compressed"

        async def mock_prune(q, t):
            return ["bash"]

        with (
            patch.object(engine, "_classify", side_effect=mock_classify),
            patch.object(engine, "_compress", side_effect=mock_compress),
            patch.object(engine, "_prune_tools", side_effect=mock_prune),
        ):
            result = await engine.run_parallel(
                "run tests", memory_text="x" * 200, tool_names=["bash", "grep", "read_file", "git"]
            )

        assert result.routing is not None
        assert result.routing.intent == "command"
        assert result.compressed_memory == "compressed"
        assert result.pruned_tools == ["bash"]

    @pytest.mark.asyncio
    async def test_classify_failure_doesnt_break_others(self):
        engine = Tier1Engine(db_factory=MagicMock())

        async def mock_classify(q, h):
            raise RuntimeError("LLM down")

        async def mock_compress(t):
            return "compressed"

        with (
            patch.object(engine, "_classify", side_effect=mock_classify),
            patch.object(engine, "_compress", side_effect=mock_compress),
        ):
            result = await engine.run_parallel("test", memory_text="x" * 200)

        assert result.routing is None  # classify failed
        assert result.compressed_memory == "compressed"  # compress succeeded

    @pytest.mark.asyncio
    async def test_no_memory_no_tools_skips_tasks(self):
        engine = Tier1Engine(db_factory=MagicMock())

        async def mock_classify(q, h):
            return RoutingResult(intent="question", confidence=0.7, tier=1, matched_by="llm")

        with patch.object(engine, "_classify", side_effect=mock_classify):
            result = await engine.run_parallel("hello")

        assert result.routing is not None
        assert result.compressed_memory is None
        assert result.pruned_tools is None
