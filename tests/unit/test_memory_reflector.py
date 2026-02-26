"""Unit tests for TypedReflector — Task 6."""

import json
from datetime import datetime
from unittest.mock import MagicMock

import pytest

from core.memory.typed_reflector import TypedReflector
from core.memory.types import Memory, MemoryType


@pytest.fixture
def mock_store():
    store = MagicMock()
    store.list_active.return_value = []
    store.create.side_effect = lambda m: m
    store.deactivate.return_value = True
    return store


@pytest.fixture
def mock_llm():
    llm = MagicMock()
    llm.chat_with_tools.return_value = {"content": "{}"}
    return llm


def _episodic(mid, content, embedding=None):
    return Memory(
        memory_id=mid, user_id="u1", memory_type=MemoryType.EPISODIC,
        content=content, confidence=0.7, embedding=embedding,
        observed_at=datetime(2026, 2, 26),
    )


class TestFindClusters:
    def test_no_clusters_when_few_memories(self, mock_store):
        reflector = TypedReflector(store=mock_store, cluster_min_size=3)
        memories = [_episodic("m1", "test", [1.0] * 10)]
        assert reflector._find_clusters(memories) == []

    def test_finds_similar_cluster(self, mock_store):
        reflector = TypedReflector(store=mock_store, cluster_similarity=0.9, cluster_min_size=3)
        # All have identical embeddings → should cluster
        memories = [
            _episodic("m1", "Go test 1", [1.0, 0.0, 0.0]),
            _episodic("m2", "Go test 2", [1.0, 0.0, 0.0]),
            _episodic("m3", "Go test 3", [1.0, 0.0, 0.0]),
        ]
        clusters = reflector._find_clusters(memories)
        assert len(clusters) == 1
        assert len(clusters[0]) == 3

    def test_no_cluster_when_dissimilar(self, mock_store):
        reflector = TypedReflector(store=mock_store, cluster_similarity=0.9, cluster_min_size=3)
        # Orthogonal embeddings → no cluster
        memories = [
            _episodic("m1", "Go", [1.0, 0.0, 0.0]),
            _episodic("m2", "Python", [0.0, 1.0, 0.0]),
            _episodic("m3", "Rust", [0.0, 0.0, 1.0]),
        ]
        clusters = reflector._find_clusters(memories)
        assert len(clusters) == 0


class TestCondenseCluster:
    def test_creates_semantic_memory(self, mock_store, mock_llm):
        mock_llm.chat_with_tools.return_value = {
            "content": json.dumps({"content": "User frequently tests Go code", "confidence": 0.8})
        }
        reflector = TypedReflector(store=mock_store, llm_client=mock_llm)

        cluster = [
            _episodic("m1", "tested Go", [1.0] * 10),
            _episodic("m2", "ran Go tests", [1.0] * 10),
            _episodic("m3", "Go unit test", [1.0] * 10),
        ]
        result = reflector._condense_cluster("u1", cluster)

        assert result is not None
        assert result.memory_type == MemoryType.SEMANTIC
        assert "Go" in result.content
        mock_store.create.assert_called_once()

    def test_deactivates_old_memories(self, mock_store, mock_llm):
        mock_llm.chat_with_tools.return_value = {
            "content": json.dumps({"content": "condensed", "confidence": 0.8})
        }
        reflector = TypedReflector(store=mock_store, llm_client=mock_llm)

        cluster = [
            _episodic("m1", "a", [1.0] * 10),
            _episodic("m2", "b", [1.0] * 10),
            _episodic("m3", "c", [1.0] * 10),
        ]
        reflector._condense_cluster("u1", cluster)

        assert mock_store.deactivate.call_count == 3

    def test_no_llm_returns_none(self, mock_store):
        reflector = TypedReflector(store=mock_store, llm_client=None)
        cluster = [_episodic("m1", "a", [1.0] * 10)]
        assert reflector._condense_cluster("u1", cluster) is None


class TestReflect:
    def test_returns_zero_when_few_episodics(self, mock_store):
        mock_store.list_active.return_value = [_episodic("m1", "test", [1.0] * 10)]
        reflector = TypedReflector(store=mock_store, cluster_min_size=3)
        result = reflector.reflect("u1")
        assert result["promoted"] == 0

    def test_promotes_cluster(self, mock_store, mock_llm):
        mock_store.list_active.return_value = [
            _episodic("m1", "Go test 1", [1.0, 0.0, 0.0]),
            _episodic("m2", "Go test 2", [1.0, 0.0, 0.0]),
            _episodic("m3", "Go test 3", [1.0, 0.0, 0.0]),
        ]
        mock_llm.chat_with_tools.return_value = {
            "content": json.dumps({"content": "User tests Go", "confidence": 0.8})
        }
        reflector = TypedReflector(
            store=mock_store, llm_client=mock_llm,
            cluster_similarity=0.9, cluster_min_size=3,
        )
        result = reflector.reflect("u1")
        assert result["promoted"] == 1
        assert result["clusters_found"] == 1
