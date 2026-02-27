"""End-to-end test: multi-agent code review workflow.

Tests the full flow: orchestrator → spawn reviewers → fan-in → synthesize.
Uses mock LLM to simulate realistic agent behavior.
"""

import asyncio
import json
import pytest
from unittest.mock import MagicMock, patch, AsyncMock
from sqlalchemy.orm import Session

from core.agent.run import RunStatus
from core.agent.run_engine import (
    RunEngine, _active_runs, _agent_run_events, _run_waiters, _run_tasks, _child_runs,
    _MAX_RESUME_INPUT_CHARS, cleanup_fan_in_tasks,
)
from core.events.models import StreamEvent, StreamEventType


@pytest.fixture(autouse=True)
def clean_state():
    _active_runs.clear()
    _agent_run_events.clear()
    _run_waiters.clear()
    _run_tasks.clear()
    _child_runs.clear()
    cleanup_fan_in_tasks()
    yield
    _active_runs.clear()
    _agent_run_events.clear()
    _run_waiters.clear()
    _run_tasks.clear()
    _child_runs.clear()
    cleanup_fan_in_tasks()


@pytest.fixture
def mock_db():
    db = MagicMock(spec=Session)
    db.execute.return_value.fetchone.return_value = None
    db.execute.return_value.fetchall.return_value = []
    return db


@pytest.fixture
def engine(mock_db):
    from tests.conftest import make_run_engine_mock_init
    with patch.object(RunEngine, '__init__', make_run_engine_mock_init()):
        e = RunEngine(lambda: mock_db)
        e._try_claim_resume = MagicMock(return_value=True)
        return e


def _make_mock_loop(responses: list[str]):
    """Create a mock ChatLoop that yields text responses in sequence.

    Each call to run_step_stream yields the next response from the list.
    If a response starts with 'TOOL:spawn_runs:', it simulates a tool call.
    """
    call_idx = [0]

    class MockLoop:
        _current_run_id = None

        async def run_step_stream(self, **kw):
            idx = call_idx[0]
            call_idx[0] += 1
            text = responses[idx] if idx < len(responses) else "done"

            if text.startswith("TOOL:spawn_runs:"):
                # Simulate async tool call → wait_for
                payload = json.loads(text[len("TOOL:spawn_runs:"):])
                yield StreamEvent(
                    event_type=StreamEventType.TOOL_RESULT,
                    data={"wait_for": payload["wait_for"]},
                )
            else:
                yield StreamEvent(
                    event_type=StreamEventType.TEXT_DELTA,
                    data={"chunk": text},
                )

    return MockLoop()


class TestMultiAgentE2E:

    @pytest.mark.asyncio
    async def test_single_child_run_lifecycle(self, engine):
        """Parent creates one child, child completes, parent resumes."""
        # Child will output "LGTM, no issues found"
        child_loop = _make_mock_loop(["LGTM, no issues found"])
        # Parent will output "Review complete: all clear"
        parent_loop = _make_mock_loop([
            "Starting review...",  # First run (before wait)
            "Review complete: all clear",  # After resume
        ])

        # We need different loops for parent vs child
        loops = {"parent": parent_loop, "child": child_loop}
        current_agent = [None]

        def build_loop(db):
            # Return different loop based on which run is being started
            loop = MagicMock()
            loop._current_run_id = None

            async def stream(**kw):
                user_input = kw.get("user_input", "")
                if "Review for" in user_input:
                    async for ev in child_loop.run_step_stream(**kw):
                        yield ev
                elif "[Async result" in user_input:
                    # This is the resumed parent
                    yield StreamEvent(
                        event_type=StreamEventType.TEXT_DELTA,
                        data={"chunk": "Review complete: all clear"},
                    )
                else:
                    # First parent call — will trigger wait
                    yield StreamEvent(
                        event_type=StreamEventType.TOOL_RESULT,
                        data={"wait_for": "will_be_set_by_engine"},
                    )

            loop.run_step_stream = stream
            return loop

        with patch("api.routers.chat._build_chat_loop", side_effect=build_loop):
            # Create and start parent
            parent = engine.create_run(session_id="s1", user_id="u1", user_input="Review auth.py")

            # Manually create child (simulating what spawn_runs does)
            child = await engine.create_child_run(
                parent_run_id=parent.run_id,
                agent_id="security-reviewer",
                task="Review for security issues in auth.py",
            )

            # Parent should be waiting
            parent.status = RunStatus.WAITING
            parent.waiting_for = f"children:{parent.run_id}"

            # Wait for child task to complete
            await _run_tasks[child.run_id]

        assert child.status == RunStatus.COMPLETED
        assert parent.status == RunStatus.COMPLETED

        # Verify child output was captured
        child_events = _agent_run_events.get(child.run_id, [])
        child_text = "".join(
            ev.get("data", {}).get("chunk", "")
            for ev in child_events if ev.get("event_type") == "text_delta"
        )
        assert "LGTM" in child_text

    @pytest.mark.asyncio
    async def test_fan_out_fan_in_three_reviewers(self, engine):
        """Spawn 3 reviewers, all complete, parent gets aggregated results."""
        reviewer_outputs = {
            "security": "No SQL injection found. Auth looks solid.",
            "perf": "N+1 query on line 42. Add .select_related().",
            "style": "Missing docstring on public method validate().",
        }

        def build_loop(db):
            loop = MagicMock()
            loop._current_run_id = None

            async def stream(**kw):
                user_input = kw.get("user_input", "")
                # Match reviewer by task content
                for key, output in reviewer_outputs.items():
                    if key in user_input.lower():
                        yield StreamEvent(
                            event_type=StreamEventType.TEXT_DELTA,
                            data={"chunk": output},
                        )
                        return
                # Parent resume
                yield StreamEvent(
                    event_type=StreamEventType.TEXT_DELTA,
                    data={"chunk": "All reviews done. Summary: 1 perf issue, 1 style issue."},
                )

            loop.run_step_stream = stream
            return loop

        with patch("api.routers.chat._build_chat_loop", side_effect=build_loop):
            parent = engine.create_run(session_id="s1", user_id="u1", user_input="Review auth module")
            parent.status = RunStatus.WAITING
            parent.waiting_for = f"children:{parent.run_id}"

            children = []
            for agent_id, task in [
                ("security-reviewer", "Review for security issues"),
                ("perf-reviewer", "Review for perf issues"),
                ("style-reviewer", "Review for style issues"),
            ]:
                c = await engine.create_child_run(parent.run_id, agent_id, task)
                children.append(c)

            # Wait for all child tasks to complete
            await asyncio.gather(*[_run_tasks[c.run_id] for c in children])

        # All children completed
        for c in children:
            assert c.status == RunStatus.COMPLETED

        # Parent resumed and completed
        assert parent.status == RunStatus.COMPLETED

        # Verify parent received child results in its input
        assert "child_results" in (parent.context or {}).get("async_result", {})

    @pytest.mark.asyncio
    async def test_child_failure_doesnt_crash_parent(self, engine):
        """If one child fails, parent still gets results from all children."""
        call_count = [0]

        def build_loop(db):
            loop = MagicMock()
            loop._current_run_id = None

            async def stream(**kw):
                user_input = kw.get("user_input", "")
                if user_input == "Review FAIL part":
                    # Only the child with exactly this task fails
                    raise RuntimeError("LLM API error")
                yield StreamEvent(
                    event_type=StreamEventType.TEXT_DELTA,
                    data={"chunk": "Review OK"},
                )

            loop.run_step_stream = stream
            return loop

        with patch("api.routers.chat._build_chat_loop", side_effect=build_loop):
            parent = engine.create_run(session_id="s1", user_id="u1", user_input="Review")
            parent.status = RunStatus.WAITING
            parent.waiting_for = f"children:{parent.run_id}"

            c_ok = await engine.create_child_run(parent.run_id, "reviewer-a", "Review OK part")
            c_fail = await engine.create_child_run(parent.run_id, "reviewer-b", "Review FAIL part")

            # Wait for all child tasks (some may raise)
            await asyncio.gather(
                _run_tasks[c_ok.run_id], _run_tasks[c_fail.run_id],
                return_exceptions=True,
            )

        assert c_ok.status == RunStatus.COMPLETED
        assert c_fail.status == RunStatus.FAILED
        # Parent should still resume (fan-in accepts FAILED children)
        assert parent.status == RunStatus.COMPLETED

    @pytest.mark.asyncio
    async def test_agent_config_injection(self, engine, mock_db):
        """Child run should receive system_prompt from agent config."""
        # Mock DB to return agent config
        mock_db.execute.return_value.fetchone.return_value = (
            json.dumps({
                "system_prompt": "You are a security expert.",
                "allowed_tools": ["read_file"],
            }),
        )

        received_context = {}

        def build_loop(db_factory):
            loop = MagicMock()
            loop._current_run_id = None

            async def stream(**kw):
                received_context.update(kw.get("context", {}))
                yield StreamEvent(
                    event_type=StreamEventType.TEXT_DELTA,
                    data={"chunk": "done"},
                )

            loop.run_step_stream = stream
            return loop

        with patch("api.routers.chat._build_chat_loop", side_effect=build_loop):
            parent = engine.create_run(session_id="s1", user_id="u1", user_input="test")
            parent.status = RunStatus.RUNNING
            child = await engine.create_child_run(parent.run_id, "security-reviewer", "review")
            await asyncio.sleep(0.1)

        assert child.context.get("system_prompt") == "You are a security expert."
        assert child.context.get("allowed_tools") == ["read_file"]

    @pytest.mark.asyncio
    async def test_cancel_parent_stops_children(self, engine):
        """Cancelling parent should propagate to running children."""
        started = asyncio.Event()

        def build_loop(db):
            loop = MagicMock()
            loop._current_run_id = None

            async def stream(**kw):
                started.set()
                await asyncio.sleep(10)  # Simulate long-running child
                yield StreamEvent(event_type=StreamEventType.TEXT_DELTA, data={"chunk": "done"})

            loop.run_step_stream = stream
            return loop

        with patch("api.routers.chat._build_chat_loop", side_effect=build_loop):
            parent = engine.create_run(session_id="s1", user_id="u1", user_input="test")
            parent.status = RunStatus.WAITING
            parent.waiting_for = f"children:{parent.run_id}"

            child = await engine.create_child_run(parent.run_id, "reviewer", "review")
            await started.wait()

            # Save task ref before cancel_run pops it from _run_tasks
            child_task = _run_tasks.get(child.run_id)

            # Cancel parent — should propagate to child
            engine.cancel_run(parent.run_id)

            # Await cancelled task to let finally block run
            if child_task and not child_task.done():
                try:
                    await child_task
                except asyncio.CancelledError:
                    pass
            await asyncio.sleep(0)

        assert parent.status == RunStatus.CANCELLED
        assert child.status == RunStatus.CANCELLED


class TestSeedAgents:

    def test_seed_inserts_agents(self):
        from core.agent.seed_agents import seed_agents, SEED_AGENTS
        db = MagicMock()
        # No existing agents
        db.execute.return_value.fetchone.return_value = None
        count = seed_agents(db)
        assert count == len(SEED_AGENTS)
        db.commit.assert_called_once()

    def test_seed_skips_existing(self):
        from core.agent.seed_agents import seed_agents
        db = MagicMock()
        # All agents already exist
        db.execute.return_value.fetchone.return_value = (1,)
        count = seed_agents(db)
        assert count == 0

    def test_seed_agent_configs(self):
        from core.agent.seed_agents import SEED_AGENTS
        for agent in SEED_AGENTS:
            assert "system_prompt" in agent["agent_config"]
            assert len(agent["agent_config"]["system_prompt"]) > 20
        # Reviewers should have allowed_tools
        reviewers = [a for a in SEED_AGENTS if a["agent_type"] == "reviewer"]
        for r in reviewers:
            assert "allowed_tools" in r["agent_config"]
