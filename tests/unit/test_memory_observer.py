"""Unit tests for TypedObserver — Task 3."""

import json
from datetime import datetime
from unittest.mock import MagicMock, patch

import pytest

from core.memory.typed_observer import TypedObserver, _parse_json_array
from core.memory.types import Memory, MemoryType


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def mock_store():
    store = MagicMock()
    store.create.side_effect = lambda m: m
    store.supersede.side_effect = lambda old_id, m: m
    store.list_active.return_value = []
    return store


@pytest.fixture
def mock_llm():
    llm = MagicMock()
    llm.chat_with_tools.return_value = {"content": "[]"}
    return llm


def _embed_fn(text):
    """Deterministic fake embedding: hash-based."""
    h = hash(text) % 1000
    return [h / 1000.0] * 10


@pytest.fixture
def observer(mock_store, mock_llm):
    return TypedObserver(
        store=mock_store, llm_client=mock_llm, embed_fn=_embed_fn,
        contradiction_threshold=0.85,
    )


# ---------------------------------------------------------------------------
# JSON parsing
# ---------------------------------------------------------------------------

class TestParseJsonArray:
    def test_bare_json(self):
        assert _parse_json_array('[{"a": 1}]') == [{"a": 1}]

    def test_code_block(self):
        assert _parse_json_array('```json\n[{"a": 1}]\n```') == [{"a": 1}]

    def test_garbage_around(self):
        assert _parse_json_array('Here: [{"a": 1}] done') == [{"a": 1}]

    def test_empty(self):
        assert _parse_json_array("nothing here") == []


# ---------------------------------------------------------------------------
# Typed extraction
# ---------------------------------------------------------------------------

class TestTypedExtraction:
    def test_extracts_typed_memories(self, observer, mock_llm, mock_store):
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "prefers Go", "confidence": 0.9},
            {"type": "episodic", "content": "discussed testing", "confidence": 0.7},
        ])}
        results = observer.observe("u1", [{"role": "user", "content": "I prefer Go"}])
        assert len(results) == 2
        assert results[0].memory_type == MemoryType.PROFILE
        assert results[1].memory_type == MemoryType.EPISODIC

    def test_invalid_type_defaults_to_episodic(self, observer, mock_llm):
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "invalid_type", "content": "something", "confidence": 0.5},
        ])}
        results = observer.observe("u1", [{"role": "user", "content": "test"}])
        assert results[0].memory_type == MemoryType.EPISODIC

    def test_skips_empty_content(self, observer, mock_llm):
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "", "confidence": 0.9},
        ])}
        results = observer.observe("u1", [{"role": "user", "content": "test"}])
        assert len(results) == 0

    def test_clamps_confidence(self, observer, mock_llm):
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "test", "confidence": 1.5},
        ])}
        results = observer.observe("u1", [{"role": "user", "content": "test"}])
        assert results[0].confidence == 1.0

    def test_no_llm_returns_empty(self, mock_store):
        obs = TypedObserver(store=mock_store, llm_client=None)
        assert obs.observe("u1", [{"role": "user", "content": "test"}]) == []

    def test_records_observed_at(self, observer, mock_llm):
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "semantic", "content": "fact", "confidence": 0.8},
        ])}
        results = observer.observe("u1", [{"role": "user", "content": "test"}])
        assert results[0].observed_at is not None


# ---------------------------------------------------------------------------
# Contradiction detection
# ---------------------------------------------------------------------------

class TestContradictionDetection:
    def test_contradiction_supersedes(self, observer, mock_llm, mock_store):
        """'prefers tabs' then 'prefers spaces' → old superseded."""
        # Existing memory with similar embedding
        old_mem = Memory(
            memory_id="old1", user_id="u1", memory_type=MemoryType.PROFILE,
            content="prefers tabs", confidence=0.8,
            embedding=[0.5] * 10,  # will have high similarity with same-type
        )
        mock_store.list_active.return_value = [old_mem]

        # New extraction
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "prefers spaces", "confidence": 0.9},
        ])}

        # Make embeddings nearly identical to trigger contradiction
        def same_embed(text):
            return [0.5] * 10

        observer.embed_fn = same_embed
        results = observer.observe("u1", [{"role": "user", "content": "I prefer spaces"}])

        assert len(results) == 1
        mock_store.supersede.assert_called_once()
        assert mock_store.supersede.call_args[0][0] == "old1"

    def test_non_contradiction_both_active(self, observer, mock_llm, mock_store):
        """'likes Go' + 'likes Rust' → both active (different embeddings)."""
        old_mem = Memory(
            memory_id="old1", user_id="u1", memory_type=MemoryType.PROFILE,
            content="likes Go", confidence=0.8,
            embedding=[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        )
        mock_store.list_active.return_value = [old_mem]

        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "likes Rust", "confidence": 0.8},
        ])}

        def different_embed(text):
            return [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]

        observer.embed_fn = different_embed
        results = observer.observe("u1", [{"role": "user", "content": "I also like Rust"}])

        assert len(results) == 1
        mock_store.create.assert_called_once()
        mock_store.supersede.assert_not_called()

    def test_no_embedding_skips_contradiction(self, observer, mock_llm, mock_store):
        observer.embed_fn = None
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "test", "confidence": 0.8},
        ])}
        results = observer.observe("u1", [{"role": "user", "content": "test"}])
        assert len(results) == 1
        mock_store.create.assert_called_once()


# ---------------------------------------------------------------------------
# Explicit write (MemoryWriteTool path)
# ---------------------------------------------------------------------------

class TestObserveExplicit:
    def test_writes_directly(self, observer, mock_store):
        result = observer.observe_explicit("u1", "remember this", MemoryType.SEMANTIC)
        assert result.content == "remember this"
        assert result.memory_type == MemoryType.SEMANTIC
        assert result.confidence == 0.9

    def test_runs_contradiction_check(self, observer, mock_store):
        old_mem = Memory(
            memory_id="old1", user_id="u1", memory_type=MemoryType.PROFILE,
            content="old fact", confidence=0.8, embedding=[0.5] * 10,
        )
        mock_store.list_active.return_value = [old_mem]
        observer.embed_fn = lambda t: [0.5] * 10

        observer.observe_explicit("u1", "new fact", MemoryType.PROFILE)
        mock_store.supersede.assert_called_once()
