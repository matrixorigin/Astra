"""Regression test for Bug 19: create_memory_service ImportError."""

from __future__ import annotations
import os
from unittest.mock import patch


class TestCreateMemoryServiceImport:
    def test_importable(self):
        """Bug 19: from core.memory import create_memory_service must not raise ImportError."""
        from core.memory import create_memory_service

        assert callable(create_memory_service)

    def test_returns_memoria_storage(self):
        """create_memory_service must return a MemoriaStorage instance."""
        from core.memory import create_memory_service
        from core.memory.backends.memoria_http import MemoriaStorage

        with patch.dict(
            os.environ,
            {
                "MEMORIA_BASE_URL": "http://localhost:8100",
                "MEMORIA_MASTER_KEY": "test-key",
            },
        ):
            svc = create_memory_service(db_factory=None, user_id="alice")

        assert isinstance(svc, MemoriaStorage)
        assert svc.user_id == "alice"

    def test_db_factory_ignored(self):
        """db_factory param is accepted but ignored (Memoria is HTTP-based)."""
        from core.memory import create_memory_service

        sentinel = object()  # not a real db_factory
        with patch.dict(
            os.environ,
            {
                "MEMORIA_BASE_URL": "http://localhost:8100",
                "MEMORIA_MASTER_KEY": "test-key",
            },
        ):
            svc = create_memory_service(sentinel, user_id="bob")

        assert svc.user_id == "bob"
