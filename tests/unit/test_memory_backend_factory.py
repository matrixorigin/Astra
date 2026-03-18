"""Tests for memory backend factory and preferred tool selection."""

from __future__ import annotations

from unittest.mock import patch


class TestMemoryBackendFactory:
    def test_capabilities_for_memoria_include_all_memory_interfaces(self):
        from core.memory.backends import get_memory_backend_capabilities

        capabilities = get_memory_backend_capabilities("memoria")

        assert capabilities.supports_tool("memory_search")
        assert capabilities.supports_tool("memory_store")
        assert capabilities.supports_context_mode("retrieve")
        assert capabilities.resolve_tool("memory_profile") == "memory_profile"
        assert capabilities.resolve_context_mode("search") == "search"

    def test_create_storage_uses_memoria_backend_alias(self):
        with patch.dict(
            "os.environ",
            {
                "MEMORY_BACKEND": "memoria",
                "MEMORIA_BASE_URL": "http://localhost:8100",
                "MEMORIA_MASTER_KEY": "test-key",
            },
            clear=False,
        ):
            from core.memory.backends import create_memory_storage
            from core.memory.backends.memoria_http import MemoriaStorage

            storage = create_memory_storage("alice")

        assert isinstance(storage, MemoriaStorage)
        assert storage.user_id == "alice"

    def test_create_storage_rejects_unknown_backend(self):
        with patch.dict("os.environ", {"MEMORY_BACKEND": "unknown"}, clear=False):
            from core.memory.backends import create_memory_storage

            try:
                create_memory_storage("alice")
                raised = False
            except ValueError as e:
                raised = True
                assert "Unsupported memory backend" in str(e)
        assert raised

    def test_capability_resolution_can_fallback_to_supported_tool_and_mode(self):
        from core.memory.backends.factory import MemoryBackendCapabilities

        capabilities = MemoryBackendCapabilities(
            backend_name="test",
            supported_tools=("memory_retrieve",),
            supported_context_modes=("retrieve",),
        )

        assert capabilities.resolve_tool("memory_search") == "memory_retrieve"
        assert capabilities.resolve_tool("memory_profile") == "memory_retrieve"
        assert capabilities.resolve_context_mode("search") == "retrieve"

    def test_create_editor_rejects_backend_without_write_capability(self):
        from core.memory.backends.factory import MemoryBackendCapabilities
        from core.memory.factory import create_editor

        capabilities = MemoryBackendCapabilities(
            backend_name="test",
            supported_tools=("memory_retrieve",),
            supported_context_modes=("retrieve",),
        )

        with patch("core.memory.backends.get_memory_backend_capabilities", return_value=capabilities):
            try:
                create_editor(user_id="alice")
                raised = False
            except RuntimeError as e:
                raised = True
                assert "does not support writes" in str(e)

        assert raised
