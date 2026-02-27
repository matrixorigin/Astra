"""Tests for EdgeChatLoop — the agentic turn loop between edge and cloud."""

import asyncio
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import pytest

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
        assert result == "The answer is 42."
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
        assert "Hello, world!" in result
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
        assert result == "Done."
        tr = api.calls[1]["tool_results"]
        assert len(tr) == 2

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
            turns.append([
                {"type": "tool_call", "id": f"tc_{i}", "name": "read_file",
                 "arguments": {"path": "hello.txt"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ])

        api = MockAPIClient(turns)
        await edge_chat_loop("Loop forever", api, router, perms, renderer=renderer)
        assert len(api.calls) == 25  # MAX_TURNS, no flush
        assert any("maximum turns" in e.lower() for e in renderer.errors)



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
        assert result == ""

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
    async def test_keyboard_interrupt_handled(self, router, perms, renderer):
        """Ctrl+C during chat_turn → graceful exit."""
        class InterruptAPI:
            async def chat_turn(self, **kwargs):
                raise KeyboardInterrupt()
                yield  # noqa: E501

        result = await edge_chat_loop("Hi", InterruptAPI(), router, perms, renderer=renderer)
        assert any("interrupt" in e.lower() for e in renderer.errors)
        assert result == ""


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
