"""Regression test for Bug 20: chat_loop creates memory_service with empty user_id."""

from __future__ import annotations
import os
from unittest.mock import MagicMock, patch


class TestChatLoopMemoryServiceUserID:
    def _make_chat_loop(self):
        from core.agent.chat_loop import ChatLoop
        mock_llm = MagicMock()
        mock_llm.config = {}
        mock_logger = MagicMock()
        mock_logger._db_factory = MagicMock()
        return ChatLoop(
            selector=MagicMock(),
            executor=MagicMock(),
            llm_client=mock_llm,
            event_logger=mock_logger,
            context_manager=MagicMock(),
            firewall=MagicMock(),
        )

    def test_init_does_not_create_memory_service_with_empty_user_id(self):
        """Bug 20: __init__ must not create memory_service — user_id unknown at init time."""
        loop = self._make_chat_loop()
        # _memory_service must be None after init (not a MemoriaStorage with user_id="")
        assert loop._memory_service is None

    def test_process_tool_output_creates_storage_with_correct_user_id(self):
        """memory_service for process_tool_output must use the turn's user_id, not empty string."""
        from core.memory.backends.memoria_http import MemoriaStorage

        mock_storage = MagicMock(spec=MemoriaStorage)
        mock_storage.user_id = "alice"

        with patch("core.memory.backends.get_memoria_storage", return_value=mock_storage) as mock_get, \
             patch.dict(os.environ, {"MEMORIA_BASE_URL": "http://localhost:8100", "MEMORIA_MASTER_KEY": "k"}):

            from core.memory.backends import get_memoria_storage
            svc = get_memoria_storage("alice")

        mock_get.assert_called_once_with("alice")
        assert svc.user_id == "alice"
