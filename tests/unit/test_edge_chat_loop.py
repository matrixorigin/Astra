"""Tests for EdgeChatLoop — the agentic turn loop between edge and cloud."""

import asyncio
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import pytest

from cli.api_client import AuthenticationError
from cli.edge_chat_loop import edge_chat_loop, load_project_rules, _consume_turn, StderrRenderer
from cli.permissions import PermissionManager
from cli.tools.router import ToolRouter
from cli.tools.file_ops import register_file_tools
from cli.tools.shell import register_shell_tools


# ============================================================================
# Test helpers
# ============================================================================

@dataclass
class RecordingRenderer:
    """Captures all render calls for assertions."""
    texts: list[str] = field(default_factory=list)
    tool_starts: list[str] = field(default_factory=list)
    tool_dones: list[tuple[str, bool]] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)
    infos: list[str] = field(default_factory=list)

    def text(self, chunk: str) -> None:
        self.texts.append(chunk)

    def tool_start(self, name: str, args: dict[str, Any]) -> None:
        self.tool_starts.append(name)

    def tool_done(self, name: str, result: str, error: bool) -> None:
        self.tool_dones.append((name, error))

    def error(self, msg: str) -> None:
        self.errors.append(msg)

    def info(self, msg: str) -> None:
        self.infos.append(msg)

    @property
    def full_text(self) -> str:
        return "".join(self.texts)


class MockAPIClient:
    """Mock API client that returns scripted SSE responses per turn."""

    def __init__(self, turns: list[list[dict[str, Any]]]):
        self._turns = turns
        self._call_count = 0
        self.calls: list[dict[str, Any]] = []  # record what was sent

    async def chat_turn(self, **kwargs):
        self.calls.append(kwargs)
        events = self._turns[self._call_count] if self._call_count < len(self._turns) else []
        self._call_count += 1
        for e in events:
            yield e


# ============================================================================
# Fixtures
# ============================================================================

@pytest.fixture
def project(tmp_path):
    (tmp_path / "hello.txt").write_text("Hello, world!\n")
    (tmp_path / "src").mkdir()
    (tmp_path / "src" / "main.py").write_text("print('hi')\n")
    return tmp_path


@pytest.fixture
def router(project):
    r = ToolRouter()
    register_file_tools(r, str(project))
    register_shell_tools(r, str(project))
    return r


@pytest.fixture
def perms():
    return PermissionManager(auto_approve=True)


@pytest.fixture
def renderer():
    return RecordingRenderer()


# ============================================================================
# Tests: _consume_turn
# ============================================================================

class TestConsumeTurn:
    @pytest.mark.asyncio
    async def test_text_only(self, renderer):
        async def stream():
            yield {"type": "text_delta", "content": "Hello "}
            yield {"type": "text_delta", "content": "world"}
            yield {"type": "turn_complete", "has_tool_calls": False}

        result = await _consume_turn(stream(), renderer)
        assert result.text == "Hello world"
        assert not result.has_tool_calls
        assert renderer.full_text == "Hello world"

    @pytest.mark.asyncio
    async def test_with_tool_calls(self, renderer):
        async def stream():
            yield {"type": "text_delta", "content": "Let me read that."}
            yield {"type": "tool_call", "id": "tc_1", "name": "read_file", "arguments": {"path": "hello.txt"}}
            yield {"type": "usage", "prompt_tokens": 100, "completion_tokens": 20}
            yield {"type": "turn_complete", "has_tool_calls": True}

        result = await _consume_turn(stream(), renderer)
        assert result.has_tool_calls
        assert len(result.tool_calls) == 1
        assert result.tool_calls[0]["name"] == "read_file"
        assert result.usage["prompt_tokens"] == 100

    @pytest.mark.asyncio
    async def test_session_info(self, renderer):
        async def stream():
            yield {"type": "session_info", "session_id": "ses_123", "run_id": "run_456"}
            yield {"type": "turn_complete", "has_tool_calls": False}

        result = await _consume_turn(stream(), renderer)
        assert result.session_id == "ses_123"
        assert result.run_id == "run_456"

    @pytest.mark.asyncio
    async def test_suppress_duplicate_prefix(self, renderer):
        """When LLM repeats previous turn's text, the duplicate is suppressed."""
        async def stream():
            yield {"type": "text_delta", "content": "Let me "}
            yield {"type": "text_delta", "content": "read that."}
            yield {"type": "text_delta", "content": " Here is the result."}
            yield {"type": "turn_complete", "has_tool_calls": False}

        result = await _consume_turn(
            stream(), renderer, suppress_prefix="Let me read that.",
        )
        # Full text is still recorded (for history)
        assert result.text == "Let me read that. Here is the result."
        # But only the non-duplicate part was rendered
        assert renderer.full_text == " Here is the result."

    @pytest.mark.asyncio
    async def test_suppress_no_match_flushes(self, renderer):
        """When LLM says something different, buffer is flushed — no text lost."""
        async def stream():
            yield {"type": "text_delta", "content": "Something new."}
            yield {"type": "turn_complete", "has_tool_calls": False}

        result = await _consume_turn(
            stream(), renderer, suppress_prefix="Let me read that.",
        )
        assert result.text == "Something new."
        assert renderer.full_text == "Something new."

    @pytest.mark.asyncio
    async def test_suppress_flushed_on_tool_call(self, renderer):
        """Dedup buffer is flushed when a tool_call arrives mid-buffer."""
        async def stream():
            yield {"type": "text_delta", "content": "Partial"}
            yield {"type": "tool_call", "id": "tc_1", "name": "read_file",
                   "arguments": {"path": "x"}}
            yield {"type": "turn_complete", "has_tool_calls": True}

        result = await _consume_turn(
            stream(), renderer, suppress_prefix="Something else entirely.",
        )
        assert renderer.full_text == "Partial"
        assert len(result.tool_calls) == 1


# ============================================================================
# Tests: edge_chat_loop
# ============================================================================

class TestEdgeChatLoop:
    @pytest.mark.asyncio
    async def test_single_turn_text_only(self, router, perms, renderer):
        """Cloud returns text, no tool calls → loop exits after 1 turn."""
        api = MockAPIClient([
            [
                {"type": "text_delta", "content": "The answer is 42."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        result = await edge_chat_loop(
            "What is the answer?", api, router, perms, renderer=renderer,
        )
        assert result.text == "The answer is 42."
        assert len(api.calls) == 1
        assert api.calls[0]["messages"] == [{"role": "user", "content": "What is the answer?"}]

    @pytest.mark.asyncio
    async def test_tool_call_then_answer(self, project, router, perms, renderer):
        """Turn 1: cloud requests read_file → Turn 2: cloud gives final answer."""
        api = MockAPIClient([
            # Turn 1: request tool
            [
                {"type": "text_delta", "content": "Let me read that."},
                {"type": "tool_call", "id": "tc_1", "name": "read_file",
                 "arguments": {"path": "hello.txt"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            # Turn 2: final answer
            [
                {"type": "text_delta", "content": "The file says Hello, world!"},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        result = await edge_chat_loop(
            "Read hello.txt", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        assert "Hello, world!" in result.text
        assert len(api.calls) == 2
        # Second call should have tool_results
        tr = api.calls[1]["tool_results"]
        assert len(tr) == 1
        assert tr[0]["name"] == "read_file"
        assert "Hello, world!" in tr[0]["result"]

    @pytest.mark.asyncio
    async def test_multiple_tool_calls_concurrent(self, project, router, perms, renderer):
        """Cloud requests two tools at once → both executed concurrently."""
        api = MockAPIClient([
            [
                {"type": "tool_call", "id": "tc_1", "name": "read_file",
                 "arguments": {"path": "hello.txt"}},
                {"type": "tool_call", "id": "tc_2", "name": "list_dir",
                 "arguments": {"path": "."}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "Done."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        result = await edge_chat_loop(
            "Show me the project", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        assert result.text == "Done."
        tr = api.calls[1]["tool_results"]
        assert len(tr) == 2
        # tool_start and tool_done are paired: each start is immediately followed by its done
        assert renderer.tool_starts == ["read_file", "list_dir"]
        assert renderer.tool_dones == [("read_file", False), ("list_dir", False)]

    @pytest.mark.asyncio
    async def test_cross_turn_dedup(self, project, router, perms, renderer):
        """LLM repeating pre-tool text in the next turn is suppressed."""
        api = MockAPIClient([
            # Turn 1: text + tool call
            [
                {"type": "text_delta", "content": "Let me read that."},
                {"type": "tool_call", "id": "tc_1", "name": "read_file",
                 "arguments": {"path": "hello.txt"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            # Turn 2: LLM repeats the same text, then gives answer
            [
                {"type": "text_delta", "content": "Let me read that."},
                {"type": "text_delta", "content": " The file contains Hello, world!"},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        result = await edge_chat_loop(
            "Read hello.txt", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        # The duplicate "Let me read that." from turn 2 should be suppressed
        rendered = renderer.full_text
        assert rendered.count("Let me read that.") == 1
        assert "Hello, world!" in rendered

    @pytest.mark.asyncio
    async def test_permission_deny_dangerous(self, project, renderer):
        """Dangerous command is blocked even in auto_approve mode."""
        router = ToolRouter()
        register_shell_tools(router, str(project))
        perms = PermissionManager(auto_approve=True)

        api = MockAPIClient([
            [
                {"type": "tool_call", "id": "tc_1", "name": "bash",
                 "arguments": {"command": "sudo rm -rf /"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "I can't do that."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        result = await edge_chat_loop(
            "Delete everything", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        tr = api.calls[1]["tool_results"]
        assert "denied" in tr[0]["result"].lower() or "blocked" in tr[0]["result"].lower()

    @pytest.mark.asyncio
    async def test_permission_ask_denied_by_user(self, project, renderer, monkeypatch):
        """WRITE tool with ask → user says no → denied result sent to cloud."""
        router = ToolRouter()
        register_file_tools(router, str(project))
        perms = PermissionManager(auto_approve=False)
        monkeypatch.setattr("builtins.input", lambda _: "n")

        api = MockAPIClient([
            [
                {"type": "tool_call", "id": "tc_1", "name": "write_file",
                 "arguments": {"path": "new.txt", "content": "bad stuff"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "OK, I won't write."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        result = await edge_chat_loop(
            "Write a file", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        tr = api.calls[1]["tool_results"]
        assert "denied by user" in tr[0]["result"].lower()
        assert not (project / "new.txt").exists()

    @pytest.mark.asyncio
    async def test_unknown_tool(self, router, perms, renderer):
        """Cloud requests a tool that doesn't exist → error result."""
        api = MockAPIClient([
            [
                {"type": "tool_call", "id": "tc_1", "name": "nonexistent_tool",
                 "arguments": {}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "Sorry, that tool doesn't exist."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        await edge_chat_loop("Do something", api, router, perms, renderer=renderer)
        tr = api.calls[1]["tool_results"]
        assert "unknown tool" in tr[0]["result"].lower()

    @pytest.mark.asyncio
    async def test_project_rules_sent_first_turn_only(self, project, router, perms, renderer):
        """Project rules sent on turn 0, None on subsequent turns."""
        (project / ".mo-agent").mkdir()
        (project / ".mo-agent" / "rules.md").write_text("Always be polite.")

        api = MockAPIClient([
            [
                {"type": "tool_call", "id": "tc_1", "name": "read_file",
                 "arguments": {"path": "hello.txt"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "Done."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        await edge_chat_loop(
            "Hi", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        assert "Always be polite" in (api.calls[0]["project_rules"] or "")
        assert api.calls[1].get("project_rules") is None

    @pytest.mark.asyncio
    async def test_session_id_tracked(self, router, perms, renderer):
        """Session ID from cloud response is used in subsequent turns."""
        api = MockAPIClient([
            [
                {"type": "session_info", "session_id": "ses_auto", "run_id": "run_1"},
                {"type": "tool_call", "id": "tc_1", "name": "read_file",
                 "arguments": {"path": "hello.txt"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "Done."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        await edge_chat_loop("Hi", api, router, perms, renderer=renderer)
        # Second call should include the session_id from first response
        assert api.calls[1]["session_id"] == "ses_auto"

    @pytest.mark.asyncio
    async def test_max_turns_limit(self, router, perms, renderer):
        """Loop stops at MAX_TURNS and reports error. No flush call."""
        turns = []
        for i in range(30):
            # Use different arguments each turn to avoid triggering stall detection
            turns.append([
                {"type": "tool_call", "id": f"tc_{i}", "name": "read_file",
                 "arguments": {"path": f"file_{i}.txt"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ])

        api = MockAPIClient(turns)
        await edge_chat_loop("Loop forever", api, router, perms, renderer=renderer)
        assert len(api.calls) == 25  # MAX_TURNS, no flush
        assert any("maximum turns" in e.lower() for e in renderer.errors)

    @pytest.mark.asyncio
    async def test_extra_rules_merged_into_project_rules(
        self, tmp_path, router, perms, renderer,
    ):
        """extra_rules are merged with project_rules and sent on turn 0."""
        (tmp_path / ".mo-agent").mkdir()
        (tmp_path / ".mo-agent" / "rules.md").write_text("base rule")

        api = MockAPIClient([
            [{"type": "turn_complete", "has_tool_calls": False}],
        ])
        await edge_chat_loop(
            "hi", api, router, perms,
            project_root=str(tmp_path),
            renderer=renderer,
            extra_rules="SKILL DEV MODE: my_tool",
        )
        sent_rules = api.calls[0]["project_rules"]
        assert "base rule" in sent_rules
        assert "SKILL DEV MODE: my_tool" in sent_rules

    @pytest.mark.asyncio
    async def test_extra_rules_alone_when_no_project_rules(
        self, tmp_path, router, perms, renderer,
    ):
        """extra_rules work even when there are no project rule files."""
        api = MockAPIClient([
            [{"type": "turn_complete", "has_tool_calls": False}],
        ])
        await edge_chat_loop(
            "hi", api, router, perms,
            project_root=str(tmp_path),
            renderer=renderer,
            extra_rules="SKILL DEV MODE: echo",
        )
        sent_rules = api.calls[0]["project_rules"]
        assert sent_rules == "SKILL DEV MODE: echo"


# ============================================================================
# Realistic multi-turn scenarios
# ============================================================================

class TestRealisticScenarios:
    """End-to-end scenarios that mirror real agent workflows."""

    @pytest.mark.asyncio
    async def test_read_edit_verify_workflow(self, project, router, perms, renderer):
        """3-turn coding workflow: read file → str_replace → read to verify."""
        api = MockAPIClient([
            [
                {"type": "text_delta", "content": "Let me look at main.py."},
                {"type": "tool_call", "id": "tc_1", "name": "read_file",
                 "arguments": {"path": "src/main.py"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "I'll fix the print statement."},
                {"type": "tool_call", "id": "tc_2", "name": "str_replace",
                 "arguments": {"path": "src/main.py",
                               "old_str": "print('hi')",
                               "new_str": "print('hello world')"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "Let me verify the change."},
                {"type": "tool_call", "id": "tc_3", "name": "read_file",
                 "arguments": {"path": "src/main.py"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "Done! Changed to hello world."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        result = await edge_chat_loop(
            "Fix main.py", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        assert len(api.calls) == 4
        # Verify the file was actually modified on disk
        assert "hello world" in (project / "src" / "main.py").read_text()
        # Verify turn 3 tool_results contain the updated content
        tr3 = api.calls[3]["tool_results"]
        assert "hello world" in tr3[0]["result"]

    @pytest.mark.asyncio
    async def test_bash_result_sent_to_cloud(self, project, router, perms, renderer):
        """Shell output is correctly captured and sent back as tool_result."""
        api = MockAPIClient([
            [
                {"type": "tool_call", "id": "tc_1", "name": "bash",
                 "arguments": {"command": "echo 'test output 123'"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "Got it."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        await edge_chat_loop(
            "Run echo", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        tr = api.calls[1]["tool_results"]
        assert "test output 123" in tr[0]["result"]

    @pytest.mark.asyncio
    async def test_tool_execution_error_sent_to_cloud(self, project, router, perms, renderer):
        """Tool that fails (file not found) sends error back to cloud."""
        api = MockAPIClient([
            [
                {"type": "tool_call", "id": "tc_1", "name": "read_file",
                 "arguments": {"path": "nonexistent.txt"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "That file doesn't exist."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        await edge_chat_loop(
            "Read it", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        tr = api.calls[1]["tool_results"]
        assert "error" in tr[0]["result"].lower() or "not found" in tr[0]["result"].lower()

    @pytest.mark.asyncio
    async def test_mixed_permissions_same_turn(self, project, renderer, monkeypatch):
        """Same turn: READ (auto-allow) + EXECUTE (ask, user approves)."""
        router = ToolRouter()
        register_file_tools(router, str(project))
        register_shell_tools(router, str(project))
        perms = PermissionManager(auto_approve=False)
        monkeypatch.setattr("builtins.input", lambda _: "y")

        api = MockAPIClient([
            [
                {"type": "tool_call", "id": "tc_1", "name": "read_file",
                 "arguments": {"path": "hello.txt"}},
                {"type": "tool_call", "id": "tc_2", "name": "bash",
                 "arguments": {"command": "echo ok"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "Both succeeded."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        await edge_chat_loop(
            "Do both", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        tr = api.calls[1]["tool_results"]
        assert len(tr) == 2
        results_text = " ".join(r["result"] for r in tr)
        assert "Hello, world!" in results_text
        assert "ok" in results_text

    @pytest.mark.asyncio
    async def test_cloud_error_event(self, router, perms, renderer):
        """Cloud sends an error event → renderer shows it, loop ends."""
        api = MockAPIClient([
            [
                {"type": "error", "message": "Rate limit exceeded"},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        await edge_chat_loop("Hi", api, router, perms, renderer=renderer)
        assert any("rate limit" in e.lower() for e in renderer.errors)

    @pytest.mark.asyncio
    async def test_auth_error_propagates(self, router, perms, renderer):
        """AuthenticationError from api_client propagates out of edge_chat_loop."""
        class AuthFailAPI:
            async def chat_turn(self, **kwargs):
                raise AuthenticationError("Session expired — please login again")
                yield  # noqa: E501

        with pytest.raises(AuthenticationError, match="Session expired"):
            await edge_chat_loop("Hi", AuthFailAPI(), router, perms, renderer=renderer)

    @pytest.mark.asyncio
    async def test_partial_deny_in_batch(self, project, renderer, monkeypatch):
        """Two WRITE tools — user approves first, denies second."""
        router = ToolRouter()
        register_file_tools(router, str(project))
        perms = PermissionManager(auto_approve=False)
        responses = iter(["y", "n"])
        monkeypatch.setattr("builtins.input", lambda _: next(responses))

        api = MockAPIClient([
            [
                {"type": "tool_call", "id": "tc_1", "name": "write_file",
                 "arguments": {"path": "a.txt", "content": "aaa"}},
                {"type": "tool_call", "id": "tc_2", "name": "write_file",
                 "arguments": {"path": "b.txt", "content": "bbb"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "Wrote a.txt, skipped b.txt."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        await edge_chat_loop(
            "Write files", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        assert (project / "a.txt").read_text() == "aaa"
        assert not (project / "b.txt").exists()
        tr = api.calls[1]["tool_results"]
        denied = [r for r in tr if "denied" in r["result"].lower()]
        assert len(denied) == 1

    @pytest.mark.asyncio
    async def test_model_and_agent_id_forwarded(self, router, perms, renderer):
        """model and agent_id params are forwarded to chat_turn."""
        api = MockAPIClient([
            [
                {"type": "text_delta", "content": "Hi."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        await edge_chat_loop(
            "Hi", api, router, perms, renderer=renderer,
            model="claude-sonnet-4-20250514", agent_id="dev-agent",
        )
        assert api.calls[0]["model"] == "claude-sonnet-4-20250514"
        assert api.calls[0]["agent_id"] == "dev-agent"

    @pytest.mark.asyncio
    async def test_large_output_truncated_in_tool_result(self, project, router, perms, renderer):
        """Shell producing >100KB output gets truncated before sending to cloud."""
        api = MockAPIClient([
            [
                {"type": "tool_call", "id": "tc_1", "name": "bash",
                 "arguments": {"command": "python3 -c \"print('x' * 200_000)\""}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "Got it."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        await edge_chat_loop(
            "Big output", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        tr = api.calls[1]["tool_results"]
        result = tr[0]["result"]
        assert "truncated" in result.lower()
        # Must be under 110KB (100KB + truncation message)
        assert len(result) < 110_000

    @pytest.mark.asyncio
    async def test_network_timeout_handled(self, router, perms, renderer):
        """Network timeout during chat_turn → error rendered, loop exits cleanly."""
        class TimeoutAPI:
            async def chat_turn(self, **kwargs):
                raise TimeoutError("Connection timed out")
                yield  # noqa: E501

        result = await edge_chat_loop("Hi", TimeoutAPI(), router, perms, renderer=renderer)
        assert any("network error" in e.lower() for e in renderer.errors)
        assert result.text == ""

    @pytest.mark.asyncio
    async def test_connection_error_handled(self, router, perms, renderer):
        """ConnectionError during SSE stream → error rendered, loop exits."""
        class ConnErrAPI:
            async def chat_turn(self, **kwargs):
                raise ConnectionError("Connection refused")
                yield  # noqa: E501

        result = await edge_chat_loop("Hi", ConnErrAPI(), router, perms, renderer=renderer)
        assert any("network error" in e.lower() for e in renderer.errors)

    @pytest.mark.asyncio
    async def test_httpx_read_error_retried(self, router, perms, renderer):
        """httpx.ReadError (proxy drops SSE) → retried, not treated as fatal.

        Regression: before the fix, ReadError fell through to the generic
        ``except Exception`` branch which broke out of the retry loop
        immediately, showing a bare "ReadError:" with no retry.
        """
        import httpx

        call_count = 0

        class ReadErrThenOkAPI:
            async def chat_turn(self, **kwargs):
                nonlocal call_count
                call_count += 1
                if call_count == 1:
                    raise httpx.ReadError("proxy dropped connection")
                yield {"type": "text_delta", "content": "recovered"}
                yield {"type": "turn_complete", "has_tool_calls": False}

        result = await edge_chat_loop("Hi", ReadErrThenOkAPI(), router, perms, renderer=renderer)
        assert call_count == 2, "Should have retried after ReadError"
        assert result.text == "recovered"
        assert any("network error" in e.lower() for e in renderer.infos)

    @pytest.mark.asyncio
    async def test_httpx_read_error_exhausts_retries(self, router, perms, renderer):
        """httpx.ReadError on all attempts → error rendered after retries exhausted."""
        import httpx

        call_count = 0

        class AlwaysReadErrAPI:
            async def chat_turn(self, **kwargs):
                nonlocal call_count
                call_count += 1
                raise httpx.ReadError("")
                yield  # noqa: E501

        result = await edge_chat_loop("Hi", AlwaysReadErrAPI(), router, perms, renderer=renderer)
        assert call_count == 3, "Should attempt 1 + 2 retries"
        assert any("network error" in e.lower() for e in renderer.errors)

    @pytest.mark.asyncio
    async def test_httpx_unsupported_protocol_not_retried(self, router, perms, renderer):
        """httpx.UnsupportedProtocol is a config bug — must not retry."""
        import httpx

        call_count = 0

        class BadProtoAPI:
            async def chat_turn(self, **kwargs):
                nonlocal call_count
                call_count += 1
                raise httpx.UnsupportedProtocol("ftp:// not supported")
                yield  # noqa: E501

        result = await edge_chat_loop("Hi", BadProtoAPI(), router, perms, renderer=renderer)
        assert call_count == 1, "Must not retry UnsupportedProtocol"
        assert any("unsupportedprotocol" in e.lower() for e in renderer.errors)

    @pytest.mark.asyncio
    async def test_keyboard_interrupt_handled(self, router, perms, renderer):
        """Ctrl+C during chat_turn → graceful exit."""
        class InterruptAPI:
            async def chat_turn(self, **kwargs):
                raise KeyboardInterrupt()
                yield  # noqa: E501

        result = await edge_chat_loop("Hi", InterruptAPI(), router, perms, renderer=renderer)
        assert any("interrupt" in e.lower() for e in renderer.errors)
        assert result.text == ""


# ============================================================================
# Tests: load_project_rules
# ============================================================================

class TestLoadProjectRules:
    def test_no_rules(self, tmp_path):
        assert load_project_rules(str(tmp_path)) is None

    def test_mo_agent_rules(self, tmp_path):
        (tmp_path / ".mo-agent").mkdir()
        (tmp_path / ".mo-agent" / "rules.md").write_text("Rule 1")
        rules = load_project_rules(str(tmp_path))
        assert "Rule 1" in rules

    def test_claude_md_fallback(self, tmp_path):
        (tmp_path / "CLAUDE.md").write_text("Claude rules")
        rules = load_project_rules(str(tmp_path))
        assert "Claude rules" in rules

    def test_steering_files(self, tmp_path):
        (tmp_path / ".mo-agent").mkdir()
        (tmp_path / ".mo-agent" / "steering").mkdir()
        (tmp_path / ".mo-agent" / "steering" / "a.md").write_text("Steering A")
        (tmp_path / ".mo-agent" / "steering" / "b.md").write_text("Steering B")
        rules = load_project_rules(str(tmp_path))
        assert "Steering A" in rules
        assert "Steering B" in rules

    def test_multiple_sources_combined(self, tmp_path):
        (tmp_path / ".mo-agent").mkdir()
        (tmp_path / ".mo-agent" / "rules.md").write_text("Main rules")
        (tmp_path / "CLAUDE.md").write_text("Claude compat")
        rules = load_project_rules(str(tmp_path))
        assert "Main rules" in rules
        assert "Claude compat" in rules


# ============================================================================
# Heartbeat / ping / timeout tests
# ============================================================================

class TestPingAndTimeout:
    """Tests for ping event handling and app-level timeout in _consume_turn."""

    @pytest.mark.asyncio
    async def test_ping_events_ignored_by_consume_turn(self):
        """Ping events are silently ignored — no effect on TurnResult."""
        async def stream():
            yield {"type": "ping", "ts": 1234567890}
            yield {"type": "text_delta", "content": "hello"}
            yield {"type": "ping", "ts": 1234567891}
            yield {"type": "turn_complete", "has_tool_calls": False}

        renderer = RecordingRenderer()
        result = await _consume_turn(stream(), renderer)
        assert result.text == "hello"
        assert result.has_tool_calls is False
        assert result.error is None
        assert len(renderer.texts) == 1

    @pytest.mark.asyncio
    async def test_app_level_timeout_breaks_turn(self):
        """Turn times out when stream hangs — asyncio.timeout fires."""
        async def slow_stream():
            yield {"type": "text_delta", "content": "a"}
            await asyncio.sleep(5)  # far exceeds timeout — 500x margin
            yield {"type": "text_delta", "content": "b"}
            yield {"type": "turn_complete", "has_tool_calls": False}

        renderer = RecordingRenderer()
        result = await _consume_turn(slow_stream(), renderer, timeout=0.01)
        assert result.text == "a"
        assert result.error is not None
        assert result.error["code"] == "CLIENT_TIMEOUT"
        assert len(renderer.errors) == 1


class TestCloudSkillInjection:
    """Cloud skill descriptions must NOT be duplicated into project_context.

    tool_schemas already carries skill names + descriptions + parameters.
    Duplicating them wastes ~700-1000 tokens every Turn 0.
    """

    def test_prefetch_function_deleted(self):
        """_prefetch_cloud_skills must not exist — it's dead code after removal."""
        import cli.mo_agent_api as m

        assert not hasattr(m, "_prefetch_cloud_skills"), (
            "_prefetch_cloud_skills is dead code with zero callers — delete it"
        )

    def test_extra_rules_contain_skill_usage_rules(self):
        """_run_edge_turn must inject small behavioural skill rules.

        Verifies the injected rules contain the two key behavioural directives:
        1. Call skills directly (don't explore filesystem to infer params)
        2. GitHub skills share one token namespace
        These are NOT in tool_schemas — they're cross-cutting behavioural rules.
        """
        import inspect
        import cli.mo_agent_api as m

        source = inspect.getsource(m._run_edge_turn)
        # Verify behavioural rules are present (content, not variable names)
        assert "call it directly" in source.lower() or "Skill Usage Rules" in source, (
            "Must inject skill usage rules into extra_rules"
        )
        assert "GitHub skills share ONE token" in source, (
            "Must inject GitHub token namespace rule"
        )


# ============================================================================
# Tests: Edge stall detection
# ============================================================================

class TestEdgeStallDetection:
    """Verify the edge chat loop detects and breaks out of tool call loops."""

    @pytest.mark.asyncio
    async def test_stall_breaks_loop(self, project, router, perms, renderer):
        """Same tool+args for 3 consecutive turns → stall detected on 3rd turn,
        tools NOT executed on that turn, nudge sent on turn 3, final answer on turn 3."""
        api = MockAPIClient([
            # Turn 0: read_file → executed normally
            [
                {"type": "tool_call", "id": "tc_1", "name": "read_file",
                 "arguments": {"path": "hello.txt"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            # Turn 1: same tool+args → executed normally
            [
                {"type": "tool_call", "id": "tc_2", "name": "read_file",
                 "arguments": {"path": "hello.txt"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            # Turn 2: same tool+args → stall detected BEFORE execution,
            # tool NOT executed, nudge injected via continue
            [
                {"type": "tool_call", "id": "tc_3", "name": "read_file",
                 "arguments": {"path": "hello.txt"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            # Turn 3: cloud receives nudge, gives final answer
            [
                {"type": "text_delta", "content": "Based on what I have, the file says Hello."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        result = await edge_chat_loop(
            "Read hello.txt repeatedly", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        assert len(api.calls) == 4
        # The 4th call should contain the nudge message
        nudge_call = api.calls[3]
        nudge_msgs = nudge_call.get("messages", [])
        assert any("[SYSTEM]" in (m.get("content", "")) for m in nudge_msgs), \
            "Nudge message should contain [SYSTEM] prefix"
        assert any("stop" in (m.get("content", "")).lower() for m in nudge_msgs), \
            "Nudge message should tell LLM to stop calling tools"
        # No tool_results in the nudge turn
        assert not nudge_call.get("tool_results")
        # Stall detected on turn 2 — tool should NOT have been executed on that turn.
        # Turns 0 and 1 execute tools (2 tool_start calls), turn 2 does not.
        tool_starts = [t for t in renderer.tool_starts if t == "read_file"]
        assert len(tool_starts) == 2, \
            f"Tool should execute on turns 0,1 only (not turn 2); got {len(tool_starts)} executions"

    @pytest.mark.asyncio
    async def test_no_stall_with_different_args(self, project, router, perms, renderer):
        """Different arguments each turn → no stall detected."""
        api = MockAPIClient([
            [
                {"type": "tool_call", "id": "tc_1", "name": "read_file",
                 "arguments": {"path": "hello.txt"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "tool_call", "id": "tc_2", "name": "read_file",
                 "arguments": {"path": "src/main.py"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "tool_call", "id": "tc_3", "name": "list_dir",
                 "arguments": {"path": "."}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "Done."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        result = await edge_chat_loop(
            "Explore the project", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        assert len(api.calls) == 4
        # No nudge — the 4th call should have tool_results from turn 2
        assert api.calls[3].get("tool_results") is not None
        # All 3 tools should have been executed
        assert len(renderer.tool_starts) == 3

    @pytest.mark.asyncio
    async def test_server_stall_signal_stops_loop(self, project, router, perms, renderer):
        """Server sends stall_detected=True in turn_complete → edge stops."""
        api = MockAPIClient([
            [
                {"type": "tool_call", "id": "tc_1", "name": "read_file",
                 "arguments": {"path": "hello.txt"}},
                {"type": "turn_complete", "has_tool_calls": True, "stall_detected": True},
            ],
        ])
        result = await edge_chat_loop(
            "Read hello.txt", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        # Loop should stop after 1 turn because server signaled stall
        assert len(api.calls) == 1

    @pytest.mark.asyncio
    async def test_max_nudges_force_breaks(self, project, router, perms, renderer):
        """After _MAX_NUDGES nudges the edge gives up and force-breaks.

        Timeline (_turn_sigs resets after each nudge):
          Turn 0: api#1 → tool executed, sigs=[s]
          Turn 1: api#2 → tool executed, sigs=[s,s]
          Turn 2: api#3 → stall! nudge#1, sigs reset to []
          Turn 3: api#4 (nudge) → same tool, sigs=[s]
          Turn 4: api#5 → tool executed, sigs=[s,s]
          Turn 5: api#6 → stall! nudge#2, sigs reset to []
          Turn 6: api#7 (nudge) → same tool, sigs=[s]
          Turn 7: api#8 → tool executed, sigs=[s,s]
          Turn 8: api#9 → nudge_count=3 > 2 → force break
        Total: 9 API calls, tools executed on turns 0,1,4,7 (4 times).
        """
        identical_turn = [
            {"type": "tool_call", "id": "tc", "name": "read_file",
             "arguments": {"path": "hello.txt"}},
            {"type": "turn_complete", "has_tool_calls": True},
        ]
        api = MockAPIClient([identical_turn] * 20)  # enough to keep looping
        result = await edge_chat_loop(
            "Read hello.txt", api, router, perms,
            project_root=str(project), renderer=renderer,
        )
        assert len(api.calls) == 9, (
            f"Expected 9 API calls (3 per stall cycle × 3 cycles), "
            f"got {len(api.calls)}"
        )
        assert any("giving up" in msg.lower() for msg in renderer.infos), \
            "Should show force-break info message"
