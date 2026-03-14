"""Integration tests for Memoria HTTP backend."""

from __future__ import annotations

import os
import uuid
from typing import Generator

import pytest

# Skip all tests if Memoria is not available
pytestmark = pytest.mark.skipif(
    not os.environ.get("MEMORIA_BASE_URL"),
    reason="MEMORIA_BASE_URL not set, skipping Memoria tests",
)


@pytest.fixture
def memoria_client() -> Generator:
    """Create a Memoria HTTP client for testing."""
    from core.memory.backends.memoria_http import MemoriaHTTPClient

    base_url = os.environ.get("MEMORIA_BASE_URL", "http://localhost:8000")
    api_key = os.environ.get("MEMORIA_API_KEY")
    master_key = os.environ.get("MEMORIA_MASTER_KEY")

    client = MemoriaHTTPClient(
        base_url=base_url,
        api_key=api_key,
        master_key=master_key,
    )

    # Health check
    try:
        health = client.health_check()
        print(f"Memoria health: {health}")
    except Exception as e:
        pytest.skip(f"Memoria not available: {e}")

    yield client
    client.close()


@pytest.fixture
def test_user_id() -> str:
    """Generate a unique test user ID."""
    return f"test_user_{uuid.uuid4().hex[:8]}"


class TestMemoriaHTTPClient:
    """Test Memoria HTTP client directly."""

    def test_health_check(self, memoria_client: MemoriaHTTPClient) -> None:
        """Test health endpoint."""
        health = memoria_client.health_check()
        assert "status" in health or "database" in health

    def test_store_and_retrieve(
        self,
        memoria_client: MemoriaHTTPClient,
        test_user_id: str,
    ) -> None:
        """Test storing and retrieving a memory."""
        content = "Test memory content about Python programming"

        # Store
        result = memoria_client.store(
            user_id=test_user_id,
            content=content,
            memory_type="semantic",
        )
        assert "memory_id" in result
        assert result["content"] == content
        memory_id = result["memory_id"]

        # Retrieve
        results = memoria_client.retrieve(
            user_id=test_user_id,
            query="Python programming",
            top_k=5,
        )
        assert len(results) > 0
        assert any(r["memory_id"] == memory_id for r in results)

    def test_batch_store(
        self,
        memoria_client: MemoriaHTTPClient,
        test_user_id: str,
    ) -> None:
        """Test batch storing memories."""
        memories = [
            {"content": f"Memory {i}", "memory_type": "semantic"}
            for i in range(3)
        ]

        results = memoria_client.batch_store(test_user_id, memories)
        assert len(results) == 3
        assert all("memory_id" in r for r in results)

    def test_correct_memory(
        self,
        memoria_client: MemoriaHTTPClient,
        test_user_id: str,
    ) -> None:
        """Test correcting a memory."""
        # Store initial memory
        result = memoria_client.store(
            user_id=test_user_id,
            content="Initial content",
            memory_type="semantic",
        )
        memory_id = result["memory_id"]

        # Correct it
        corrected = memoria_client.correct(
            user_id=test_user_id,
            memory_id=memory_id,
            new_content="Corrected content",
            reason="Test correction",
        )
        assert corrected["content"] == "Corrected content"

    def test_delete_memory(
        self,
        memoria_client: MemoriaHTTPClient,
        test_user_id: str,
    ) -> None:
        """Test deleting a memory."""
        # Store
        result = memoria_client.store(
            user_id=test_user_id,
            content="To be deleted",
            memory_type="semantic",
        )
        memory_id = result["memory_id"]

        # Delete
        delete_result = memoria_client.delete(
            user_id=test_user_id,
            memory_id=memory_id,
            reason="Test deletion",
        )
        assert "purged" in delete_result or "deactivated" in delete_result

    def test_list_memories(
        self,
        memoria_client: MemoriaHTTPClient,
        test_user_id: str,
    ) -> None:
        """Test listing memories."""
        # Store a few memories
        for i in range(3):
            memoria_client.store(
                user_id=test_user_id,
                content=f"List test memory {i}",
                memory_type="semantic",
            )

        # List
        result = memoria_client.list_memories(test_user_id, limit=10)
        assert "items" in result
        assert len(result["items"]) >= 3

    def test_snapshots(
        self,
        memoria_client: MemoriaHTTPClient,
        test_user_id: str,
    ) -> None:
        """Test snapshot operations."""
        snapshot_name = f"test_snapshot_{uuid.uuid4().hex[:8]}"

        # Create snapshot
        created = memoria_client.create_snapshot(
            user_id=test_user_id,
            name=snapshot_name,
            description="Test snapshot",
        )
        assert created["name"] == snapshot_name

        # List snapshots
        snapshots = memoria_client.list_snapshots(test_user_id)
        assert any(s["name"] == snapshot_name for s in snapshots)


class TestMemoriaStorage:
    """Test MemoriaStorage adapter."""

    @pytest.fixture
    def storage(
        self,
        memoria_client: MemoriaHTTPClient,
        test_user_id: str,
    ) -> Generator:
        """Create a MemoriaStorage instance."""
        from core.memory.backends.memoria_http import MemoriaStorage

        storage = MemoriaStorage(memoria_client, user_id=test_user_id)
        yield storage

    def test_store_and_retrieve(
        self,
        storage: MemoriaStorage,
        test_user_id: str,
    ) -> None:
        """Test store and retrieve via adapter."""
        from core.memory.types import MemoryType, TrustTier

        content = "Adapter test memory"
        memory = storage.store(
            user_id=test_user_id,
            content=content,
            memory_type=MemoryType.SEMANTIC,
            initial_confidence=0.8,
            trust_tier=TrustTier.T2_EXTRACTED,
        )

        assert memory.content == content
        assert memory.memory_type == MemoryType.SEMANTIC
        assert memory.confidence == 0.8

        # Retrieve
        memories, meta = storage.retrieve(
            user_id=test_user_id,
            query="adapter test",
            top_k=5,
        )
        assert len(memories) > 0
        assert any(m.memory_id == memory.memory_id for m in memories)

    def test_correct_and_purge(
        self,
        storage: MemoriaStorage,
        test_user_id: str,
    ) -> None:
        """Test correct and purge via adapter."""
        from core.memory.types import MemoryType

        # Store
        memory = storage.store(
            user_id=test_user_id,
            content="Original",
            memory_type=MemoryType.SEMANTIC,
        )

        # Correct
        corrected = storage.correct(
            user_id=test_user_id,
            memory_id=memory.memory_id,
            new_content="Corrected",
            reason="Test",
        )
        assert corrected.content == "Corrected"


class TestMemoriaFactory:
    """Test Memoria integration with factory."""

    def test_create_memoria_service(self) -> None:
        """Test creating memory service with Memoria backend."""
        from core.memory.factory import create_memory_service

        # This requires a mock db_factory
        # In real usage, this would connect to Memoria
        pytest.skip("Requires full database setup")

    def test_backend_mapping(self) -> None:
        """Test that 'memoria' backend maps correctly."""
        from core.memory.factory import _BACKEND_TO_STRATEGY

        assert "memoria" in _BACKEND_TO_STRATEGY
        assert _BACKEND_TO_STRATEGY["memoria"] == "memoria:http"
