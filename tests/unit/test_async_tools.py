"""Tests for async tools: registry, submit_job, submit_workflow, lifecycle."""

import asyncio
import pytest
from unittest.mock import MagicMock, patch, AsyncMock

from core.agent.async_tools import (
    AsyncToolRegistry,
    get_async_tool_registry,
    _workflow_runs,
    _workflow_waits,
    resume_workflow,
    cleanup_stale_workflows,
)


class TestAsyncToolRegistry:

    def setup_method(self):
        self.reg = AsyncToolRegistry()

    def test_register_and_lookup(self):
        async def my_exec(params, run_id=None):
            return {"wait_for": "test:1"}

        schema = {"type": "function", "function": {"name": "my_tool"}}
        self.reg.register("my_tool", my_exec, schema)

        assert self.reg.is_async_tool("my_tool")
        assert not self.reg.is_async_tool("unknown")
        schemas = self.reg.get_schemas()
        assert any(s["function"]["name"] == "my_tool" for s in schemas)

    @pytest.mark.asyncio
    async def test_execute_tracks_handle(self):
        async def my_exec(params, run_id=None):
            return {"wait_for": "test:abc"}

        self.reg.register("my_tool", my_exec, {})
        result = await self.reg.execute("my_tool", {}, "run_123")

        assert result["wait_for"] == "test:abc"
        assert self.reg._handle_to_run["test:abc"] == "run_123"

    @pytest.mark.asyncio
    async def test_execute_no_wait_for(self):
        async def my_exec(params, run_id=None):
            return {"status": "done"}

        self.reg.register("my_tool", my_exec, {})
        result = await self.reg.execute("my_tool", {}, "run_1")
        assert "wait_for" not in result or result.get("wait_for") is None

    def test_resolve_handle(self):
        self.reg._handle_to_run["test:1"] = "run_1"
        assert self.reg.resolve_handle("test:1") == "run_1"
        # Should be popped
        assert self.reg.resolve_handle("test:1") is None

    def test_resolve_unknown_handle(self):
        assert self.reg.resolve_handle("unknown:x") is None

    @pytest.mark.asyncio
    async def test_execute_unknown_tool(self):
        with pytest.raises(KeyError):
            await self.reg.execute("nonexistent", {}, "run_1")


class TestGlobalRegistry:

    def test_singleton(self):
        r1 = get_async_tool_registry()
        r2 = get_async_tool_registry()
        assert r1 is r2

    def test_builtin_tools_registered(self):
        reg = get_async_tool_registry()
        assert reg.is_async_tool("submit_job")
        assert reg.is_async_tool("submit_workflow")


class TestResumeWorkflow:

    def setup_method(self):
        _workflow_runs.clear()
        _workflow_waits.clear()

    @pytest.mark.asyncio
    async def test_resume_unknown_handle(self):
        result = await resume_workflow("unknown:handle", {"data": 1})
        assert result is False

    @pytest.mark.asyncio
    async def test_resume_missing_entry(self):
        _workflow_waits["test:1"] = "wf_missing"
        result = await resume_workflow("test:1", {"data": 1})
        assert result is False

    @pytest.mark.asyncio
    async def test_resume_no_wf_run(self):
        _workflow_waits["test:1"] = "wf_1"
        _workflow_runs["wf_1"] = {"workflow": None, "engine": None, "wf_run": None}
        result = await resume_workflow("test:1", {"data": 1})
        assert result is False


class TestCleanupStaleWorkflows:

    @pytest.mark.asyncio
    async def test_cleanup_removes_stale(self):
        """Cleanup should mark stale workflows as failed."""
        from datetime import datetime, timezone, timedelta

        mock_row = MagicMock()
        mock_row.run_id = "wf_stale"
        mock_row.status = "waiting"
        mock_row.waiting_for = "human:review"
        mock_row.created_at = datetime.now(timezone.utc) - timedelta(hours=48)

        # Pre-populate in-memory state
        _workflow_runs["wf_stale"] = {"workflow": None, "engine": None, "wf_run": None}
        _workflow_waits["human:review"] = "wf_stale"

        mock_db = MagicMock()
        mock_query = MagicMock()
        mock_query.filter.return_value = mock_query
        mock_query.all.return_value = [mock_row]
        mock_db.query.return_value = mock_query

        with patch("api.database.get_db_session", return_value=iter([mock_db])):
            count = await cleanup_stale_workflows(max_age_hours=24)

        assert count == 1
        assert mock_row.status == "failed"
        assert "Timed out" in mock_row.error
        assert "wf_stale" not in _workflow_runs
        assert "human:review" not in _workflow_waits

    @pytest.mark.asyncio
    async def test_cleanup_db_error_returns_zero(self):
        with patch("api.database.get_db_session", side_effect=RuntimeError("db down")):
            count = await cleanup_stale_workflows()
        assert count == 0
