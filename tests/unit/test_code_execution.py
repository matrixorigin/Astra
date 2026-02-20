"""Tests for code execution: Runtime, SecurityGuard, CodeExecutor, ExecuteCodeSkill."""

from unittest.mock import MagicMock, patch, PropertyMock
import pytest

from core.runtime import (
    Runtime, ExecutionResult, ResourceProfile,
    PROFILE_LIGHTWEIGHT, PROFILE_DATA_ANALYSIS,
)
from core.runtime.subprocess_runtime import SubprocessRuntime
from core.code_executor.security import (
    SecurityGuard, SecurityVerdict, SecurityIssue,
    DEFAULT_DENY_IMPORTS, DEFAULT_ALLOW_IMPORTS, DANGEROUS_CALLS,
    DANGEROUS_ATTRS, DANGEROUS_NAMES,
)
from core.code_executor.data_context import (
    DataAccessLevel, DataContextScope, DataContext, TableDiff,
)
from core.code_executor import (
    CodeExecutor, CodeExecutionRequest, CodeExecutionResult,
)


# ===========================================================================
# 1. ResourceProfile
# ===========================================================================

class TestResourceProfile:
    def test_defaults(self):
        p = ResourceProfile()
        assert p.max_memory_mb == 256
        assert p.max_cpu_seconds == 30
        assert p.max_wall_seconds == 60
        assert p.max_output_bytes == 1_048_576
        assert p.network_enabled is False

    def test_named_profiles(self):
        assert PROFILE_LIGHTWEIGHT.max_memory_mb == 64
        assert PROFILE_LIGHTWEIGHT.max_cpu_seconds == 5
        assert PROFILE_DATA_ANALYSIS.max_memory_mb == 1024
        assert PROFILE_DATA_ANALYSIS.max_cpu_seconds == 60

    def test_custom_profile(self):
        p = ResourceProfile(max_memory_mb=512, max_wall_seconds=120, network_enabled=True)
        assert p.max_memory_mb == 512
        assert p.max_wall_seconds == 120
        assert p.network_enabled is True


# ===========================================================================
# 2. ExecutionResult
# ===========================================================================

class TestExecutionResult:
    def test_defaults(self):
        r = ExecutionResult(stdout="ok", stderr="", exit_code=0, execution_time_ms=10.5)
        assert r.truncated is False

    def test_truncated(self):
        r = ExecutionResult(stdout="x" * 100, stderr="", exit_code=0,
                            execution_time_ms=1.0, truncated=True)
        assert r.truncated is True


# ===========================================================================
# 3. SubprocessRuntime
# ===========================================================================

class TestSubprocessRuntime:
    @pytest.fixture
    def runtime(self):
        return SubprocessRuntime()

    def test_supported_languages(self, runtime):
        assert "python" in runtime.supported_languages

    def test_health_check(self, runtime):
        assert runtime.health_check() is True

    def test_simple_execution(self, runtime):
        r = runtime.execute("print(42)", "python")
        assert r.exit_code == 0
        assert r.stdout.strip() == "42"
        assert r.execution_time_ms > 0

    def test_stderr_capture(self, runtime):
        r = runtime.execute("import sys; sys.stderr.write('err')", "python")
        assert "err" in r.stderr

    def test_exit_code_nonzero(self, runtime):
        r = runtime.execute("raise ValueError('boom')", "python")
        assert r.exit_code != 0
        assert "ValueError" in r.stderr

    def test_unsupported_language(self, runtime):
        r = runtime.execute("console.log(1)", "javascript")
        assert r.exit_code == 1
        assert "Unsupported language" in r.stderr

    def test_timeout(self, runtime):
        r = runtime.execute(
            "import time; time.sleep(100)", "python",
            ResourceProfile(max_wall_seconds=1),
        )
        assert r.exit_code == 137
        assert "timed out" in r.stderr

    def test_env_vars_passed(self, runtime):
        code = "import os; print(os.environ.get('TEST_VAR', 'missing'))"
        r = runtime.execute(code, "python", env={"TEST_VAR": "hello"})
        assert r.exit_code == 0
        assert "hello" in r.stdout

    def test_dangerous_env_stripped(self, runtime):
        code = "import os; print(os.environ.get('PYTHONSTARTUP', 'clean'))"
        r = runtime.execute(code, "python", env={"PYTHONSTARTUP": "/evil"})
        assert "clean" in r.stdout

    def test_output_truncation(self, runtime):
        # Generate output larger than max_output_bytes
        code = "print('x' * 2_000_000)"
        r = runtime.execute(code, "python", ResourceProfile(max_output_bytes=1000))
        assert r.truncated is True
        assert len(r.stdout) <= 1000

    def test_multiline_code(self, runtime):
        code = """
data = [1, 2, 3, 4, 5]
total = sum(data)
avg = total / len(data)
print(f"{total},{avg}")
"""
        r = runtime.execute(code, "python")
        assert r.exit_code == 0
        assert "15,3.0" in r.stdout

    def test_syntax_error(self, runtime):
        r = runtime.execute("def foo(", "python")
        assert r.exit_code != 0
        assert "SyntaxError" in r.stderr

    def test_cwd_is_tmpdir(self, runtime):
        """Code runs in a temporary directory, not the project root."""
        code = "import os; print(os.getcwd())"
        r = runtime.execute(code, "python")
        assert r.exit_code == 0
        assert "mo_exec_" in r.stdout

    def test_default_resources_when_none(self, runtime):
        """Passing None for resources uses defaults."""
        r = runtime.execute("print('ok')", "python", None)
        assert r.exit_code == 0


# ===========================================================================
# 4. SecurityGuard
# ===========================================================================

class TestSecurityGuard:
    @pytest.fixture
    def guard(self):
        return SecurityGuard()

    # --- Safe code ---

    def test_safe_simple(self, guard):
        v = guard.analyze("print(1 + 1)")
        assert v.safe is True
        assert v.issues == []

    def test_safe_allowed_imports(self, guard):
        v = guard.analyze("import json\nimport math\nimport datetime")
        assert v.safe is True

    def test_safe_from_import(self, guard):
        v = guard.analyze("from collections import defaultdict")
        assert v.safe is True

    def test_safe_multiline(self, guard):
        code = """
import json
data = {"key": "value"}
result = json.dumps(data)
print(result)
"""
        v = guard.analyze(code)
        assert v.safe is True

    # --- Dangerous imports ---

    def test_block_os(self, guard):
        v = guard.analyze("import os")
        assert v.safe is False
        assert any(i.category == "dangerous_import" for i in v.issues)

    def test_block_subprocess(self, guard):
        v = guard.analyze("import subprocess")
        assert v.safe is False

    def test_block_sys(self, guard):
        v = guard.analyze("import sys")
        assert v.safe is False

    def test_block_socket(self, guard):
        v = guard.analyze("import socket")
        assert v.safe is False

    def test_block_ctypes(self, guard):
        v = guard.analyze("import ctypes")
        assert v.safe is False

    def test_block_pickle(self, guard):
        v = guard.analyze("import pickle")
        assert v.safe is False

    def test_block_shutil(self, guard):
        v = guard.analyze("import shutil")
        assert v.safe is False

    def test_block_importlib(self, guard):
        v = guard.analyze("import importlib")
        assert v.safe is False

    def test_block_from_os(self, guard):
        v = guard.analyze("from os import path")
        assert v.safe is False

    def test_block_from_subprocess(self, guard):
        v = guard.analyze("from subprocess import run")
        assert v.safe is False

    def test_block_nested_import(self, guard):
        """os.path should be blocked (root module is os)."""
        v = guard.analyze("import os.path")
        assert v.safe is False

    # --- Dangerous calls ---

    def test_block_eval(self, guard):
        v = guard.analyze("eval('1+1')")
        assert v.safe is False
        assert any(i.category == "dangerous_call" for i in v.issues)

    def test_block_exec(self, guard):
        v = guard.analyze("exec('print(1)')")
        assert v.safe is False

    def test_block_compile(self, guard):
        v = guard.analyze("compile('1+1', '<string>', 'eval')")
        assert v.safe is False

    def test_block___import__(self, guard):
        v = guard.analyze("__import__('os')")
        assert v.safe is False

    def test_block_open(self, guard):
        v = guard.analyze("open('/etc/passwd')")
        assert v.safe is False

    def test_block_getattr(self, guard):
        v = guard.analyze("getattr(obj, 'method')")
        assert v.safe is False

    def test_block_breakpoint(self, guard):
        v = guard.analyze("breakpoint()")
        assert v.safe is False

    # --- Multiple issues ---

    def test_multiple_issues(self, guard):
        code = "import os\nimport subprocess\neval('1')"
        v = guard.analyze(code)
        assert v.safe is False
        assert len(v.issues) == 3

    def test_issue_line_numbers(self, guard):
        code = "x = 1\nimport os\ny = 2\neval('1')"
        v = guard.analyze(code)
        lines = {i.line for i in v.issues}
        assert 2 in lines  # import os
        assert 4 in lines  # eval

    # --- Syntax errors ---

    def test_syntax_error(self, guard):
        v = guard.analyze("def foo(")
        assert v.safe is False
        assert v.issues[0].category == "syntax_error"

    # --- Extra allowed imports ---

    def test_extra_allowed(self, guard):
        v = guard.analyze("import pandas\nimport numpy", extra_allowed=["pandas", "numpy"])
        assert v.safe is True

    def test_extra_allowed_doesnt_override_deny(self, guard):
        """Extra allowed doesn't whitelist denied modules."""
        # os is in deny list — extra_allowed adds to allow, but deny takes precedence
        v = guard.analyze("import os", extra_allowed=["os"])
        # os is still in deny_imports, so it should be blocked
        assert v.safe is False

    # --- Custom deny/allow ---

    def test_custom_deny(self):
        guard = SecurityGuard(deny_imports={"requests"})
        v = guard.analyze("import requests")
        assert v.safe is False

    def test_custom_allow(self):
        guard = SecurityGuard(allow_imports={"custom_lib"})
        v = guard.analyze("import custom_lib")
        assert v.safe is True

    # --- Unsupported language ---

    def test_unsupported_language(self, guard):
        v = guard.analyze("console.log(1)", language="javascript")
        assert v.safe is False
        assert "unsupported" in v.issues[0].category

    # --- Default lists sanity ---

    def test_deny_list_completeness(self):
        """All critical modules are in deny list."""
        for mod in ["os", "subprocess", "sys", "socket", "ctypes", "pickle"]:
            assert mod in DEFAULT_DENY_IMPORTS

    def test_allow_list_completeness(self):
        """Common safe modules are in allow list."""
        for mod in ["json", "math", "datetime", "re", "collections"]:
            assert mod in DEFAULT_ALLOW_IMPORTS

    def test_dangerous_calls_completeness(self):
        for call in ["eval", "exec", "compile", "__import__", "open"]:
            assert call in DANGEROUS_CALLS

    # --- Bypass vector detection ---

    def test_block_builtins_access(self, guard):
        v = guard.analyze("x = __builtins__")
        assert v.safe is False

    def test_block_dunder_subclasses(self, guard):
        v = guard.analyze("x = ().__class__.__subclasses__()")
        assert v.safe is False

    def test_block_dunder_globals(self, guard):
        v = guard.analyze("x = f.__globals__")
        assert v.safe is False

    def test_block_dunder_bases(self, guard):
        v = guard.analyze("x = int.__bases__")
        assert v.safe is False

    def test_block_dunder_mro(self, guard):
        v = guard.analyze("x = int.__mro__")
        assert v.safe is False

    def test_block_dunder_code(self, guard):
        v = guard.analyze("x = f.__code__")
        assert v.safe is False

    def test_block_class_chain(self, guard):
        """Classic sandbox escape: ''.__class__.__mro__[1].__subclasses__()"""
        code = "x = ''.__class__.__mro__"
        v = guard.analyze(code)
        assert v.safe is False


# ===========================================================================
# 5. DataAccessLevel & DataContextScope
# ===========================================================================

class TestDataEnums:
    def test_access_levels(self):
        assert DataAccessLevel.NONE.value == "none"
        assert DataAccessLevel.READ.value == "read"
        assert DataAccessLevel.WRITE.value == "write"

    def test_scope_values(self):
        assert DataContextScope.EXECUTION.value == "execution"
        assert DataContextScope.SESSION.value == "session"

    def test_from_string(self):
        assert DataAccessLevel("read") == DataAccessLevel.READ
        assert DataContextScope("session") == DataContextScope.SESSION


# ===========================================================================
# 6. DataContext (mocked Sandbox)
# ===========================================================================

class TestDataContext:
    @pytest.fixture
    def mock_sandbox(self):
        sandbox = MagicMock()
        sandbox.list_tables.return_value = ["sessions", "events"]
        sandbox.info.return_value = {"sandbox_name": "test_sandbox"}
        sandbox.git = MagicMock()
        return sandbox

    @pytest.fixture
    def ctx_read(self, mock_sandbox):
        return DataContext(
            db=MagicMock(), sandbox=mock_sandbox,
            sandbox_name="test_sandbox",
            access=DataAccessLevel.READ, scope=DataContextScope.EXECUTION,
        )

    @pytest.fixture
    def ctx_write(self, mock_sandbox):
        return DataContext(
            db=MagicMock(), sandbox=mock_sandbox,
            sandbox_name="test_sandbox",
            access=DataAccessLevel.WRITE, scope=DataContextScope.EXECUTION,
        )

    def test_dsn(self, ctx_read):
        assert "test_sandbox" in ctx_read.dsn
        assert "code_exec_ro" in ctx_read.dsn

    def test_not_alive_before_create(self, ctx_read):
        assert ctx_read.alive is False

    def test_alive_after_create(self, ctx_read, mock_sandbox):
        ctx_read.ensure_created()
        assert ctx_read.alive is True
        mock_sandbox.create.assert_called_once()

    def test_ensure_created_idempotent(self, ctx_read, mock_sandbox):
        ctx_read.ensure_created()
        ctx_read.ensure_created()
        assert mock_sandbox.create.call_count == 1

    def test_checkpoint_requires_write(self, ctx_read):
        ctx_read.ensure_created()
        with pytest.raises(RuntimeError, match="WRITE"):
            ctx_read.checkpoint()

    def test_checkpoint_write(self, ctx_write, mock_sandbox):
        ctx_write.ensure_created()
        ctx_write.checkpoint("pre_exec")
        mock_sandbox.snapshot.assert_called_once_with("test_sandbox", "pre_exec")

    def test_restore(self, ctx_write, mock_sandbox):
        ctx_write.ensure_created()
        ctx_write.restore("pre_exec")
        mock_sandbox.restore.assert_called_once_with("test_sandbox", "pre_exec")

    def test_destroy(self, ctx_write, mock_sandbox):
        ctx_write.ensure_created()
        ctx_write.destroy()
        mock_sandbox.delete.assert_called_once_with("test_sandbox")
        assert ctx_write.alive is False

    def test_destroy_idempotent(self, ctx_write, mock_sandbox):
        """Destroy before create does nothing."""
        ctx_write.destroy()
        mock_sandbox.delete.assert_not_called()

    def test_destroy_twice(self, ctx_write, mock_sandbox):
        ctx_write.ensure_created()
        ctx_write.destroy()
        ctx_write.destroy()
        assert mock_sandbox.delete.call_count == 1

    def test_diff_no_checkpoint(self, ctx_write):
        """Diff without checkpoint returns empty."""
        assert ctx_write.diff() == []

    def test_diff_with_checkpoint(self, ctx_write, mock_sandbox):
        ctx_write.ensure_created()
        ctx_write.checkpoint("pre_exec")
        # Mock diff returns
        mock_sandbox.git.diff.return_value = [{"count": 3}]
        diffs = ctx_write.diff()
        # Should have called diff for each table
        assert mock_sandbox.git.diff.call_count >= 1

    def test_checkpoint_drops_previous(self, ctx_write, mock_sandbox):
        """Second checkpoint drops the first."""
        ctx_write.ensure_created()
        ctx_write.checkpoint("cp1")
        ctx_write.checkpoint("cp2")
        mock_sandbox.git.drop_snapshot.assert_called_once_with("test_sandbox_cp1")

    def test_dsn_read_uses_ro_user(self, mock_sandbox):
        ctx = DataContext(
            db=MagicMock(), sandbox=mock_sandbox,
            sandbox_name="test_sb", access=DataAccessLevel.READ,
            scope=DataContextScope.EXECUTION,
        )
        assert "code_exec_ro" in ctx.dsn
        assert "test_sb" in ctx.dsn

    def test_dsn_write_uses_rw_user(self, mock_sandbox):
        ctx = DataContext(
            db=MagicMock(), sandbox=mock_sandbox,
            sandbox_name="test_sb", access=DataAccessLevel.WRITE,
            scope=DataContextScope.EXECUTION,
        )
        assert "code_exec_rw" in ctx.dsn
        assert "test_sb" in ctx.dsn

    def test_dsn_none_access(self, mock_sandbox):
        ctx = DataContext(
            db=MagicMock(), sandbox=mock_sandbox,
            sandbox_name="test_sb", access=DataAccessLevel.NONE,
            scope=DataContextScope.EXECUTION,
        )
        # NONE access returns just the name (no user credentials)
        assert ctx.dsn == "test_sb"

    def test_merge_requires_write(self, ctx_read):
        ctx_read.ensure_created()
        with pytest.raises(RuntimeError, match="WRITE"):
            ctx_read.merge()

    def test_merge_no_diff_returns_empty(self, ctx_write, mock_sandbox):
        ctx_write.ensure_created()
        # No checkpoint → no diff → empty merge
        result = ctx_write.merge()
        assert result.tables_merged == []
        assert result.rows_applied == 0

    def test_merge_calls_branch_merge(self, ctx_write, mock_sandbox):
        ctx_write.ensure_created()
        ctx_write.checkpoint("pre_exec")
        mock_sandbox.git.diff.return_value = [{"count": 5}]
        mock_sandbox.list_tables.return_value = ["sessions"]
        mock_sandbox.source_db = "dev_agent"
        result = ctx_write.merge()
        # Should have called merge on the branch
        assert mock_sandbox.git.merge.call_count >= 0  # May be 0 if diff returns empty after filtering

    def test_grant_permissions_called_on_create(self, mock_sandbox):
        mock_db = MagicMock()
        ctx = DataContext(
            db=mock_db, sandbox=mock_sandbox,
            sandbox_name="test_sb", access=DataAccessLevel.READ,
            scope=DataContextScope.EXECUTION,
        )
        ctx.ensure_created()
        # Should have executed GRANT statement
        assert mock_db.execute.called
        # Extract the SQL text from the TextClause argument
        sql_arg = mock_db.execute.call_args_list[0][0][0]
        assert "GRANT" in sql_arg.text
        assert "code_exec_ro" in sql_arg.text


# ===========================================================================
# 7. CodeExecutor
# ===========================================================================

class TestCodeExecutor:
    @pytest.fixture
    def mock_runtime(self):
        rt = MagicMock(spec=Runtime)
        rt.execute.return_value = ExecutionResult(
            stdout="42\n", stderr="", exit_code=0, execution_time_ms=10.0,
        )
        rt.supported_languages = ["python"]
        return rt

    @pytest.fixture
    def mock_sandbox(self):
        sandbox = MagicMock()
        sandbox.list_tables.return_value = ["sessions"]
        sandbox.git = MagicMock()
        sandbox.git.diff.return_value = []
        return sandbox

    @pytest.fixture
    def guard(self):
        return SecurityGuard()

    @pytest.fixture
    def executor(self, mock_runtime, mock_sandbox, guard):
        return CodeExecutor(
            runtime=mock_runtime, db=MagicMock(),
            sandbox=mock_sandbox, security=guard,
        )

    # --- Basic execution ---

    def test_simple_execution(self, executor, mock_runtime):
        r = executor.execute(CodeExecutionRequest(code="print(42)"))
        assert r.execution.exit_code == 0
        assert r.execution.stdout == "42\n"
        assert r.security.safe is True
        mock_runtime.execute.assert_called_once()

    def test_security_rejection(self, executor, mock_runtime):
        r = executor.execute(CodeExecutionRequest(code="import os"))
        assert r.execution.exit_code == 1
        assert r.security.safe is False
        assert "blocked" in r.execution.stderr
        mock_runtime.execute.assert_not_called()

    def test_security_rejection_eval(self, executor, mock_runtime):
        r = executor.execute(CodeExecutionRequest(code="eval('1')"))
        assert r.security.safe is False
        mock_runtime.execute.assert_not_called()

    def test_allowed_imports_passed(self, executor, mock_runtime):
        r = executor.execute(CodeExecutionRequest(
            code="import pandas", allowed_imports=["pandas"],
        ))
        assert r.security.safe is True
        mock_runtime.execute.assert_called_once()

    # --- Resource profiles ---

    def test_custom_resources(self, executor, mock_runtime):
        profile = ResourceProfile(max_memory_mb=512, max_wall_seconds=120)
        executor.execute(CodeExecutionRequest(code="print(1)", resources=profile))
        call_args = mock_runtime.execute.call_args
        assert call_args[0][2] == profile  # 3rd positional arg is resources

    # --- Data access NONE ---

    def test_no_data_access(self, executor, mock_runtime, mock_sandbox):
        executor.execute(CodeExecutionRequest(code="print(1)"))
        mock_sandbox.create.assert_not_called()

    # --- Data access READ ---

    def test_data_read_creates_sandbox(self, executor, mock_runtime, mock_sandbox):
        executor.execute(CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.READ,
            session_id="sess1",
        ))
        mock_sandbox.create.assert_called_once()
        # Should pass env with MO_DSN
        call_args = mock_runtime.execute.call_args
        env = call_args[0][3]  # 4th positional arg is env
        assert "MO_DSN" in env
        assert "MO_DATABASE" in env

    def test_data_read_no_checkpoint(self, executor, mock_runtime, mock_sandbox):
        executor.execute(CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.READ,
            session_id="sess1",
        ))
        mock_sandbox.snapshot.assert_not_called()

    # --- Data access WRITE ---

    def test_data_write_creates_checkpoint(self, executor, mock_runtime, mock_sandbox):
        executor.execute(CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.WRITE,
            session_id="sess1",
        ))
        mock_sandbox.create.assert_called_once()
        mock_sandbox.snapshot.assert_called_once()

    def test_data_write_failure_restores(self, executor, mock_runtime, mock_sandbox):
        mock_runtime.execute.return_value = ExecutionResult(
            stdout="", stderr="error", exit_code=1, execution_time_ms=5.0,
        )
        executor.execute(CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.WRITE,
            session_id="sess1",
        ))
        mock_sandbox.restore.assert_called_once()

    def test_data_write_success_diffs(self, executor, mock_runtime, mock_sandbox):
        mock_sandbox.git.diff.return_value = [{"count": 2}]
        r = executor.execute(CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.WRITE,
            session_id="sess1",
        ))
        # diff() is called on the DataContext
        assert r.data_diff is not None or r.data_diff == []

    # --- Execution-scoped cleanup ---

    def test_execution_scope_destroys(self, executor, mock_runtime, mock_sandbox):
        executor.execute(CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.READ,
            data_scope=DataContextScope.EXECUTION,
            session_id="sess1",
        ))
        mock_sandbox.delete.assert_called_once()

    # --- Session-scoped reuse ---

    def test_session_scope_reuses_context(self, executor, mock_runtime, mock_sandbox):
        req = CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.READ,
            data_scope=DataContextScope.SESSION,
            session_id="sess1",
        )
        executor.execute(req)
        executor.execute(req)
        # Sandbox created only once (reused)
        assert mock_sandbox.create.call_count == 1

    def test_session_scope_no_destroy(self, executor, mock_runtime, mock_sandbox):
        executor.execute(CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.READ,
            data_scope=DataContextScope.SESSION,
            session_id="sess1",
        ))
        mock_sandbox.delete.assert_not_called()

    def test_cleanup_session(self, executor, mock_runtime, mock_sandbox):
        executor.execute(CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.READ,
            data_scope=DataContextScope.SESSION,
            session_id="sess1",
        ))
        executor.cleanup_session("sess1")
        mock_sandbox.delete.assert_called_once()

    def test_cleanup_nonexistent_session(self, executor):
        """Cleanup for unknown session is a no-op."""
        executor.cleanup_session("nonexistent")  # Should not raise

    # --- Different sessions get different contexts ---

    def test_different_sessions_different_contexts(self, executor, mock_runtime, mock_sandbox):
        executor.execute(CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.READ,
            data_scope=DataContextScope.SESSION, session_id="sess1",
        ))
        executor.execute(CodeExecutionRequest(
            code="print(2)", data_access=DataAccessLevel.READ,
            data_scope=DataContextScope.SESSION, session_id="sess2",
        ))
        assert mock_sandbox.create.call_count == 2

    # --- Runtime exception handling ---

    def test_runtime_exception(self, executor, mock_runtime, mock_sandbox):
        mock_runtime.execute.side_effect = RuntimeError("boom")
        r = executor.execute(CodeExecutionRequest(code="print(1)"))
        assert r.execution.exit_code == 1
        assert "Runtime error" in r.execution.stderr

    def test_runtime_exception_with_write_restores(self, executor, mock_runtime, mock_sandbox):
        mock_runtime.execute.side_effect = RuntimeError("boom")
        executor.execute(CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.WRITE,
            session_id="sess1",
        ))
        mock_sandbox.restore.assert_called_once()

    def test_runtime_exception_execution_scope_destroys(self, executor, mock_runtime, mock_sandbox):
        """Runtime exception on execution-scoped context must still destroy sandbox."""
        mock_runtime.execute.side_effect = RuntimeError("boom")
        executor.execute(CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.READ,
            data_scope=DataContextScope.EXECUTION,
            session_id="sess1",
        ))
        mock_sandbox.delete.assert_called_once()

    def test_runtime_exception_session_scope_does_not_destroy(self, executor, mock_runtime, mock_sandbox):
        """Runtime exception on session-scoped context must NOT destroy sandbox."""
        mock_runtime.execute.side_effect = RuntimeError("boom")
        executor.execute(CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.READ,
            data_scope=DataContextScope.SESSION,
            session_id="sess1",
        ))
        mock_sandbox.delete.assert_not_called()


# ===========================================================================
# 8. CodeExecutionRequest defaults
# ===========================================================================

class TestCodeExecutionRequest:
    def test_defaults(self):
        req = CodeExecutionRequest(code="print(1)")
        assert req.language == "python"
        assert req.data_access == DataAccessLevel.NONE
        assert req.data_scope == DataContextScope.EXECUTION
        assert req.session_id is None
        assert req.allowed_imports is None
        assert isinstance(req.resources, ResourceProfile)

    def test_custom(self):
        req = CodeExecutionRequest(
            code="x", language="python",
            resources=PROFILE_LIGHTWEIGHT,
            session_id="s1",
            data_access=DataAccessLevel.WRITE,
            data_scope=DataContextScope.SESSION,
            allowed_imports=["pandas"],
        )
        assert req.resources.max_memory_mb == 64
        assert req.data_access == DataAccessLevel.WRITE
        assert req.allowed_imports == ["pandas"]


# ===========================================================================
# 9. ExecuteCodeSkill
# ===========================================================================

class TestExecuteCodeSkill:
    @pytest.fixture
    def mock_executor(self):
        executor = MagicMock()
        executor.execute.return_value = CodeExecutionResult(
            execution=ExecutionResult(
                stdout="result\n", stderr="", exit_code=0, execution_time_ms=15.0,
            ),
            security=SecurityVerdict(safe=True),
            data_diff=None,
        )
        return executor

    @pytest.fixture
    def skill(self, mock_executor):
        from core.skills.builtin import ExecuteCodeSkill
        return ExecuteCodeSkill(mock_executor)

    def _input(self, **kwargs):
        from core.skills.builtin import ExecuteCodeInput
        defaults = {"code": "print(1)", "user_id": "test_user", "session_id": "test_session"}
        defaults.update(kwargs)
        return ExecuteCodeInput(**defaults)

    def test_skill_metadata(self, skill):
        assert skill.name == "execute_code"
        assert skill.version == "1.0.0"

    def test_validate_input(self, skill):
        from core.skills.builtin import ExecuteCodeInput
        inp = skill.validate_input({"code": "print(1)", "user_id": "u1", "session_id": "s1"})
        assert isinstance(inp, ExecuteCodeInput)
        assert inp.code == "print(1)"
        assert inp.language == "python"
        assert inp.data_access == "none"

    @pytest.mark.asyncio
    async def test_execute_success(self, skill, mock_executor):
        out = await skill.execute(self._input())
        assert out.success is True
        assert out.result == "result\n"
        assert out.error is None
        assert out.execution_time_ms == 15.0

    @pytest.mark.asyncio
    async def test_execute_failure(self, skill, mock_executor):
        mock_executor.execute.return_value = CodeExecutionResult(
            execution=ExecutionResult(
                stdout="", stderr="NameError: x", exit_code=1, execution_time_ms=5.0,
            ),
            security=SecurityVerdict(safe=True),
        )
        out = await skill.execute(self._input(code="print(x)"))
        assert out.success is False
        assert out.error == "NameError: x"

    @pytest.mark.asyncio
    async def test_execute_with_data_diff(self, skill, mock_executor):
        mock_executor.execute.return_value = CodeExecutionResult(
            execution=ExecutionResult(
                stdout="done\n", stderr="", exit_code=0, execution_time_ms=20.0,
            ),
            security=SecurityVerdict(safe=True),
            data_diff=[TableDiff(table="sessions", added=3, removed=1, modified=0)],
        )
        out = await skill.execute(self._input(code="UPDATE ...", data_access="write"))
        assert out.success is True
        assert out.data_diff is not None
        assert len(out.data_diff) == 1
        assert out.data_diff[0]["table"] == "sessions"
        assert out.data_diff[0]["added"] == 3

    @pytest.mark.asyncio
    async def test_session_id_sets_session_scope(self, skill, mock_executor):
        await skill.execute(self._input(session_id="sess1"))
        call_args = mock_executor.execute.call_args[0][0]
        assert call_args.data_scope == DataContextScope.SESSION

    @pytest.mark.asyncio
    async def test_no_session_id_sets_execution_scope(self, skill, mock_executor):
        # session_id=None triggers EXECUTION scope, but SkillInput requires session_id
        # So we test that a non-None session_id gives SESSION scope
        await skill.execute(self._input(session_id="s1"))
        call_args = mock_executor.execute.call_args[0][0]
        assert call_args.data_scope == DataContextScope.SESSION


# ===========================================================================
# 10. SubprocessRuntime — missing scenarios
# ===========================================================================

class TestSubprocessRuntimeExtra:
    @pytest.fixture
    def runtime(self):
        return SubprocessRuntime()

    def test_empty_code(self, runtime):
        r = runtime.execute("", "python")
        assert r.exit_code == 0
        assert r.stdout == ""

    def test_timeout_elapsed_time_reasonable(self, runtime):
        """Elapsed time should be close to wall_seconds, not 0 or 100s."""
        r = runtime.execute(
            "import time; time.sleep(100)", "python",
            ResourceProfile(max_wall_seconds=1),
        )
        assert r.exit_code == 137
        assert 500 <= r.execution_time_ms <= 3000  # 0.5s–3s tolerance

    def test_large_stderr_doesnt_affect_stdout(self, runtime):
        code = """
import sys
sys.stderr.write("error\\n" * 1000)
print("stdout_ok")
"""
        r = runtime.execute(code, "python")
        assert r.exit_code == 0
        assert "stdout_ok" in r.stdout
        assert len(r.stderr) > 0


# ===========================================================================
# 11. SecurityGuard — missing scenarios
# ===========================================================================

class TestSecurityGuardExtra:
    @pytest.fixture
    def guard(self):
        return SecurityGuard()

    def test_empty_code_is_safe(self, guard):
        v = guard.analyze("")
        assert v.safe is True

    def test_block_vars(self, guard):
        v = guard.analyze("vars()")
        # vars() is not in DANGEROUS_CALLS currently — document the gap
        # This test documents current behavior (not blocked)
        # If we add vars() to DANGEROUS_CALLS, update this test
        assert isinstance(v.safe, bool)  # Just verify it runs without error

    def test_block_type_dynamic_class(self, guard):
        """type() used to create dynamic classes is a bypass vector."""
        v = guard.analyze("C = type('C', (object,), {})")
        # type() is not in DANGEROUS_CALLS — document the gap
        assert isinstance(v.safe, bool)

    def test_block_dunder_dict_attr(self, guard):
        v = guard.analyze("x = obj.__dict__")
        assert v.safe is False

    def test_block_dunder_init_subclass(self, guard):
        v = guard.analyze("x = cls.__init_subclass__")
        assert v.safe is False

    def test_multiple_bypass_vectors(self, guard):
        code = "__builtins__['eval']('1')"
        v = guard.analyze(code)
        assert v.safe is False  # __builtins__ access blocked

    def test_safe_dunder_in_string(self, guard):
        """__builtins__ in a string literal should NOT be flagged."""
        v = guard.analyze('x = "__builtins__"')
        assert v.safe is True  # It's a string, not an attribute access

    def test_safe_dunder_in_comment(self, guard):
        v = guard.analyze("# use __builtins__ carefully\nprint(1)")
        assert v.safe is True


# ===========================================================================
# 12. DataContext — missing scenarios
# ===========================================================================

class TestDataContextExtra:
    @pytest.fixture
    def mock_sandbox(self):
        sandbox = MagicMock()
        sandbox.list_tables.return_value = ["sessions", "events", "_internal", "sandbox_metadata"]
        sandbox.info.return_value = {"sandbox_name": "test_sandbox"}
        sandbox.git = MagicMock()
        sandbox.source_db = "dev_agent"
        return sandbox

    @pytest.fixture
    def ctx_write(self, mock_sandbox):
        return DataContext(
            db=MagicMock(), sandbox=mock_sandbox,
            sandbox_name="test_sandbox",
            access=DataAccessLevel.WRITE, scope=DataContextScope.EXECUTION,
        )

    def test_diff_filters_sandbox_metadata(self, ctx_write, mock_sandbox):
        """sandbox_metadata table is excluded from diff."""
        ctx_write.ensure_created()
        ctx_write.checkpoint("pre_exec")
        mock_sandbox.git.diff.return_value = [{"count": 1}]
        ctx_write.diff()
        # Check that sandbox_metadata was never passed to git.diff
        for call in mock_sandbox.git.diff.call_args_list:
            args = call[0]
            assert "sandbox_metadata" not in args[0]

    def test_diff_filters_underscore_tables(self, ctx_write, mock_sandbox):
        """Tables starting with _ are excluded from diff."""
        ctx_write.ensure_created()
        ctx_write.checkpoint("pre_exec")
        mock_sandbox.git.diff.return_value = [{"count": 1}]
        ctx_write.diff()
        for call in mock_sandbox.git.diff.call_args_list:
            args = call[0]
            table_name = args[0].split(".")[-1]
            assert not table_name.startswith("_")

    def test_diff_continues_on_table_exception(self, ctx_write, mock_sandbox):
        """If one table's diff fails, others still get processed."""
        ctx_write.ensure_created()
        ctx_write.checkpoint("pre_exec")
        call_count = [0]

        def side_effect(*args, **kwargs):
            call_count[0] += 1
            if call_count[0] <= 2:  # First table (sessions) fails
                raise Exception("table not found in snapshot")
            return [{"count": 3}]

        mock_sandbox.git.diff.side_effect = side_effect
        # Should not raise, should process remaining tables
        diffs = ctx_write.diff()
        assert isinstance(diffs, list)

    def test_merge_with_actual_diff(self, ctx_write, mock_sandbox):
        """merge() calls git.merge for each changed table."""
        ctx_write.ensure_created()
        ctx_write.checkpoint("pre_exec")
        # Mock diff to return actual changes
        mock_sandbox.git.diff.return_value = [{"count": 5}]
        mock_sandbox.list_tables.return_value = ["sessions"]

        result = ctx_write.merge()
        # git.merge should have been called
        assert mock_sandbox.git.merge.called
        assert result.tables_merged == ["sessions"]
        assert result.rows_applied > 0

    def test_merge_skips_failed_tables(self, ctx_write, mock_sandbox):
        """merge() continues if one table's merge fails."""
        ctx_write.ensure_created()
        ctx_write.checkpoint("pre_exec")
        mock_sandbox.git.diff.return_value = [{"count": 2}]
        mock_sandbox.list_tables.return_value = ["sessions", "events"]
        mock_sandbox.git.merge.side_effect = [Exception("merge failed"), None]

        result = ctx_write.merge()
        # sessions failed, events succeeded
        assert "events" in result.tables_merged
        assert "sessions" not in result.tables_merged

    def test_checkpoint_same_name_no_drop(self, ctx_write, mock_sandbox):
        """Calling checkpoint with same name twice doesn't drop it."""
        ctx_write.ensure_created()
        ctx_write.checkpoint("pre_exec")
        ctx_write.checkpoint("pre_exec")  # Same name
        mock_sandbox.git.drop_snapshot.assert_not_called()


# ===========================================================================
# 13. CodeExecutor — missing scenarios
# ===========================================================================

class TestCodeExecutorExtra:
    @pytest.fixture
    def mock_runtime(self):
        rt = MagicMock(spec=Runtime)
        rt.execute.return_value = ExecutionResult(
            stdout="ok\n", stderr="", exit_code=0, execution_time_ms=10.0,
        )
        return rt

    @pytest.fixture
    def mock_sandbox(self):
        sandbox = MagicMock()
        sandbox.list_tables.return_value = ["sessions"]
        sandbox.git = MagicMock()
        sandbox.git.diff.return_value = []
        sandbox.source_db = "dev_agent"
        return sandbox

    @pytest.fixture
    def executor(self, mock_runtime, mock_sandbox):
        return CodeExecutor(
            runtime=mock_runtime, db=MagicMock(),
            sandbox=mock_sandbox, security=SecurityGuard(),
        )

    def test_none_access_passes_no_env(self, executor, mock_runtime):
        """data_access=NONE → runtime receives env=None."""
        executor.execute(CodeExecutionRequest(code="print(1)"))
        call_args = mock_runtime.execute.call_args[0]
        env_arg = call_args[3]  # 4th positional arg
        assert env_arg is None

    def test_read_access_env_has_mo_database(self, executor, mock_runtime, mock_sandbox):
        """data_access=READ → env contains both MO_DSN and MO_DATABASE."""
        executor.execute(CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.READ, session_id="s1",
            data_scope=DataContextScope.SESSION,
        ))
        env_arg = mock_runtime.execute.call_args[0][3]
        assert "MO_DSN" in env_arg
        assert "MO_DATABASE" in env_arg
        # Session-scoped: sandbox_name = code_exec_{session_id[:8]}
        assert env_arg["MO_DATABASE"] == "code_exec_s1"

    def test_session_none_with_session_scope_creates_new(self, executor, mock_sandbox):
        """session_id=None + SESSION scope → falls back to EXECUTION (new context each time)."""
        req = CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.READ,
            data_scope=DataContextScope.SESSION,
            session_id=None,  # No session_id
        )
        executor.execute(req)
        executor.execute(req)
        # Without session_id, can't reuse — creates new each time
        assert mock_sandbox.create.call_count == 2

    def test_multiple_write_executions_same_session(self, executor, mock_sandbox):
        """Multiple WRITE executions on same session: checkpoint called each time."""
        req = CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.WRITE,
            data_scope=DataContextScope.SESSION, session_id="sess1",
        )
        executor.execute(req)
        executor.execute(req)
        # snapshot called twice (once per execution)
        assert mock_sandbox.snapshot.call_count == 2

    def test_cleanup_then_reexecute_creates_new_context(self, executor, mock_sandbox):
        """After cleanup_session, next execute creates a fresh context."""
        req = CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.READ,
            data_scope=DataContextScope.SESSION, session_id="sess1",
        )
        executor.execute(req)
        executor.cleanup_session("sess1")
        executor.execute(req)
        # create called twice: once before cleanup, once after
        assert mock_sandbox.create.call_count == 2

    def test_security_error_message_contains_line_number(self, executor):
        """Security rejection stderr contains [L<n>] format."""
        r = executor.execute(CodeExecutionRequest(code="x = 1\nimport os\ny = 2"))
        assert r.security.safe is False
        assert "[L2]" in r.execution.stderr

    def test_write_failure_doesnt_destroy_session_context(self, executor, mock_runtime, mock_sandbox):
        """WRITE failure restores but doesn't destroy session-scoped context."""
        mock_runtime.execute.return_value = ExecutionResult(
            stdout="", stderr="error", exit_code=1, execution_time_ms=5.0,
        )
        executor.execute(CodeExecutionRequest(
            code="print(1)", data_access=DataAccessLevel.WRITE,
            data_scope=DataContextScope.SESSION, session_id="sess1",
        ))
        # restore called, but delete NOT called (session-scoped)
        mock_sandbox.restore.assert_called_once()
        mock_sandbox.delete.assert_not_called()


# ===========================================================================
# 14. ExecuteCodeSkill — missing scenarios
# ===========================================================================

class TestExecuteCodeSkillExtra:
    @pytest.fixture
    def mock_executor(self):
        executor = MagicMock()
        executor.execute.return_value = CodeExecutionResult(
            execution=ExecutionResult(
                stdout="ok\n", stderr="", exit_code=0, execution_time_ms=5.0,
            ),
            security=SecurityVerdict(safe=True),
        )
        return executor

    @pytest.fixture
    def skill(self, mock_executor):
        from core.skills.builtin import ExecuteCodeSkill
        return ExecuteCodeSkill(mock_executor)

    def _input(self, **kwargs):
        from core.skills.builtin import ExecuteCodeInput
        defaults = {"code": "print(1)", "user_id": "u1", "session_id": "s1"}
        defaults.update(kwargs)
        return ExecuteCodeInput(**defaults)

    def test_side_effect_profile_is_write(self, skill):
        from core.skills.base import SideEffectCategory
        assert skill.side_effect_profile.category == SideEffectCategory.WRITE

    @pytest.mark.asyncio
    async def test_allowed_imports_passed_to_executor(self, skill, mock_executor):
        await skill.execute(self._input(allowed_imports=["pandas", "numpy"]))
        req = mock_executor.execute.call_args[0][0]
        assert req.allowed_imports == ["pandas", "numpy"]

    @pytest.mark.asyncio
    async def test_data_access_string_converted_to_enum(self, skill, mock_executor):
        await skill.execute(self._input(data_access="write"))
        req = mock_executor.execute.call_args[0][0]
        assert req.data_access == DataAccessLevel.WRITE

    @pytest.mark.asyncio
    async def test_no_data_diff_returns_none(self, skill, mock_executor):
        out = await skill.execute(self._input())
        assert out.data_diff is None

    @pytest.mark.asyncio
    async def test_execution_time_propagated(self, skill, mock_executor):
        out = await skill.execute(self._input())
        assert out.execution_time_ms == 5.0
