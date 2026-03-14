"""Unit tests for Memoria HTTP client and storage (with mocks)."""

from __future__ import annotations

from datetime import datetime
from typing import Any
from unittest.mock import MagicMock, patch

import pytest

from core.memory.backends.memoria_http import MemoriaHTTPClient, MemoriaStorage
from core.memory.types import Memory, MemoryType, TrustTier


class TestMemoriaHTTPClientUnit:
    """Unit tests for MemoriaHTTPClient with mocked HTTP responses."""

    @pytest.fixture
    def mock_client(self) -> MemoriaHTTPClient:
        """Create a client with mocked HTTP."""
        client = MemoriaHTTPClient(
            base_url="http://test-memoria:8000",
            api_key="test-api-key",
        )
        client.client = MagicMock()
        return client

    def test_store(self, mock_client: MemoriaHTTPClient) -> None:
        """Test store method."""
        mock_response = MagicMock()
        mock_response.json.return_value = {
            "memory_id": "test-id-123",
            "content": "Test memory",
            "memory_type": "semantic",
            "confidence": 0.8,
            "observed_at": datetime.now().isoformat(),
        }
        mock_client.client.post.return_value = mock_response

        result = mock_client.store(
            user_id="user-123",
            content="Test memory",
            memory_type="semantic",
        )

        assert result["memory_id"] == "test-id-123"
        assert result["content"] == "Test memory"

    def test_retrieve(self, mock_client: MemoriaHTTPClient) -> None:
        """Test retrieve method."""
        mock_response = MagicMock()
        mock_response.json.return_value = [
            {
                "memory_id": "mem-1",
                "content": "Memory 1",
                "memory_type": "semantic",
                "initial_confidence": 0.9,
            },
            {
                "memory_id": "mem-2",
                "content": "Memory 2",
                "memory_type": "semantic",
                "initial_confidence": 0.8,
            },
        ]
        mock_client.client.post.return_value = mock_response

        results = mock_client.retrieve(
            user_id="user-123",
            query="test query",
            top_k=5,
        )

        assert len(results) == 2
        assert results[0]["memory_id"] == "mem-1"

    def test_observe_turn(self, mock_client: MemoriaHTTPClient) -> None:
        """Test observe_turn method."""
        mock_response = MagicMock()
        mock_response.json.return_value = [
            {
                "memory_id": "obs-1",
                "content": "Extracted memory",
                "memory_type": "semantic",
                "initial_confidence": 0.85,
            }
        ]
        mock_client.client.post.return_value = mock_response

        messages = [
            {"role": "user", "content": "I like Python"},
        ]

        results = mock_client.observe_turn(
            user_id="user-123",
            messages=messages,
        )

        assert len(results) == 1
        assert results[0]["content"] == "Extracted memory"

    def test_correct(self, mock_client: MemoriaHTTPClient) -> None:
        """Test correct method."""
        mock_response = MagicMock()
        mock_response.json.return_value = {
            "memory_id": "mem-123",
            "content": "Corrected content",
            "memory_type": "semantic",
        }
        mock_client.client.put.return_value = mock_response

        result = mock_client.correct(
            user_id="user-123",
            memory_id="mem-123",
            new_content="Corrected content",
            reason="Test correction",
        )

        assert result["content"] == "Corrected content"


class TestMemoriaStorageUnit:
    """Unit tests for MemoriaStorage with mocked HTTP client."""

    @pytest.fixture
    def mock_http_client(self) -> MagicMock:
        """Create a mock HTTP client."""
        return MagicMock()

    @pytest.fixture
    def storage(self, mock_http_client: MagicMock) -> MemoriaStorage:
        """Create a storage instance with mock client."""
        return MemoriaStorage(mock_http_client, user_id="test-user")

    def test_store(self, storage: MemoriaStorage, mock_http_client: MagicMock) -> None:
        """Test store method."""
        mock_http_client.store.return_value = {
            "memory_id": "mem-123",
            "content": "Test content",
            "memory_type": "semantic",
            "initial_confidence": 0.8,
            "observed_at": datetime.now().isoformat(),
        }

        memory = storage.store(
            user_id="test-user",
            content="Test content",
            memory_type=MemoryType.SEMANTIC,
            initial_confidence=0.8,
            trust_tier=TrustTier.T2_CURATED,
        )

        assert isinstance(memory, Memory)
        assert memory.content == "Test content"
        assert memory.memory_type == MemoryType.SEMANTIC

    def test_retrieve(self, storage: MemoriaStorage, mock_http_client: MagicMock) -> None:
        """Test retrieve method."""
        mock_http_client.retrieve.return_value = {
            "results": [
                {
                    "memory_id": "mem-1",
                    "content": "Memory 1",
                    "memory_type": "semantic",
                    "initial_confidence": 0.9,
                    "observed_at": datetime.now().isoformat(),
                }
            ],
            "explain": {"path": "graph", "count": 1}
        }

        memories, meta = storage.retrieve(
            user_id="test-user",
            query="test",
            top_k=5,
        )

        assert len(memories) == 1
        assert isinstance(memories[0], Memory)
        assert meta["source"] == "memoria"

    def test_observe_turn(self, storage: MemoriaStorage, mock_http_client: MagicMock) -> None:
        """Test observe_turn method."""
        mock_http_client.observe_turn.return_value = [
            {
                "memory_id": "obs-1",
                "content": "Extracted memory",
                "memory_type": "semantic",
                "initial_confidence": 0.85,
                "observed_at": datetime.now().isoformat(),
            }
        ]

        messages = [
            {"role": "user", "content": "I like Python"},
        ]

        memories = storage.observe_turn(
            user_id="test-user",
            messages=messages,
        )

        assert len(memories) == 1
        assert isinstance(memories[0], Memory)
        assert memories[0].content == "Extracted memory"

    def test_run_pipeline(self, storage: MemoriaStorage, mock_http_client: MagicMock) -> None:
        """Test run_pipeline method."""
        mock_http_client.observe_turn.return_value = [
            {
                "memory_id": "obs-1",
                "content": "Extracted memory",
                "memory_type": "semantic",
                "initial_confidence": 0.85,
                "observed_at": datetime.now().isoformat(),
            },
            {
                "memory_id": "obs-2",
                "content": "Another memory",
                "memory_type": "profile",
                "initial_confidence": 0.9,
                "observed_at": datetime.now().isoformat(),
            },
        ]

        messages = [
            {"role": "user", "content": "I like Python and tea"},
        ]

        result = storage.run_pipeline(
            user_id="test-user",
            messages=messages,
        )

        assert result.memories_extracted == 2
        assert result.memories_stored == 2
        assert result.errors == []

    def test_get_profile(self, storage: MemoriaStorage, mock_http_client: MagicMock) -> None:
        """Test get_profile method."""
        mock_http_client.get_profile.return_value = {"profile": "User likes Python"}
        profile = storage.get_profile("test-user")
        assert profile == "User likes Python"

    def test_get_profile_empty(self, storage: MemoriaStorage, mock_http_client: MagicMock) -> None:
        """Test get_profile when no profile exists."""
        mock_http_client.get_profile.return_value = {"profile": None}
        profile = storage.get_profile("test-user")
        assert profile is None

    def test_correct(self, storage: MemoriaStorage, mock_http_client: MagicMock) -> None:
        """Test correct method."""
        mock_http_client.correct.return_value = {
            "memory_id": "mem-123",
            "content": "Corrected content",
            "memory_type": "semantic",
            "initial_confidence": 0.8,
            "observed_at": datetime.now().isoformat(),
        }

        memory = storage.correct(
            user_id="test-user",
            memory_id="mem-123",
            new_content="Corrected content",
            reason="Test",
        )

        assert isinstance(memory, Memory)
        assert memory.content == "Corrected content"

    def test_purge(self, storage: MemoriaStorage, mock_http_client: MagicMock) -> None:
        """Test purge method."""
        mock_http_client.purge.return_value = {"purged": 3}

        result = storage.purge(
            user_id="test-user",
            memory_ids=["mem-1", "mem-2", "mem-3"],
            reason="Test purge",
        )

        assert result.deactivated == 3


class TestMemoriaStorageToMemory:
    """Test _to_memory conversion logic."""

    @pytest.fixture
    def storage(self) -> MemoriaStorage:
        """Create a storage instance."""
        mock_client = MagicMock(spec=MemoriaHTTPClient)
        return MemoriaStorage(mock_client, user_id="test-user")

    def test_to_memory_with_string_datetime(self, storage: MemoriaStorage) -> None:
        """Test _to_memory handles string datetime."""
        data = {
            "memory_id": "mem-123",
            "content": "Test",
            "memory_type": "semantic",
            "initial_confidence": 0.8,
            "observed_at": "2024-01-15T10:30:00",
        }

        memory = storage._to_memory(data, "test-user")

        assert isinstance(memory.observed_at, datetime)
        assert memory.observed_at.year == 2024

    def test_to_memory_with_iso_datetime(self, storage: MemoriaStorage) -> None:
        """Test _to_memory handles ISO format with timezone."""
        data = {
            "memory_id": "mem-123",
            "content": "Test",
            "memory_type": "semantic",
            "initial_confidence": 0.8,
            "observed_at": "2024-01-15T10:30:00+00:00",
        }

        memory = storage._to_memory(data, "test-user")

        assert isinstance(memory.observed_at, datetime)

    def test_to_memory_fallback_to_confidence(self, storage: MemoriaStorage) -> None:
        """Test _to_memory falls back to 'confidence' if 'initial_confidence' not present."""
        data = {
            "memory_id": "mem-123",
            "content": "Test",
            "memory_type": "semantic",
            "confidence": 0.75,  # Old field name
        }

        memory = storage._to_memory(data, "test-user")

        assert memory.initial_confidence == 0.75

    def test_to_memory_with_all_fields(self, storage: MemoriaStorage) -> None:
        """Test _to_memory with all possible fields."""
        data = {
            "memory_id": "mem-123",
            "content": "Test content",
            "memory_type": "semantic",
            "initial_confidence": 0.9,
            "observed_at": datetime.now().isoformat(),
            "is_active": True,
            "superseded_by": None,
        }

        memory = storage._to_memory(data, "test-user")

        assert memory.memory_id == "mem-123"
        assert memory.content == "Test content"
        assert memory.is_active is True
        assert memory.initial_confidence == 0.9


class TestMemoriaStorageNewMethods:
    """Test newly added MemoriaStorage methods."""

    @pytest.fixture
    def mock_http(self) -> MagicMock:
        return MagicMock()

    @pytest.fixture
    def storage(self, mock_http: MagicMock) -> MemoriaStorage:
        return MemoriaStorage(mock_http, user_id="u1")

    def test_create_memory(self, storage: MemoriaStorage, mock_http: MagicMock) -> None:
        from core.memory.types import MemoryType, TrustTier

        mock_http.store.return_value = {
            "memory_id": "m1",
            "content": "x",
            "memory_type": "semantic",
            "confidence": 0.8,
        }
        mem = Memory(memory_id="", user_id="u1", content="x", memory_type=MemoryType.SEMANTIC)
        result = storage.create_memory(mem)
        assert result.content == "x"

    def test_update_memory_content(self, storage: MemoriaStorage, mock_http: MagicMock) -> None:
        mock_http.correct.return_value = {
            "memory_id": "m1",
            "content": "new",
            "memory_type": "semantic",
        }
        storage.update_memory_content("m1", "new")
        mock_http.correct.assert_called_once_with("u1", "m1", "new", reason="content update")

    def test_update_memory_embedding_noop(
        self, storage: MemoriaStorage, mock_http: MagicMock
    ) -> None:
        storage.update_memory_embedding("m1")  # should not raise

    def test_invalidate_profile_noop(self, storage: MemoriaStorage) -> None:
        storage.invalidate_profile("u1")  # should not raise

    def test_generate_session_summary_returns_none(self, storage: MemoriaStorage) -> None:
        assert storage.generate_session_summary("u1", "s1", []) is None

    def test_check_and_summarize_returns_none(self, storage: MemoriaStorage) -> None:
        assert storage.check_and_summarize("u1", "s1", [], 5, None) is None

    def test_get_memory_found(self, storage: MemoriaStorage, mock_http: MagicMock) -> None:
        mock_http.get_memory.return_value = {
            "memory_id": "m1",
            "content": "found",
            "memory_type": "semantic",
            "confidence": 0.9,
        }
        mem = storage.get_memory("m1")
        assert mem is not None
        assert mem.memory_id == "m1"

    def test_get_memory_not_found(self, storage: MemoriaStorage, mock_http: MagicMock) -> None:
        mock_http.get_memory.return_value = None
        assert storage.get_memory("missing") is None

    def test_list_active(self, storage: MemoriaStorage, mock_http: MagicMock) -> None:
        mock_http.list_memories.return_value = {
            "items": [
                {"memory_id": "m1", "content": "a", "memory_type": "semantic", "confidence": 0.8},
                {"memory_id": "m2", "content": "b", "memory_type": "profile", "confidence": 0.9},
            ]
        }
        mems = storage.list_active("u1")
        assert len(mems) == 2

    def test_run_governance(self, storage: MemoriaStorage, mock_http: MagicMock) -> None:
        from core.memory.interfaces import GovernanceReport

        mock_http.consolidate.return_value = {"status": "done"}
        report = storage.run_governance("u1")
        assert isinstance(report, GovernanceReport)

    def test_run_governance_failure_is_silent(
        self, storage: MemoriaStorage, mock_http: MagicMock
    ) -> None:
        from core.memory.interfaces import GovernanceReport

        mock_http.consolidate.side_effect = Exception("network error")
        report = storage.run_governance("u1")
        assert isinstance(report, GovernanceReport)

    def test_health_check(self, storage: MemoriaStorage, mock_http: MagicMock) -> None:
        from core.memory.interfaces import HealthReport

        mock_http.health_check.return_value = {"status": "ok"}
        report = storage.health_check("u1")
        assert isinstance(report, HealthReport)

    def test_get_profile(self, storage: MemoriaStorage, mock_http: MagicMock) -> None:
        mock_http.get_profile.return_value = {"profile": "Alice likes Python"}
        assert storage.get_profile("u1") == "Alice likes Python"

    def test_get_profile_none_on_error(self, storage: MemoriaStorage, mock_http: MagicMock) -> None:
        mock_http.get_profile.side_effect = Exception("not found")
        assert storage.get_profile("u1") is None

    def test_consolidate(self, storage: MemoriaStorage, mock_http: MagicMock) -> None:
        mock_http.consolidate.return_value = {"status": "done"}
        result = storage.consolidate("u1")
        assert result == {"status": "done"}
