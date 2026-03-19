"""Tests for code execution: Runtime, SecurityGuard, DataContext, CodeExecutor, ExecuteCodeSkill."""

from datetime import datetime, timezone
from unittest.mock import MagicMock, patch, PropertyMock, call
import pytest

pytestmark = pytest.mark.slow

from core.runtime import (
    Runtime,
    ExecutionResult,
    ResourceProfile,
    PROFILE_LIGHTWEIGHT,
    PROFILE_DATA_ANALYSIS,
)
from core.runtime.subprocess_runtime import SubprocessRuntime
from core.code_executor.security import (
    SecurityGuard,
    SecurityVerdict,
    SecurityIssue,
    DEFAULT_DENY_IMPORTS,
    DEFAULT_ALLOW_IMPORTS,
    DANGEROUS_CALLS,
    DANGEROUS_ATTRS,
    DANGEROUS_NAMES,
)
from core.code_executor.data_context import (
    DataAccessLevel,
    DataContext,
    TableDiff,
)
from core.code_executor import (
    CodeExecutor,
    CodeExecutionRequest,
    CodeExecutionResult,
    TimeTravelInfo,
)
from core.sandbox.sandbox import Sandbox


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
        assert r.started_at is None

    def test_truncated(self):
        r = ExecutionResult(
            stdout="x" * 100, stderr="", exit_code=0, execution_time_ms=1.0, truncated=True
        )
        assert r.truncated is True

    def test_started_at(self):
        now = datetime.now(timezone.utc)
        r = ExecutionResult(
            stdout="", stderr="", exit_code=0, execution_time_ms=1.0, started_at=now
        )
        assert r.started_at == now


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

    def test_started_at_recorded(self, runtime):
        r = runtime.execute("print(1)", "python")
        assert r.started_at is not None
        assert isinstance(r.started_at, datetime)
        assert r.started_at.tzinfo is not None  # UTC aware

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
            "import time; time.sleep(100)",
            "python",
            ResourceProfile(max_wall_seconds=1),
        )
        assert r.exit_code == 137
        assert "timed out" in r.stderr
        assert r.started_at is not None

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
        code = "print('x' * 2_000_000)"
        r = runtime.execute(code, "python", ResourceProfile(max_output_bytes=1000))
        assert r.truncated is True
        assert len(r.stdout) <= 1000

    def test_multiline_code(self, runtime):
        code = "data = [1,2,3,4,5]\nprint(f'{sum(data)},{sum(data)/len(data)}')"
        r = runtime.execute(code, "python")
        assert r.exit_code == 0
        assert "15,3.0" in r.stdout

    def test_syntax_error(self, runtime):
        r = runtime.execute("def foo(", "python")
        assert r.exit_code != 0
        assert "SyntaxError" in r.stderr

    def test_cwd_is_tmpdir(self, runtime):
        code = "import os; print(os.getcwd())"
        r = runtime.execute(code, "python")
        assert r.exit_code == 0
        assert "mo_exec_" in r.stdout

    def test_default_resources_when_none(self, runtime):
        r = runtime.execute("print('ok')", "python", None)
        assert r.exit_code == 0

    def test_empty_code(self, runtime):
        r = runtime.execute("", "python")
        assert r.exit_code == 0
        assert r.stdout == ""

    def test_timeout_elapsed_time_reasonable(self, runtime):
        r = runtime.execute(
            "import time; time.sleep(100)",
            "python",
            ResourceProfile(max_wall_seconds=1),
        )
        assert r.exit_code == 137
        assert 500 <= r.execution_time_ms <= 3000

    def test_large_stderr_doesnt_affect_stdout(self, runtime):
        code = "import sys\nsys.stderr.write('error\\n' * 1000)\nprint('stdout_ok')"
        r = runtime.execute(code, "python")
        assert r.exit_code == 0
        assert "stdout_ok" in r.stdout

    def test_subprocess_allowed_in_executed_code(self, runtime):
        """RLIMIT_NPROC must allow subprocess creation from executed code."""
        code = "import subprocess; r = subprocess.run(['echo', 'ok'], capture_output=True, text=True); print(r.stdout.strip())"
        r = runtime.execute(code, "python")
        assert r.exit_code == 0
        assert "ok" in r.stdout

    def test_memory_overhead_reserved(self, runtime):
        """Memory limit should include interpreter overhead so user code gets full allocation."""
        code = "x = bytearray(200 * 1024 * 1024); print('ok')"  # 200MB
        r = runtime.execute(code, "python", ResourceProfile(max_memory_mb=256))
        assert r.exit_code == 0
        assert "ok" in r.stdout


# ===========================================================================
# 4. SecurityGuard
# ===========================================================================


class TestSecurityGuard:
    @pytest.fixture
    def guard(self):
        return SecurityGuard()

    # --- Safe code ---
    def test_safe_simple(self, guard):
        assert guard.analyze("print(1 + 1)").safe is True

    def test_safe_allowed_imports(self, guard):
        assert guard.analyze("import json\nimport math\nimport datetime").safe is True

    def test_safe_from_import(self, guard):
        assert guard.analyze("from collections import defaultdict").safe is True

    def test_safe_multiline(self, guard):
        assert guard.analyze("import json\nprint(json.dumps({'a': 1}))").safe is True

    # --- Dangerous imports ---
    def test_block_os(self, guard):
        v = guard.analyze("import os")
        assert v.safe is False
        assert any(i.category == "dangerous_import" for i in v.issues)

    def test_block_subprocess(self, guard):
        assert guard.analyze("import subprocess").safe is False

    def test_block_sys(self, guard):
        assert guard.analyze("import sys").safe is False

    def test_block_socket(self, guard):
        assert guard.analyze("import socket").safe is False

    def test_block_ctypes(self, guard):
        assert guard.analyze("import ctypes").safe is False

    def test_block_pickle(self, guard):
        assert guard.analyze("import pickle").safe is False

    def test_block_shutil(self, guard):
        assert guard.analyze("import shutil").safe is False

    def test_block_importlib(self, guard):
        assert guard.analyze("import importlib").safe is False

    def test_block_from_os(self, guard):
        assert guard.analyze("from os import path").safe is False

    def test_block_from_subprocess(self, guard):
        assert guard.analyze("from subprocess import run").safe is False

    def test_block_nested_import(self, guard):
        assert guard.analyze("import os.path").safe is False

    # --- Dangerous calls ---
    def test_block_eval(self, guard):
        v = guard.analyze("eval('1+1')")
        assert v.safe is False
        assert any(i.category == "dangerous_call" for i in v.issues)

    def test_block_exec(self, guard):
        assert guard.analyze("exec('print(1)')").safe is False

    def test_block_compile(self, guard):
        assert guard.analyze("compile('1+1', '<string>', 'eval')").safe is False

    def test_block___import__(self, guard):
        assert guard.analyze("__import__('os')").safe is False

    def test_block_open(self, guard):
        assert guard.analyze("open('/etc/passwd')").safe is False

    def test_block_getattr(self, guard):
        assert guard.analyze("getattr(obj, 'method')").safe is False

    def test_block_breakpoint(self, guard):
        assert guard.analyze("breakpoint()").safe is False

    # --- Multiple issues ---
    def test_multiple_issues(self, guard):
        v = guard.analyze("import os\nimport subprocess\neval('1')")
        assert v.safe is False
        assert len(v.issues) == 3

    def test_issue_line_numbers(self, guard):
        v = guard.analyze("x = 1\nimport os\ny = 2\neval('1')")
        lines = {i.line for i in v.issues}
        assert 2 in lines
        assert 4 in lines

    # --- Syntax errors ---
    def test_syntax_error(self, guard):
        v = guard.analyze("def foo(")
        assert v.safe is False
        assert v.issues[0].category == "syntax_error"

    # --- Extra allowed imports ---
    def test_extra_allowed(self, guard):
        assert (
            guard.analyze("import pandas\nimport numpy", extra_allowed=["pandas", "numpy"]).safe
            is True
        )

    def test_extra_allowed_doesnt_override_deny(self, guard):
        assert guard.analyze("import os", extra_allowed=["os"]).safe is False

    # --- Custom deny/allow ---
    def test_custom_deny(self):
        assert SecurityGuard(deny_imports={"requests"}).analyze("import requests").safe is False

    def test_custom_allow(self):
        assert SecurityGuard(allow_imports={"custom_lib"}).analyze("import custom_lib").safe is True

    # --- Unsupported language ---
    def test_unsupported_language(self, guard):
        v = guard.analyze("console.log(1)", language="javascript")
        assert v.safe is False
        assert "unsupported" in v.issues[0].category

    # --- Default lists sanity ---
    def test_deny_list_completeness(self):
        for mod in ["os", "subprocess", "sys", "socket", "ctypes", "pickle"]:
            assert mod in DEFAULT_DENY_IMPORTS

    def test_allow_list_completeness(self):
        for mod in ["json", "math", "datetime", "re", "collections"]:
            assert mod in DEFAULT_ALLOW_IMPORTS

    def test_dangerous_calls_completeness(self):
        for c in ["eval", "exec", "compile", "__import__", "open"]:
            assert c in DANGEROUS_CALLS

    # --- Bypass vector detection ---
    def test_block_builtins_access(self, guard):
        assert guard.analyze("x = __builtins__").safe is False

    def test_block_dunder_subclasses(self, guard):
        assert guard.analyze("x = ().__class__.__subclasses__()").safe is False

    def test_block_dunder_globals(self, guard):
        assert guard.analyze("x = f.__globals__").safe is False

    def test_block_dunder_bases(self, guard):
        assert guard.analyze("x = int.__bases__").safe is False

    def test_block_dunder_mro(self, guard):
        assert guard.analyze("x = int.__mro__").safe is False

    def test_block_dunder_code(self, guard):
        assert guard.analyze("x = f.__code__").safe is False

    def test_block_class_chain(self, guard):
        assert guard.analyze("x = ''.__class__.__mro__").safe is False

    def test_empty_code_is_safe(self, guard):
        assert guard.analyze("").safe is True

    def test_block_dunder_dict_attr(self, guard):
        assert guard.analyze("x = obj.__dict__").safe is False

    def test_block_dunder_init_subclass(self, guard):
        assert guard.analyze("x = cls.__init_subclass__").safe is False

    def test_multiple_bypass_vectors(self, guard):
        assert guard.analyze("__builtins__['eval']('1')").safe is False

    def test_safe_dunder_in_string(self, guard):
        assert guard.analyze('x = "__builtins__"').safe is True

    def test_safe_dunder_in_comment(self, guard):
        assert guard.analyze("# use __builtins__ carefully\nprint(1)").safe is True

    def test_vars_documented_gap(self, guard):
        assert isinstance(guard.analyze("vars()").safe, bool)

    def test_type_documented_gap(self, guard):
        assert isinstance(guard.analyze("C = type('C', (object,), {})").safe, bool)


# ===========================================================================
# 5. DataAccessLevel
# ===========================================================================


class TestDataEnums:
    def test_access_levels(self):
        assert DataAccessLevel.NONE.value == "none"
        assert DataAccessLevel.READ.value == "read"
        assert DataAccessLevel.WRITE.value == "write"

    def test_from_string(self):
        assert DataAccessLevel("read") == DataAccessLevel.READ
        assert DataAccessLevel("write") == DataAccessLevel.WRITE


# ===========================================================================
# 6. DataContext (mocked Branch)
# ===========================================================================


@pytest.fixture(autouse=True)
def _stub_visibility_waits(monkeypatch):
    """Visibility polling is covered in dedicated sandbox tests."""
    monkeypatch.setattr(Sandbox, "wait_until_database_visible", lambda *args, **kwargs: None)
    monkeypatch.setattr(Sandbox, "wait_until_table_visible", lambda *args, **kwargs: None)


class TestDataContext:
    @pytest.fixture
    def mock_branch(self):
        return MagicMock()

    @pytest.fixture
    def mock_db(self):
        from sqlalchemy.engine import make_url

        db = MagicMock()
        db.get_bind.return_value.url = make_url("mysql+pymysql://root:111@localhost:6001/test")
        return db

    @pytest.fixture
    def ctx_read(self, mock_branch, mock_db):
        return DataContext(
            db_factory=lambda: mock_db,
            branch=mock_branch,
            sandbox_name="test_sandbox",
            source_db="dev_agent",
            access=DataAccessLevel.READ,
        )

    @pytest.fixture
    def ctx_write(self, mock_branch, mock_db):
        return DataContext(
            db_factory=lambda: mock_db,
            branch=mock_branch,
            sandbox_name="test_sandbox",
            source_db="dev_agent",
            access=DataAccessLevel.WRITE,
        )

    # --- DSN ---
    def test_dsn_uses_engine_credentials(self, ctx_write):
        dsn = ctx_write.dsn
        assert "root:111" in dsn
        assert "localhost:6001" in dsn
        assert "test_sandbox" in dsn

    # --- Lifecycle ---
    def test_not_alive_before_create(self, ctx_read):
        assert ctx_read.alive is False

    def test_alive_after_create(self, ctx_write, mock_db):
        ctx_write.ensure_created()
        assert ctx_write.alive is True
        # CREATE DATABASE IF NOT EXISTS called
        assert mock_db.execute.called

    def test_ensure_created_idempotent(self, ctx_write, mock_db):
        ctx_write.ensure_created()
        call_count_1 = mock_db.execute.call_count
        ctx_write.ensure_created()
        assert mock_db.execute.call_count == call_count_1  # no new calls

    # --- Table-level branch ---
    def test_ensure_tables(self, ctx_write, mock_branch):
        ctx_write.ensure_created()
        ctx_write.ensure_tables(["orders", "products"])
        assert mock_branch.create.call_count == 2
        mock_branch.create.assert_any_call(
            name="test_sandbox.orders",
            source="dev_agent.orders",
        )
        mock_branch.create.assert_any_call(
            name="test_sandbox.products",
            source="dev_agent.products",
        )

    def test_ensure_tables_idempotent(self, ctx_write, mock_branch):
        ctx_write.ensure_created()
        ctx_write.ensure_tables(["orders"])
        ctx_write.ensure_tables(["orders"])
        assert mock_branch.create.call_count == 1

    def test_ensure_tables_incremental(self, ctx_write, mock_branch):
        ctx_write.ensure_created()
        ctx_write.ensure_tables(["orders"])
        ctx_write.ensure_tables(["orders", "products"])
        assert mock_branch.create.call_count == 2

    # --- Diff (native data branch diff) ---
    def test_diff_empty(self, ctx_write, mock_branch):
        mock_branch.diff.return_value = []
        ctx_write.ensure_created()
        ctx_write.ensure_tables(["orders"])
        assert ctx_write.diff() == []

    def test_diff_returns_rows(self, ctx_write, mock_branch):
        mock_branch.diff.return_value = [
            {"diff t2 against t1": "t2", "flag": "INSERT", "a": 5, "b": 5},
        ]
        ctx_write.ensure_created()
        ctx_write.ensure_tables(["orders"])
        diffs = ctx_write.diff()
        assert len(diffs) == 1
        assert diffs[0].table == "orders"
        assert len(diffs[0].rows) == 1

    def test_diff_with_explicit_tables(self, ctx_write, mock_branch):
        mock_branch.diff.return_value = [{"flag": "INSERT"}]
        ctx_write.ensure_created()
        ctx_write.ensure_tables(["orders", "products"])
        diffs = ctx_write.diff(["orders"])
        # Only orders diffed
        assert mock_branch.diff.call_count == 1
        call_args = mock_branch.diff.call_args
        assert "orders" in call_args[1]["target"]

    def test_diff_continues_on_exception(self, ctx_write, mock_branch):
        mock_branch.diff.side_effect = [Exception("fail"), [{"flag": "INSERT"}]]
        ctx_write.ensure_created()
        ctx_write.ensure_tables(["orders", "products"])
        diffs = ctx_write.diff()
        assert isinstance(diffs, list)

    # --- Merge ---
    def test_merge_requires_write(self, ctx_read):
        ctx_read.ensure_created()
        with pytest.raises(RuntimeError, match="WRITE"):
            ctx_read.merge()

    def test_merge_calls_branch_merge(self, ctx_write, mock_branch):
        ctx_write.ensure_created()
        ctx_write.ensure_tables(["orders"])
        result = ctx_write.merge()
        mock_branch.merge.assert_called_once_with(
            source="test_sandbox.orders",
            target="dev_agent.orders",
            on_conflict="skip",
        )
        assert result.tables_merged == ["orders"]

    def test_merge_with_conflict_accept(self, ctx_write, mock_branch):
        ctx_write.ensure_created()
        ctx_write.ensure_tables(["orders"])
        ctx_write.merge(on_conflict="accept")
        mock_branch.merge.assert_called_once_with(
            source="test_sandbox.orders",
            target="dev_agent.orders",
            on_conflict="accept",
        )

    def test_merge_tracks_failures(self, ctx_write, mock_branch):
        def merge_side_effect(source, target, on_conflict):
            if "orders" in source:
                raise Exception("conflict")

        mock_branch.merge.side_effect = merge_side_effect
        ctx_write.ensure_created()
        ctx_write.ensure_tables(["orders", "products"])
        result = ctx_write.merge()
        assert "orders" in result.tables_failed
        assert "products" in result.tables_merged

    # --- Destroy ---
    def test_destroy(self, ctx_write, mock_branch, mock_db):
        ctx_write.ensure_created()
        ctx_write.ensure_tables(["orders", "products"])
        ctx_write.destroy()
        assert ctx_write.alive is False
        # data branch delete called per table
        assert mock_branch.delete.call_count == 2
        # DROP DATABASE called via db.execute(text(...))
        drop_found = False
        for c in mock_db.execute.call_args_list:
            sql_arg = c[0][0]
            if hasattr(sql_arg, "text") and "DROP DATABASE" in sql_arg.text:
                drop_found = True
        assert drop_found

    def test_destroy_idempotent(self, ctx_write):
        ctx_write.destroy()  # before create — no-op

    def test_destroy_clears_state(self, ctx_write, mock_branch):
        ctx_write.ensure_created()
        ctx_write.ensure_tables(["orders"])
        ctx_write.destroy()
        assert ctx_write._branched_tables == set()


# ===========================================================================
# 7. TimeTravelInfo
# ===========================================================================


class TestTimeTravelInfo:
    def test_fields(self):
        now = datetime.now(timezone.utc)
        tt = TimeTravelInfo(
            started_at=now,
            source_db="prod",
            sandbox_db="sandbox_s1",
        )
        assert tt.started_at == now
        assert tt.source_db == "prod"
        assert tt.sandbox_db == "sandbox_s1"


# ===========================================================================
# 8. CodeExecutionRequest
# ===========================================================================


class TestCodeExecutionRequest:
    def test_defaults(self):
        req = CodeExecutionRequest(code="print(1)")
        assert req.language == "python"
        assert req.data_access == DataAccessLevel.NONE
        assert req.session_id is None
        assert req.source_db is None
        assert req.tables is None
        assert req.allowed_imports is None

    def test_write_request(self):
        req = CodeExecutionRequest(
            code="x",
            data_access=DataAccessLevel.WRITE,
            source_db="prod",
            tables=["orders"],
            session_id="s1",
            allowed_imports=["pandas"],
        )
        assert req.source_db == "prod"
        assert req.tables == ["orders"]


# ===========================================================================
# 9. CodeExecutor
# ===========================================================================


class TestCodeExecutor:
    @pytest.fixture
    def mock_runtime(self):
        rt = MagicMock(spec=Runtime)
        rt.execute.return_value = ExecutionResult(
            stdout="42\n",
            stderr="",
            exit_code=0,
            execution_time_ms=10.0,
            started_at=datetime(2026, 2, 20, 15, 0, 0, tzinfo=timezone.utc),
        )
        rt.supported_languages = ["python"]
        return rt

    @pytest.fixture
    def mock_branch(self):
        branch = MagicMock()
        branch.diff.return_value = []
        return branch

    @pytest.fixture
    def mock_db(self):
        return MagicMock()

    @pytest.fixture
    def executor(self, mock_runtime, mock_branch, mock_db):
        return CodeExecutor(
            runtime=mock_runtime,
            db_factory=lambda: mock_db,
            branch=mock_branch,
            security=SecurityGuard(),
        )

    # --- Basic ---
    def test_simple_execution(self, executor, mock_runtime):
        r = executor.execute(CodeExecutionRequest(code="print(42)"))
        assert r.execution.exit_code == 0
        assert r.security.safe is True
        assert r.time_travel is None
        mock_runtime.execute.assert_called_once()

    def test_security_rejection(self, executor, mock_runtime):
        r = executor.execute(CodeExecutionRequest(code="import os"))
        assert r.security.safe is False
        assert "blocked" in r.execution.stderr
        mock_runtime.execute.assert_not_called()

    def test_security_rejection_eval(self, executor, mock_runtime):
        assert executor.execute(CodeExecutionRequest(code="eval('1')")).security.safe is False

    def test_allowed_imports(self, executor, mock_runtime):
        r = executor.execute(
            CodeExecutionRequest(
                code="import pandas",
                allowed_imports=["pandas"],
            )
        )
        assert r.security.safe is True

    def test_custom_resources(self, executor, mock_runtime):
        profile = ResourceProfile(max_memory_mb=512)
        executor.execute(CodeExecutionRequest(code="print(1)", resources=profile))
        assert mock_runtime.execute.call_args[0][2] == profile

    # --- NONE access ---
    def test_none_access_injects_runtime_caps(self, executor, mock_runtime):
        executor.execute(CodeExecutionRequest(code="print(1)"))
        env = mock_runtime.execute.call_args[0][3]
        assert "MO_RUNTIME_ISOLATION" in env
        assert "MO_DATABASE" not in env

    # --- READ access ---
    def test_read_access_passes_source_db(self, executor, mock_runtime):
        executor.execute(
            CodeExecutionRequest(
                code="print(1)",
                data_access=DataAccessLevel.READ,
                source_db="mydb",
            )
        )
        assert mock_runtime.execute.call_args[0][3]["MO_DATABASE"] == "mydb"

    def test_read_access_no_branch(self, executor, mock_branch):
        executor.execute(
            CodeExecutionRequest(
                code="print(1)",
                data_access=DataAccessLevel.READ,
                source_db="mydb",
            )
        )
        mock_branch.create.assert_not_called()

    # --- WRITE access ---
    def test_write_requires_session_id(self, executor):
        with pytest.raises(ValueError, match="session_id"):
            executor.execute(
                CodeExecutionRequest(
                    code="print(1)",
                    data_access=DataAccessLevel.WRITE,
                    source_db="db",
                    tables=["t"],
                )
            )

    def test_write_requires_source_db(self, executor):
        with pytest.raises(ValueError, match="source_db"):
            executor.execute(
                CodeExecutionRequest(
                    code="print(1)",
                    data_access=DataAccessLevel.WRITE,
                    session_id="s1",
                    tables=["t"],
                )
            )

    def test_write_requires_tables(self, executor):
        with pytest.raises(ValueError, match="tables"):
            executor.execute(
                CodeExecutionRequest(
                    code="print(1)",
                    data_access=DataAccessLevel.WRITE,
                    session_id="s1",
                    source_db="db",
                )
            )

    def test_write_branches_tables(self, executor, mock_branch):
        executor.execute(
            CodeExecutionRequest(
                code="print(1)",
                data_access=DataAccessLevel.WRITE,
                session_id="sess1",
                source_db="prod",
                tables=["orders", "products"],
            )
        )
        assert mock_branch.create.call_count == 2

    def test_write_returns_time_travel(self, executor):
        r = executor.execute(
            CodeExecutionRequest(
                code="print(1)",
                data_access=DataAccessLevel.WRITE,
                session_id="sess1",
                source_db="prod",
                tables=["orders"],
            )
        )
        assert r.time_travel is not None
        assert r.time_travel.source_db == "prod"
        assert "code_exec_" in r.time_travel.sandbox_db
        assert r.time_travel.started_at is not None

    def test_write_success_returns_diff(self, executor, mock_branch):
        mock_branch.diff.return_value = [{"flag": "INSERT", "a": 1}]
        r = executor.execute(
            CodeExecutionRequest(
                code="print(1)",
                data_access=DataAccessLevel.WRITE,
                session_id="sess1",
                source_db="prod",
                tables=["orders"],
            )
        )
        assert r.data_diff is not None

    def test_write_failure_no_diff(self, executor, mock_runtime):
        mock_runtime.execute.return_value = ExecutionResult(
            stdout="",
            stderr="error",
            exit_code=1,
            execution_time_ms=5.0,
            started_at=datetime(2026, 2, 20, 15, 0, 0, tzinfo=timezone.utc),
        )
        r = executor.execute(
            CodeExecutionRequest(
                code="print(1)",
                data_access=DataAccessLevel.WRITE,
                session_id="sess1",
                source_db="prod",
                tables=["orders"],
            )
        )
        assert r.data_diff is None
        # time_travel still recorded even on failure
        assert r.time_travel is not None

    # --- Session reuse ---
    def test_session_reuses_context(self, executor, mock_branch):
        req = CodeExecutionRequest(
            code="print(1)",
            data_access=DataAccessLevel.WRITE,
            session_id="sess1",
            source_db="prod",
            tables=["orders"],
        )
        executor.execute(req)
        executor.execute(req)
        # branch.create called once (idempotent)
        assert mock_branch.create.call_count == 1

    def test_different_sessions(self, executor, mock_branch):
        for sid in ["sess1", "sess2"]:
            executor.execute(
                CodeExecutionRequest(
                    code="print(1)",
                    data_access=DataAccessLevel.WRITE,
                    session_id=sid,
                    source_db="prod",
                    tables=["orders"],
                )
            )
        assert mock_branch.create.call_count == 2

    def test_dynamic_table_addition(self, executor, mock_branch):
        """Second execution adds new table to existing session."""
        executor.execute(
            CodeExecutionRequest(
                code="print(1)",
                data_access=DataAccessLevel.WRITE,
                session_id="sess1",
                source_db="prod",
                tables=["orders"],
            )
        )
        executor.execute(
            CodeExecutionRequest(
                code="print(2)",
                data_access=DataAccessLevel.WRITE,
                session_id="sess1",
                source_db="prod",
                tables=["orders", "products"],
            )
        )
        # orders branched once, products branched once
        assert mock_branch.create.call_count == 2

    # --- Cleanup ---
    def test_cleanup_session(self, executor, mock_branch, mock_db):
        executor.execute(
            CodeExecutionRequest(
                code="print(1)",
                data_access=DataAccessLevel.WRITE,
                session_id="sess1",
                source_db="prod",
                tables=["orders"],
            )
        )
        executor.cleanup_session("sess1")
        mock_branch.delete.assert_called()
        drop_found = False
        for c in mock_db.execute.call_args_list:
            sql_arg = c[0][0]
            if hasattr(sql_arg, "text") and "DROP DATABASE" in sql_arg.text:
                drop_found = True
        assert drop_found

    def test_cleanup_nonexistent(self, executor):
        executor.cleanup_session("nonexistent")

    # --- Runtime exception ---
    def test_runtime_exception(self, executor, mock_runtime):
        mock_runtime.execute.side_effect = RuntimeError("boom")
        r = executor.execute(CodeExecutionRequest(code="print(1)"))
        assert r.execution.exit_code == 1
        assert "Runtime error" in r.execution.stderr

    def test_runtime_exception_no_destroy(self, executor, mock_runtime):
        mock_runtime.execute.side_effect = RuntimeError("boom")
        executor.execute(
            CodeExecutionRequest(
                code="print(1)",
                data_access=DataAccessLevel.WRITE,
                session_id="sess1",
                source_db="prod",
                tables=["orders"],
            )
        )
        assert "sess1" in executor._session_contexts

    def test_security_error_contains_line_number(self, executor):
        r = executor.execute(CodeExecutionRequest(code="x = 1\nimport os\ny = 2"))
        assert "[L2]" in r.execution.stderr


# ===========================================================================
# 10. ExecuteCodeSkill
# ===========================================================================


class TestExecuteCodeSkill:
    @pytest.fixture
    def mock_executor(self):
        executor = MagicMock()
        executor.execute.return_value = CodeExecutionResult(
            execution=ExecutionResult(
                stdout="result\n",
                stderr="",
                exit_code=0,
                execution_time_ms=15.0,
                started_at=datetime(2026, 2, 20, 15, 0, 0, tzinfo=timezone.utc),
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

    def test_skill_metadata(self, skill):
        assert skill.name == "execute_code"
        assert skill.version == "1.0.0"

    def test_validate_input(self, skill):
        from core.skills.builtin import ExecuteCodeInput

        inp = skill.validate_input({"code": "print(1)", "user_id": "u1", "session_id": "s1"})
        assert isinstance(inp, ExecuteCodeInput)
        assert inp.data_access == "none"

    @pytest.mark.asyncio
    async def test_execute_success(self, skill):
        out = await skill.execute(self._input())
        assert out.success is True
        assert out.result == "result\n"
        assert out.error is None

    @pytest.mark.asyncio
    async def test_execute_failure(self, skill, mock_executor):
        mock_executor.execute.return_value = CodeExecutionResult(
            execution=ExecutionResult(
                stdout="",
                stderr="NameError: x",
                exit_code=1,
                execution_time_ms=5.0,
            ),
            security=SecurityVerdict(safe=True),
        )
        out = await skill.execute(self._input())
        assert out.success is False
        assert out.error == "NameError: x"

    @pytest.mark.asyncio
    async def test_execute_with_data_diff(self, skill, mock_executor):
        mock_executor.execute.return_value = CodeExecutionResult(
            execution=ExecutionResult(
                stdout="done\n",
                stderr="",
                exit_code=0,
                execution_time_ms=20.0,
            ),
            security=SecurityVerdict(safe=True),
            data_diff=[TableDiff(table="orders", rows=[{"flag": "INSERT", "a": 1}])],
        )
        out = await skill.execute(self._input(data_access="write"))
        assert out.data_diff is not None
        assert out.data_diff[0]["table"] == "orders"

    @pytest.mark.asyncio
    async def test_execute_with_time_travel(self, skill, mock_executor):
        now = datetime(2026, 2, 20, 15, 0, 0, tzinfo=timezone.utc)
        mock_executor.execute.return_value = CodeExecutionResult(
            execution=ExecutionResult(
                stdout="done\n",
                stderr="",
                exit_code=0,
                execution_time_ms=20.0,
            ),
            security=SecurityVerdict(safe=True),
            time_travel=TimeTravelInfo(
                started_at=now,
                source_db="prod",
                sandbox_db="sandbox_s1",
            ),
        )
        out = await skill.execute(self._input(data_access="write"))
        assert out.time_travel is not None
        assert out.time_travel["source_db"] == "prod"
        assert out.time_travel["sandbox_db"] == "sandbox_s1"

    @pytest.mark.asyncio
    async def test_no_time_travel_returns_none(self, skill):
        out = await skill.execute(self._input())
        assert out.time_travel is None

    @pytest.mark.asyncio
    async def test_source_db_and_tables_passed(self, skill, mock_executor):
        await skill.execute(
            self._input(
                data_access="write",
                source_db="prod",
                tables=["orders"],
            )
        )
        req = mock_executor.execute.call_args[0][0]
        assert req.source_db == "prod"
        assert req.tables == ["orders"]

    @pytest.mark.asyncio
    async def test_allowed_imports_passed(self, skill, mock_executor):
        await skill.execute(self._input(allowed_imports=["pandas"]))
        req = mock_executor.execute.call_args[0][0]
        assert req.allowed_imports == ["pandas"]

    @pytest.mark.asyncio
    async def test_data_access_string_to_enum(self, skill, mock_executor):
        await skill.execute(self._input(data_access="write"))
        req = mock_executor.execute.call_args[0][0]
        assert req.data_access == DataAccessLevel.WRITE

    def test_side_effect_profile_is_write(self, skill):
        from core.skills.base import SideEffectCategory

        assert skill.side_effect_profile.category == SideEffectCategory.WRITE

    @pytest.mark.asyncio
    async def test_execution_time_propagated(self, skill):
        out = await skill.execute(self._input())
        assert out.execution_time_ms == 15.0
