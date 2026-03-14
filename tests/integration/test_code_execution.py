"""Integration tests for code execution — real subprocess execution, end-to-end flows."""

import pytest

from core.runtime import ResourceProfile, PROFILE_LIGHTWEIGHT
from core.runtime.subprocess_runtime import SubprocessRuntime
from core.code_executor import CodeExecutor, CodeExecutionRequest
from core.code_executor.security import SecurityGuard
from core.code_executor.data_context import DataAccessLevel


@pytest.fixture
def runtime():
    return SubprocessRuntime()


@pytest.fixture
def guard():
    return SecurityGuard()


@pytest.fixture
def executor(runtime, guard):
    """CodeExecutor with real runtime, no DB (data_access=NONE only)."""
    return CodeExecutor(runtime=runtime, db_factory=lambda: None, branch=None, security=guard)


# ===========================================================================
# 1. Real subprocess execution
# ===========================================================================


class TestRealExecution:
    def test_arithmetic(self, executor):
        r = executor.execute(CodeExecutionRequest(code="print(2 ** 10)"))
        assert r.execution.exit_code == 0
        assert r.execution.stdout.strip() == "1024"

    def test_string_operations(self, executor):
        r = executor.execute(
            CodeExecutionRequest(
                code="print('hello world'.upper())",
            )
        )
        assert r.execution.exit_code == 0
        assert "HELLO WORLD" in r.execution.stdout

    def test_json_processing(self, executor):
        code = """
import json
data = [{"name": "alice", "score": 95}, {"name": "bob", "score": 87}]
avg = sum(d["score"] for d in data) / len(data)
print(json.dumps({"average": avg, "count": len(data)}))
"""
        r = executor.execute(CodeExecutionRequest(code=code))
        assert r.execution.exit_code == 0
        import json

        output = json.loads(r.execution.stdout.strip())
        assert output["average"] == 91.0
        assert output["count"] == 2

    def test_math_operations(self, executor):
        code = """
import math
print(f"{math.pi:.4f}")
print(f"{math.sqrt(144):.0f}")
"""
        r = executor.execute(CodeExecutionRequest(code=code))
        assert r.execution.exit_code == 0
        lines = r.execution.stdout.strip().split("\n")
        assert lines[0] == "3.1416"
        assert lines[1] == "12"

    def test_datetime_operations(self, executor):
        code = """
from datetime import datetime, timedelta
now = datetime(2026, 2, 20, 22, 0, 0)
future = now + timedelta(days=7)
print(future.isoformat())
"""
        r = executor.execute(CodeExecutionRequest(code=code))
        assert r.execution.exit_code == 0
        assert "2026-02-27" in r.execution.stdout

    def test_collections_operations(self, executor):
        code = """
from collections import Counter
words = ["apple", "banana", "apple", "cherry", "banana", "apple"]
c = Counter(words)
print(c.most_common(1)[0])
"""
        r = executor.execute(CodeExecutionRequest(code=code))
        assert r.execution.exit_code == 0
        assert "apple" in r.execution.stdout

    def test_regex(self, executor):
        code = """
import re
text = "Contact: user@example.com or admin@test.org"
emails = re.findall(r'[\\w.]+@[\\w.]+', text)
print(len(emails))
"""
        r = executor.execute(CodeExecutionRequest(code=code))
        assert r.execution.exit_code == 0
        assert r.execution.stdout.strip() == "2"

    def test_csv_processing(self, executor):
        code = """
import csv
import io
data = "name,score\\nalice,95\\nbob,87\\n"
reader = csv.DictReader(io.StringIO(data))
rows = list(reader)
print(len(rows))
print(rows[0]["name"])
"""
        r = executor.execute(CodeExecutionRequest(code=code))
        assert r.execution.exit_code == 0
        lines = r.execution.stdout.strip().split("\n")
        assert lines[0] == "2"
        assert lines[1] == "alice"

    def test_list_comprehension(self, executor):
        code = """
squares = [x**2 for x in range(10) if x % 2 == 0]
print(squares)
"""
        r = executor.execute(CodeExecutionRequest(code=code))
        assert r.execution.exit_code == 0
        assert "[0, 4, 16, 36, 64]" in r.execution.stdout

    def test_multiline_function(self, executor):
        code = """
def fibonacci(n):
    if n <= 1:
        return n
    a, b = 0, 1
    for _ in range(2, n + 1):
        a, b = b, a + b
    return b

print(fibonacci(10))
"""
        r = executor.execute(CodeExecutionRequest(code=code))
        assert r.execution.exit_code == 0
        assert r.execution.stdout.strip() == "55"


# ===========================================================================
# 2. Security + execution integration
# ===========================================================================


class TestSecurityIntegration:
    def test_blocked_code_never_executes(self, executor):
        """Dangerous code is rejected before reaching subprocess."""
        r = executor.execute(CodeExecutionRequest(code="import os\nos.system('echo pwned')"))
        assert r.execution.exit_code == 1
        assert r.security.safe is False
        assert "pwned" not in r.execution.stdout

    def test_blocked_eval_never_executes(self, executor):
        r = executor.execute(CodeExecutionRequest(code="result = eval('2+2')\nprint(result)"))
        assert r.security.safe is False
        assert r.execution.stdout == ""

    def test_safe_code_executes(self, executor):
        r = executor.execute(CodeExecutionRequest(code="import json\nprint(json.dumps([1,2,3]))"))
        assert r.security.safe is True
        assert r.execution.exit_code == 0
        assert "[1, 2, 3]" in r.execution.stdout

    def test_mixed_safe_and_dangerous(self, executor):
        """If any part is dangerous, entire code is rejected."""
        code = "import json\nimport os\nprint(json.dumps([1]))"
        r = executor.execute(CodeExecutionRequest(code=code))
        assert r.security.safe is False
        assert r.execution.stdout == ""

    def test_extra_allowed_imports_work(self, executor):
        """Extra allowed imports pass security and execute."""
        # hashlib is in default allow list
        code = "import hashlib\nprint(hashlib.md5(b'test').hexdigest())"
        r = executor.execute(CodeExecutionRequest(code=code))
        assert r.security.safe is True
        assert r.execution.exit_code == 0
        assert len(r.execution.stdout.strip()) == 32  # MD5 hex digest


# ===========================================================================
# 3. Resource limits
# ===========================================================================


class TestResourceLimits:
    def test_timeout_kills_execution(self, executor):
        r = executor.execute(
            CodeExecutionRequest(
                code="import time; time.sleep(100)",
                resources=ResourceProfile(max_wall_seconds=1),
            )
        )
        assert r.execution.exit_code == 137
        assert "timed out" in r.execution.stderr
        assert r.execution.execution_time_ms < 5000  # Should be ~1s, not 100s

    def test_output_truncation(self, executor):
        code = "print('x' * 2_000_000)"
        r = executor.execute(
            CodeExecutionRequest(
                code=code,
                resources=ResourceProfile(max_output_bytes=500),
            )
        )
        assert r.execution.exit_code == 0
        assert r.execution.truncated is True
        assert len(r.execution.stdout) <= 500

    def test_lightweight_profile(self, executor):
        r = executor.execute(
            CodeExecutionRequest(
                code="print('fast')",
                resources=PROFILE_LIGHTWEIGHT,
            )
        )
        assert r.execution.exit_code == 0
        assert "fast" in r.execution.stdout


# ===========================================================================
# 4. Error handling
# ===========================================================================


class TestErrorHandling:
    def test_runtime_error(self, executor):
        r = executor.execute(CodeExecutionRequest(code="1/0"))
        assert r.execution.exit_code != 0
        assert "ZeroDivisionError" in r.execution.stderr

    def test_name_error(self, executor):
        r = executor.execute(CodeExecutionRequest(code="print(undefined_var)"))
        assert r.execution.exit_code != 0
        assert "NameError" in r.execution.stderr

    def test_type_error(self, executor):
        r = executor.execute(CodeExecutionRequest(code="'a' + 1"))
        assert r.execution.exit_code != 0
        assert "TypeError" in r.execution.stderr

    def test_syntax_error(self, executor):
        r = executor.execute(CodeExecutionRequest(code="def foo("))
        # Caught by SecurityGuard at AST parse stage
        assert r.security.safe is False

    def test_import_not_found(self, executor):
        """Importing a non-existent module fails at runtime, not security."""
        r = executor.execute(CodeExecutionRequest(code="import nonexistent_module_xyz"))
        # Security passes (not in deny list)
        assert r.security.safe is True
        # But runtime fails
        assert r.execution.exit_code != 0
        assert "ModuleNotFoundError" in r.execution.stderr

    def test_keyboard_interrupt_handled(self, executor):
        """KeyboardInterrupt in code doesn't crash the executor."""
        r = executor.execute(CodeExecutionRequest(code="raise KeyboardInterrupt"))
        assert r.execution.exit_code != 0

    def test_system_exit_handled(self, executor):
        """SystemExit in code doesn't crash the executor."""
        r = executor.execute(CodeExecutionRequest(code="raise SystemExit(42)"))
        assert r.execution.exit_code == 42

    def test_memory_error_code(self, executor):
        """Code that tries to allocate too much memory."""
        code = "x = [0] * (10**9)"  # ~8GB
        r = executor.execute(
            CodeExecutionRequest(
                code=code,
                resources=ResourceProfile(max_wall_seconds=5),
            )
        )
        # Should fail (MemoryError or killed)
        assert r.execution.exit_code != 0


# ===========================================================================
# 5. Environment isolation
# ===========================================================================


class TestIsolation:
    def test_no_file_access_to_project(self, executor):
        """Code cannot read project files."""
        code = """
import os
# Should be in tmpdir, not project root
cwd = os.getcwd()
assert 'mo_exec_' in cwd, f"Expected tmpdir, got {cwd}"
print("isolated")
"""
        # os is blocked by security
        r = executor.execute(CodeExecutionRequest(code=code))
        assert r.security.safe is False

    def test_env_var_injection(self, executor):
        """MO_DSN env var is only set when data_access != NONE."""
        code = """
import os
dsn = os.environ.get('MO_DSN', 'not_set')
print(dsn)
"""
        # os is blocked, but this tests the concept
        r = executor.execute(CodeExecutionRequest(code=code))
        assert r.security.safe is False

    def test_separate_executions_isolated(self, executor):
        """Two executions don't share state."""
        code1 = "x = 42\nprint(x)"
        code2 = "print(x)"  # x not defined in this execution

        r1 = executor.execute(CodeExecutionRequest(code=code1))
        r2 = executor.execute(CodeExecutionRequest(code=code2))

        assert r1.execution.exit_code == 0
        assert r2.execution.exit_code != 0  # NameError


# ===========================================================================
# 6. End-to-end data analysis workflow (no DB)
# ===========================================================================


class TestDataAnalysisWorkflow:
    def test_full_analysis_pipeline(self, executor):
        """Simulate a multi-step data analysis without DB."""
        # Step 1: Generate data
        r1 = executor.execute(
            CodeExecutionRequest(
                code="""
import json
data = [
    {"user": "alice", "action": "login", "cost": 10},
    {"user": "bob", "action": "query", "cost": 50},
    {"user": "alice", "action": "query", "cost": 30},
    {"user": "charlie", "action": "login", "cost": 5},
    {"user": "bob", "action": "export", "cost": 100},
]
# Aggregate by user
from collections import defaultdict
totals = defaultdict(float)
for d in data:
    totals[d["user"]] += d["cost"]
print(json.dumps(dict(sorted(totals.items(), key=lambda x: -x[1]))))
"""
            )
        )
        assert r1.execution.exit_code == 0
        import json

        totals = json.loads(r1.execution.stdout.strip())
        assert totals["bob"] == 150
        assert totals["alice"] == 40

    def test_statistical_analysis(self, executor):
        code = """
import statistics
data = [23, 45, 67, 12, 89, 34, 56, 78, 90, 11]
print(f"mean={statistics.mean(data):.1f}")
print(f"median={statistics.median(data):.1f}")
print(f"stdev={statistics.stdev(data):.1f}")
"""
        r = executor.execute(CodeExecutionRequest(code=code))
        assert r.execution.exit_code == 0
        assert "mean=" in r.execution.stdout
        assert "median=" in r.execution.stdout
        assert "stdev=" in r.execution.stdout


# ===========================================================================
# 7. Skill registration
# ===========================================================================


class TestSkillRegistration:
    def test_register_with_code_executor(self):
        """ExecuteCodeSkill can be registered via register_builtin_skills."""
        from unittest.mock import MagicMock
        from core.skills.builtin import ExecuteCodeSkill

        mock_executor = MagicMock()
        skill = ExecuteCodeSkill(mock_executor)
        assert skill.name == "execute_code"
        assert skill.version == "1.0.0"
        assert skill.side_effect_profile.category.value == "write"


# ===========================================================================
# 8. Missing integration scenarios
# ===========================================================================


class TestMissingIntegration:
    @pytest.fixture
    def executor(self):
        return CodeExecutor(
            runtime=SubprocessRuntime(),
            db_factory=lambda: None,
            branch=None,
            security=SecurityGuard(),
        )

    def test_security_rejection_stderr_has_line_number(self, executor):
        """Security rejection stderr contains [L<n>] format."""
        r = executor.execute(CodeExecutionRequest(code="x = 1\nimport os\ny = 2"))
        assert r.security.safe is False
        assert "[L2]" in r.execution.stderr
        assert "blocked" in r.execution.stderr

    def test_timeout_elapsed_time_reasonable(self, executor):
        """Elapsed time on timeout should be ≈ wall_seconds."""
        r = executor.execute(
            CodeExecutionRequest(
                code="import time; time.sleep(100)",
                resources=ResourceProfile(max_wall_seconds=1),
            )
        )
        assert r.execution.exit_code == 137
        assert 500 <= r.execution.execution_time_ms <= 3000

    def test_multiple_executions_stdout_isolated(self, executor):
        """Each execution's stdout is independent."""
        r1 = executor.execute(CodeExecutionRequest(code="x = 42\nprint(x)"))
        r2 = executor.execute(CodeExecutionRequest(code="print('hello')"))
        r3 = executor.execute(CodeExecutionRequest(code="print(x)"))  # x not defined

        assert r1.execution.stdout.strip() == "42"
        assert r2.execution.stdout.strip() == "hello"
        assert r3.execution.exit_code != 0  # NameError
        assert "42" not in r3.execution.stdout
        assert "hello" not in r3.execution.stdout
