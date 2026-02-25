"""Tests for ExecutionBackend, BackendRouter, and AgentExecutor integration."""

import pytest
from unittest.mock import MagicMock

from core.agent.execution_backend import (
    InProcessBackend, SubprocessBackend, BackendRouter,
    ExecutionRequirements, ExecutionResult, ExecutionStatus,
    _is_heavyweight,
)


# ---------------------------------------------------------------------------
# ExecutionRequirements
# ---------------------------------------------------------------------------

class TestExecutionRequirements:

    def test_defaults_are_lightweight(self):
        req = ExecutionRequirements()
        assert _is_heavyweight(req) is False

    def test_gpu_is_heavyweight(self):
        req = ExecutionRequirements(gpu_required=True)
        assert _is_heavyweight(req) is True

    def test_conda_is_heavyweight(self):
        req = ExecutionRequirements(conda_env="train-env")
        assert _is_heavyweight(req) is True

    def test_long_timeout_is_heavyweight(self):
        req = ExecutionRequirements(timeout_seconds=600)
        assert _is_heavyweight(req) is True

    def test_timeout_zero_clamped_to_one(self):
        """Timeout <= 0 should be clamped to 1 to prevent instant timeout."""
        req = ExecutionRequirements(timeout_seconds=0)
        assert req.timeout_seconds == 1

    def test_timeout_negative_clamped_to_one(self):
        req = ExecutionRequirements(timeout_seconds=-10)
        assert req.timeout_seconds == 1

    def test_timeout_boundary_300_is_lightweight(self):
        req = ExecutionRequirements(timeout_seconds=300)
        assert _is_heavyweight(req) is False

    def test_timeout_boundary_301_is_heavyweight(self):
        req = ExecutionRequirements(timeout_seconds=301)
        assert _is_heavyweight(req) is True


# ---------------------------------------------------------------------------
# BackendRouter — single source of truth
# ---------------------------------------------------------------------------

class TestBackendRouter:

    def test_routes_lightweight_to_in_process(self):
        router = BackendRouter()
        assert isinstance(router.select(ExecutionRequirements()), InProcessBackend)

    def test_routes_gpu_to_subprocess(self):
        router = BackendRouter()
        assert isinstance(router.select(ExecutionRequirements(gpu_required=True)), SubprocessBackend)

    def test_routes_conda_to_subprocess(self):
        router = BackendRouter()
        assert isinstance(router.select(ExecutionRequirements(conda_env="ml-env")), SubprocessBackend)

    def test_select_and_is_lightweight_consistent(self):
        """select() and is_lightweight() must always agree (single source of truth)."""
        cases = [
            ExecutionRequirements(),
            ExecutionRequirements(gpu_required=True),
            ExecutionRequirements(conda_env="x"),
            ExecutionRequirements(timeout_seconds=600),
            ExecutionRequirements(timeout_seconds=300),
            ExecutionRequirements(timeout_seconds=301),
        ]
        router = BackendRouter()
        for req in cases:
            is_lw = router.is_lightweight(req)
            selected = router.select(req)
            if is_lw:
                assert isinstance(selected, InProcessBackend), f"Mismatch for {req}"
            else:
                assert isinstance(selected, SubprocessBackend), f"Mismatch for {req}"


# ---------------------------------------------------------------------------
# InProcessBackend
# ---------------------------------------------------------------------------

class TestInProcessBackend:

    @pytest.mark.asyncio
    async def test_execute_sync_success(self):
        backend = InProcessBackend()
        job_id = await backend.submit("test_skill", {}, ExecutionRequirements())

        async def skill_fn(inputs):
            return {"answer": 42}

        result = await backend.execute_sync(job_id, skill_fn, {})
        assert result.status == ExecutionStatus.COMPLETED
        assert result.result == {"answer": 42}

    @pytest.mark.asyncio
    async def test_execute_sync_failure(self):
        backend = InProcessBackend()
        job_id = await backend.submit("bad_skill", {}, ExecutionRequirements())

        async def bad_fn(inputs):
            raise ValueError("boom")

        result = await backend.execute_sync(job_id, bad_fn, {})
        assert result.status == ExecutionStatus.FAILED
        assert "boom" in result.error

    @pytest.mark.asyncio
    async def test_execute_sync_with_sync_function(self):
        backend = InProcessBackend()
        job_id = await backend.submit("sync_skill", {}, ExecutionRequirements())

        def sync_fn(inputs):
            return {"sync": True}

        result = await backend.execute_sync(job_id, sync_fn, {})
        assert result.status == ExecutionStatus.COMPLETED
        assert result.result == {"sync": True}

    @pytest.mark.asyncio
    async def test_gc_purges_completed(self):
        """InProcessBackend should GC completed results when threshold exceeded."""
        backend = InProcessBackend()
        backend._GC_THRESHOLD = 5  # Low threshold for test
        # Fill past threshold
        for i in range(6):
            jid = await backend.submit(f"skill_{i}", {}, ExecutionRequirements())
            backend._results[jid] = ExecutionResult(job_id=jid, status=ExecutionStatus.COMPLETED)
        # Next submit triggers GC
        await backend.submit("trigger", {}, ExecutionRequirements())
        # All completed should be purged, only "trigger" (PENDING) remains
        pending = [r for r in backend._results.values() if r.status == ExecutionStatus.PENDING]
        assert len(pending) == 1

    @pytest.mark.asyncio
    async def test_gc_preserves_running(self):
        """GC should not purge RUNNING entries."""
        backend = InProcessBackend()
        backend._GC_THRESHOLD = 2
        j1 = await backend.submit("s1", {}, ExecutionRequirements())
        backend._results[j1] = ExecutionResult(job_id=j1, status=ExecutionStatus.RUNNING)
        j2 = await backend.submit("s2", {}, ExecutionRequirements())
        backend._results[j2] = ExecutionResult(job_id=j2, status=ExecutionStatus.COMPLETED)
        # Trigger GC
        await backend.submit("s3", {}, ExecutionRequirements())
        assert j1 in backend._results  # RUNNING preserved
        assert j2 not in backend._results  # COMPLETED purged


# ---------------------------------------------------------------------------
# SubprocessBackend
# ---------------------------------------------------------------------------

class TestSubprocessBackend:

    @pytest.mark.asyncio
    async def test_cancel_unknown_job(self):
        backend = SubprocessBackend()
        assert await backend.cancel("nonexistent") is False

    @pytest.mark.asyncio
    async def test_gc_purges_completed(self):
        """SubprocessBackend should GC completed results when threshold exceeded."""
        backend = SubprocessBackend()
        backend._GC_THRESHOLD = 3
        for i in range(4):
            jid = f"job-{i}"
            backend._results[jid] = ExecutionResult(job_id=jid, status=ExecutionStatus.FAILED)
            backend._tasks[jid] = MagicMock()
            backend._procs[jid] = MagicMock()
        # Manually trigger GC
        backend._maybe_gc()
        assert len(backend._results) == 0
        assert len(backend._tasks) == 0
        assert len(backend._procs) == 0


# ---------------------------------------------------------------------------
# AgentExecutor routing
# ---------------------------------------------------------------------------

class TestAgentExecutorRouting:

    def test_get_execution_requirements_default(self):
        from core.agent.executor import AgentExecutor
        from core.skills.base import SkillRequirement
        from core.repos import RepoType, AccessScope
        skill = MagicMock()
        skill.requirements = SkillRequirement(
            repo_types=[RepoType.CODE], min_access=AccessScope.READ,
        )
        req = AgentExecutor._get_execution_requirements(skill)
        assert req.gpu_required is False
        assert req.conda_env is None

    def test_get_execution_requirements_gpu(self):
        from core.agent.executor import AgentExecutor
        from core.skills.base import SkillRequirement
        from core.repos import RepoType, AccessScope
        skill = MagicMock()
        skill.requirements = SkillRequirement(
            repo_types=[RepoType.CODE], min_access=AccessScope.READ,
            gpu_required=True, conda_env="train-env", timeout_seconds=3600,
        )
        req = AgentExecutor._get_execution_requirements(skill)
        assert req.gpu_required is True
        assert req.conda_env == "train-env"

    def test_get_execution_requirements_no_requirements(self):
        from core.agent.executor import AgentExecutor
        skill = MagicMock(spec=[])  # No requirements attr
        req = AgentExecutor._get_execution_requirements(skill)
        assert req.gpu_required is False

    def test_mock_requirements_treated_as_lightweight(self):
        """MagicMock requirements should not route to heavyweight."""
        from core.agent.executor import AgentExecutor
        skill = MagicMock()  # requirements is MagicMock
        req = AgentExecutor._get_execution_requirements(skill)
        router = BackendRouter()
        assert router.is_lightweight(req) is True


# ---------------------------------------------------------------------------
# _execute_heavyweight_sync edge cases
# ---------------------------------------------------------------------------

class TestExecuteHeavyweightSync:

    def _make_executor(self):
        from unittest.mock import patch
        from core.agent.executor import AgentExecutor
        from core.skills.mocking import MockMode
        db = MagicMock()
        registry = MagicMock()
        with patch("core.skills.mocking.ToolMockingLayer.__init__", return_value=None):
            executor = AgentExecutor(db_factory=lambda: db, registry=registry, mode=MockMode.PRODUCTION)
        executor._record_execution_metrics = MagicMock()
        return executor

    def test_empty_stdout_returns_empty_dict(self):
        """If subprocess returns 0 but empty stdout, should return {} not crash."""
        import subprocess
        executor = self._make_executor()
        req = ExecutionRequirements(gpu_required=True)
        with pytest.MonkeyPatch.context() as mp:
            mp.setattr(subprocess, "run", lambda *a, **kw: MagicMock(
                returncode=0, stdout="", stderr=""))
            result = executor._execute_heavyweight_sync("s", {}, req, "sess")
            assert result == {}

    def test_invalid_json_stdout_returns_output_dict(self):
        """If subprocess returns 0 but non-JSON stdout, should not crash."""
        import subprocess
        executor = self._make_executor()
        req = ExecutionRequirements(gpu_required=True)
        with pytest.MonkeyPatch.context() as mp:
            mp.setattr(subprocess, "run", lambda *a, **kw: MagicMock(
                returncode=0, stdout="not json at all", stderr=""))
            result = executor._execute_heavyweight_sync("s", {}, req, "sess")
            assert result == {"output": "not json at all"}

    def test_nonzero_exit_raises_with_stderr(self):
        """Non-zero exit should raise RuntimeError with stderr content."""
        import subprocess
        executor = self._make_executor()
        req = ExecutionRequirements(gpu_required=True)
        with pytest.MonkeyPatch.context() as mp:
            mp.setattr(subprocess, "run", lambda *a, **kw: MagicMock(
                returncode=1, stdout="", stderr="some error"))
            with pytest.raises(RuntimeError, match="some error"):
                executor._execute_heavyweight_sync("s", {}, req, "sess")

    def test_timeout_raises_runtime_error(self):
        """TimeoutExpired should be caught and re-raised as RuntimeError."""
        import subprocess
        executor = self._make_executor()
        req = ExecutionRequirements(gpu_required=True, timeout_seconds=1)
        with pytest.MonkeyPatch.context() as mp:
            def raise_timeout(*a, **kw):
                raise subprocess.TimeoutExpired(cmd="x", timeout=1)
            mp.setattr(subprocess, "run", raise_timeout)
            with pytest.raises(RuntimeError, match="timed out"):
                executor._execute_heavyweight_sync("s", {}, req, "sess")


# ---------------------------------------------------------------------------
# SkillRequirement backward compat
# ---------------------------------------------------------------------------

class TestSkillRequirementExtension:

    def test_new_fields_have_defaults(self):
        from core.skills.base import SkillRequirement
        from core.repos import RepoType, AccessScope
        req = SkillRequirement(repo_types=[RepoType.CODE], min_access=AccessScope.READ)
        assert req.gpu_required is False
        assert req.conda_env is None
        assert req.timeout_seconds == 60
        assert req.async_execution is False

    def test_heavyweight_skill_requirement(self):
        from core.skills.base import SkillRequirement
        from core.repos import RepoType, AccessScope
        req = SkillRequirement(
            repo_types=[RepoType.CODE], min_access=AccessScope.READ,
            gpu_required=True, conda_env="train-env", timeout_seconds=3600,
        )
        assert req.gpu_required is True
        assert req.conda_env == "train-env"
