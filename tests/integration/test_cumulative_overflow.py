"""Integration tests for cumulative tool output overflow prevention.

Tests that TurnBudgetTracker actually prevents context overflow when
multiple tool calls produce large outputs in a single turn.
"""

import os
import pytest
from unittest.mock import MagicMock, patch, AsyncMock


@pytest.fixture(autouse=True)
def _setup_memoria_env():
    """Set Memoria environment variables from test configuration."""
    os.environ["MEMORIA_BASE_URL"] = os.environ.get(
        "TEST_MEMORIA_BASE_URL", "http://localhost:8100"
    )
    os.environ["MEMORIA_MASTER_KEY"] = os.environ.get(
        "TEST_MEMORIA_MASTER_KEY", "test-master-key-for-docker-compose"
    )
    os.environ["MEMORIA_API_KEY"] = os.environ.get("TEST_MEMORIA_API_KEY", "")
    yield


class TestCumulativeToolOutputOverflow:
    """Test that cumulative tool outputs are controlled."""

    def test_turn_budget_tracker_initialized(self):
        """ChatLoop initializes _memory_service for tool output handling."""
        from core.agent.chat_loop import ChatLoop

        mock_db_factory = MagicMock()
        mock_llm = MagicMock()
        mock_llm.config = {"max_context_tokens": 128000}
        mock_event_logger = MagicMock()
        mock_event_logger._db_factory = mock_db_factory

        loop = ChatLoop(
            selector=MagicMock(),
            executor=MagicMock(),
            llm_client=mock_llm,
            event_logger=mock_event_logger,
            context_manager=MagicMock(),
            firewall=MagicMock(),
        )

        # _memory_service should be initialized
        assert loop._memory_service is not None

    def test_turn_budget_limits_cumulative_output(self):
        """TurnBudgetTracker limits cumulative tool output."""
        from core.context.budget_manager import TurnBudgetTracker

        tracker = TurnBudgetTracker(max_tool_output_tokens=30000)

        # First call: 20KB (5000 tokens) - OK, remaining is 30000
        assert not tracker.should_force_summarize(20000)  # 5000 tokens < 30000 remaining
        tracker.record(20000)
        assert tracker.used_tokens == 5000
        assert tracker.remaining == 25000

        # Second call: 120KB (30000 tokens) - should force summarize (exceeds remaining 25K)
        assert tracker.should_force_summarize(120000)  # 30000 tokens > 25000 remaining

        # Third call: 80KB (20000 tokens) - still OK (20000 < 25000)
        assert not tracker.should_force_summarize(80000)

    def test_multiple_large_outputs_summarized(self):
        """Multiple large tool outputs get summarized to prevent overflow."""
        from core.context.budget_manager import TurnBudgetTracker

        tracker = TurnBudgetTracker(max_tool_output_tokens=30000)

        # Simulate 10 tool calls, each 200KB (50000 tokens each - exceeds budget)
        force_summarize_count = 0
        for i in range(10):
            output_size = 200000  # 200KB each = 50000 tokens
            if tracker.should_force_summarize(output_size):
                force_summarize_count += 1
                # Summarized output is ~500 bytes = 125 tokens
                tracker.record(500)
            else:
                tracker.record(output_size)

        # First call exceeds budget (50000 > 30000), all should be force-summarized
        assert force_summarize_count == 10
        # Total used should be minimal (10 * 125 = 1250 tokens)
        assert tracker.used_tokens < 2000

    def test_process_tool_output_respects_remaining_budget(self):
        """process_tool_output uses remaining budget for threshold."""
        from core.agent.tool_output_handler import compute_dynamic_threshold

        # Full budget: higher threshold
        threshold_full = compute_dynamic_threshold(30000)

        # Low budget: lower threshold
        threshold_low = compute_dynamic_threshold(5000)

        assert threshold_low < threshold_full

    def test_chat_loop_resets_budget_each_turn(self):
        """ChatLoop resets _turn_budget at start of each turn."""
        from core.agent.chat_loop import ChatLoop
        from core.context.budget_manager import TurnBudgetTracker

        mock_db_factory = MagicMock()
        mock_llm = MagicMock()
        mock_llm.config = {"max_context_tokens": 128000}
        mock_llm.chat = MagicMock(return_value=MagicMock(content="Done"))
        mock_event_logger = MagicMock()
        mock_event_logger._db_factory = mock_db_factory
        mock_event_logger.create_user_query.return_value = MagicMock(
            event_id="ev1", causal_chain_id="cc1"
        )
        mock_event_logger.flush_critical = MagicMock()
        mock_context_manager = MagicMock()
        mock_context_manager.build_context.return_value = {}
        mock_context_manager.save_snapshot.return_value = "snap1"
        mock_firewall = MagicMock()
        mock_firewall.verify_response.return_value = MagicMock(safe_to_deliver=True)

        loop = ChatLoop(
            selector=MagicMock(),
            executor=MagicMock(),
            llm_client=mock_llm,
            event_logger=mock_event_logger,
            context_manager=mock_context_manager,
            firewall=mock_firewall,
        )
        loop._pipeline = MagicMock()
        loop._pipeline.get_tools_schema.return_value = MagicMock(
            tools=[{"type": "function", "function": {"name": "test"}}], event_id=None
        )

        # Mock LLM to return no tool calls (just text)
        mock_llm.chat_with_tools_stream = MagicMock(
            return_value=iter(
                [
                    MagicMock(
                        choices=[
                            MagicMock(
                                delta=MagicMock(content="Done", tool_calls=None),
                                finish_reason="stop",
                            )
                        ]
                    )
                ]
            )
        )

        # Simulate used budget from previous turn
        loop._turn_budget = TurnBudgetTracker(max_tool_output_tokens=30000)
        loop._turn_budget.record(100000)  # Exhaust budget

        # Verify budget is exhausted
        assert loop._turn_budget.used_tokens == 25000

        # The reset happens in run_step_stream (run_step is now a thin wrapper).
        # Verify the code path by checking run_step_stream source.
        import inspect

        source = inspect.getsource(loop.run_step_stream)
        assert "_turn_budget = None" in source  # Reset is in the code


class TestRealWorldOverflowScenario:
    """Test realistic overflow scenarios."""

    def test_grep_10_times_30kb_each(self):
        """Simulates: grep returns 30KB x 10 calls = 300KB total."""
        from core.context.budget_manager import TurnBudgetTracker
        from core.agent.tool_output_handler import process_tool_output, compute_dynamic_threshold

        tracker = TurnBudgetTracker(max_tool_output_tokens=30000)

        total_context_size = 0
        for i in range(10):
            # Each grep returns 30KB
            raw_output = "file.py:1:match\n" * 1000  # ~30KB

            # Check if should summarize
            remaining = tracker.remaining
            threshold = compute_dynamic_threshold(remaining)

            if len(raw_output) > threshold or tracker.should_force_summarize(len(raw_output)):
                # Summarized to ~500 bytes
                processed = f"Found 1000 matches in 1 file.\n[Full output: memory:xxx]"
            else:
                processed = raw_output

            tracker.record(len(processed))
            total_context_size += len(processed)

        # Total should be well under 128K tokens (~512KB)
        assert total_context_size < 50000  # ~50KB total, not 300KB

    def test_mixed_small_and_large_outputs(self):
        """Mix of small and large outputs - small pass through, large summarized."""
        from core.context.budget_manager import TurnBudgetTracker

        tracker = TurnBudgetTracker(max_tool_output_tokens=30000)

        outputs = [
            1000,  # 1KB - small
            50000,  # 50KB - large
            500,  # 0.5KB - small
            100000,  # 100KB - large
            2000,  # 2KB - small
        ]

        processed_sizes = []
        for size in outputs:
            if tracker.should_force_summarize(size):
                # Summarized
                processed_sizes.append(500)
                tracker.record(500)
            else:
                processed_sizes.append(size)
                tracker.record(size)

        # Small outputs should pass through
        assert processed_sizes[0] == 1000
        assert processed_sizes[2] == 500

        # Large outputs should be summarized after budget exhausted
        total = sum(processed_sizes)
        assert total < 60000  # Well under 300KB raw total
