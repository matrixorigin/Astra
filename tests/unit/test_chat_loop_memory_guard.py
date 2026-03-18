from __future__ import annotations

from unittest.mock import MagicMock

import pytest

from core.events.models import StreamEventType
from core.memory.policy import MemoryPolicy


def _make_loop():
    from core.agent.chat_loop import ChatLoop

    mock_selector = MagicMock()
    mock_executor = MagicMock()
    mock_llm = MagicMock()
    mock_llm.config = {"model": "test-model"}
    mock_logger = MagicMock()
    mock_logger.create_stream_event.return_value = MagicMock(event_id="evt_1")
    mock_ctx = MagicMock()
    mock_fw = MagicMock()
    return ChatLoop(
        selector=mock_selector,
        executor=mock_executor,
        llm_client=mock_llm,
        event_logger=mock_logger,
        context_manager=mock_ctx,
        firewall=mock_fw,
    )


class TestChatLoopMemoryGuard:
    @pytest.mark.asyncio
    async def test_blocks_non_memory_tool_for_explicit_store_request(self):
        loop = _make_loop()
        loop._current_memory_policy = MemoryPolicy().decide("remember that I use vim by default")
        loop._current_tool_names = {"memory_store", "bash"}

        tc = {
            "id": "call_1",
            "function": {
                "name": "bash",
                "arguments": "{}",
            },
        }
        user_event = MagicMock(event_id="user_evt", causal_chain_id="chain_1")
        messages: list[dict] = []

        events = []
        async for event in loop._execute_single_tool(
            tc=tc,
            fn_name="bash",
            user_id="u1",
            session_id="s1",
            user_input="remember that I use vim by default",
            full_text="I'll store that.",
            user_event=user_event,
            messages=messages,
        ):
            events.append(event)

        assert any(
            e.event_type == StreamEventType.TOOL_RESULT and e.data.get("blocked") is True
            for e in events
        )
        loop.executor.execute_skill_with_feedback.assert_not_called()
        assert messages[0]["role"] == "tool"
        assert "memory_store" in messages[0]["content"]
        assert messages[1]["role"] == "system"
        assert "memory_store" in messages[1]["content"]
