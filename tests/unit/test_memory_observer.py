"""Unit tests for TypedObserver."""

import json
from datetime import datetime
from unittest.mock import MagicMock, patch

import pytest

from core.memory.typed_observer import TypedObserver, _parse_json_array
from core.memory.types import Memory, MemoryType


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
    h = hash(text) % 1000
    return [h / 1000.0] * 10


@pytest.fixture
def observer(mock_store, mock_llm):
    return TypedObserver(
        store=mock_store, llm_client=mock_llm, embed_fn=_embed_fn,
        contradiction_threshold=0.85,
    )


class TestParseJsonArray:
    def test_bare_json(self):
        assert _parse_json_array('[{"a": 1}]') == [{"a": 1}]

    def test_code_block(self):
        assert _parse_json_array('```json\n[{"a": 1}]\n```') == [{"a": 1}]

    def test_garbage_around(self):
        assert _parse_json_array('Here: [{"a": 1}] done') == [{"a": 1}]

    def test_empty(self):
        assert _parse_json_array("nothing here") == []


class TestTypedExtraction:
    def test_extracts_typed_memories(self, observer, mock_llm, mock_store):
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "prefers Go", "confidence": 0.9},
            {"type": "semantic", "content": "discussed testing", "confidence": 0.7},
        ])}
        results, _ = observer.observe("u1", [{"role": "user", "content": "I prefer Go"}])
        assert len(results) == 2
        assert results[0].memory_type == MemoryType.PROFILE
        assert results[1].memory_type == MemoryType.SEMANTIC

    def test_invalid_type_defaults_to_semantic(self, observer, mock_llm):
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "invalid_type", "content": "something", "confidence": 0.5},
        ])}
        results, _ = observer.observe("u1", [{"role": "user", "content": "test"}])
        assert results[0].memory_type == MemoryType.SEMANTIC

    def test_skips_empty_content(self, observer, mock_llm):
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "", "confidence": 0.9},
        ])}
        results, _ = observer.observe("u1", [{"role": "user", "content": "test"}])
        assert len(results) == 0

    def test_clamps_confidence(self, observer, mock_llm):
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "test", "confidence": 1.5},
        ])}
        results, _ = observer.observe("u1", [{"role": "user", "content": "test"}])
        assert results[0].initial_confidence == 1.0

    def test_no_llm_returns_empty(self, mock_store):
        obs = TypedObserver(store=mock_store, llm_client=None)
        results, _ = obs.observe("u1", [{"role": "user", "content": "test"}])
        assert results == []

    def test_records_observed_at(self, observer, mock_llm):
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "semantic", "content": "fact", "confidence": 0.8},
        ])}
        results, _ = observer.observe("u1", [{"role": "user", "content": "test"}])
        assert results[0].observed_at is not None


class TestSensitivityFilter:
    def test_blocks_email(self, observer, mock_llm):
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "email is user@example.com", "confidence": 0.9},
        ])}
        results, _ = observer.observe("u1", [{"role": "user", "content": "test"}])
        assert len(results) == 0

    def test_blocks_aws_key(self, observer, mock_llm):
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "semantic", "content": "key is AKIAIOSFODNN7EXAMPLE", "confidence": 0.8},
        ])}
        results, _ = observer.observe("u1", [{"role": "user", "content": "test"}])
        assert len(results) == 0

    def test_allows_clean_content(self, observer, mock_llm):
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "prefers Go", "confidence": 0.9},
        ])}
        results, _ = observer.observe("u1", [{"role": "user", "content": "test"}])
        assert len(results) == 1

    def test_audit_log_emitted(self, observer, mock_llm, caplog):
        """Sensitivity block emits structured audit log with content_hash."""
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "email is user@example.com", "confidence": 0.9},
        ])}
        import logging
        with caplog.at_level(logging.WARNING):
            observer.observe("u1", [{"role": "user", "content": "test"}])
        assert any("sensitivity_blocked" in r.message or "Sensitivity filter" in r.message for r in caplog.records)


class TestContradictionDetection:
    def test_contradiction_supersedes(self, mock_llm, mock_store):
        mock_db = MagicMock()
        mock_row = MagicMock()
        mock_row.memory_id = "old1"
        mock_row.content = "prefers tabs"
        mock_row.initial_confidence = 0.8
        mock_row.l2_dist = 0.1
        mock_db.execute.return_value.fetchone.return_value = mock_row

        observer = TypedObserver(
            store=mock_store, llm_client=mock_llm,
            embed_fn=lambda t: [0.5] * 10,
            contradiction_threshold=0.85,
            db_factory=lambda: mock_db,
        )

        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "prefers spaces", "confidence": 0.9},
        ])}

        results, _ = observer.observe("u1", [{"role": "user", "content": "I prefer spaces"}])

        assert len(results) == 1
        mock_store.supersede.assert_called_once()
        assert mock_store.supersede.call_args[0][0] == "old1"

    def test_non_contradiction_both_active(self, mock_llm, mock_store):
        mock_db = MagicMock()
        mock_row = MagicMock()
        mock_row.memory_id = "old1"
        mock_row.content = "likes Go"
        mock_row.initial_confidence = 0.8
        mock_row.l2_dist = 5.0
        mock_db.execute.return_value.fetchone.return_value = mock_row

        observer = TypedObserver(
            store=mock_store, llm_client=mock_llm,
            embed_fn=lambda t: [0.0, 1.0] + [0.0] * 8,
            contradiction_threshold=0.85,
            db_factory=lambda: mock_db,
        )

        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "likes Rust", "confidence": 0.8},
        ])}

        results, _ = observer.observe("u1", [{"role": "user", "content": "I also like Rust"}])

        assert len(results) == 1
        mock_store.create.assert_called_once()
        mock_store.supersede.assert_not_called()

    def test_no_embedding_skips_contradiction(self, observer, mock_llm, mock_store):
        observer.embed_fn = None
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "test", "confidence": 0.8},
        ])}
        results, _ = observer.observe("u1", [{"role": "user", "content": "test"}])
        assert len(results) == 1
        mock_store.create.assert_called_once()


class TestObserveExplicit:
    def test_writes_directly(self, observer, mock_store):
        result, _ = observer.observe_explicit("u1", "remember this", MemoryType.SEMANTIC)
        assert result.content == "remember this"
        assert result.memory_type == MemoryType.SEMANTIC
        assert result.initial_confidence == 0.9

    def test_runs_contradiction_check(self, mock_store):
        mock_db = MagicMock()
        mock_row = MagicMock()
        mock_row.memory_id = "old1"
        mock_row.content = "old fact"
        mock_row.initial_confidence = 0.8
        mock_row.l2_dist = 0.1
        mock_db.execute.return_value.fetchone.return_value = mock_row

        observer = TypedObserver(
            store=mock_store, llm_client=None,
            embed_fn=lambda t: [0.5] * 10,
            db_factory=lambda: mock_db,
        )

        observer.observe_explicit("u1", "new fact", MemoryType.PROFILE)
        mock_store.supersede.assert_called_once()

    def test_blocks_sensitive_content(self, observer):
        with pytest.raises(ValueError, match="sensitivity filter"):
            observer.observe_explicit("u1", "password=secret123", MemoryType.SEMANTIC)
