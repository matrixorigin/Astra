"""SubprocessRuntime — default runtime using subprocess + resource limits."""

import os
import platform
import resource
import subprocess
import tempfile
import time
from datetime import datetime, timezone

from core.runtime import ExecutionResult, ResourceProfile, Runtime, RuntimeCapabilities, IsolationLevel

_IS_LINUX = platform.system() == "Linux"


class SubprocessRuntime(Runtime):
    """Execute code in a subprocess with rlimit-based resource control.

    Suitable for dev/demo with trusted code. For untrusted code, use DockerRuntime.
    """

    @property
    def capabilities(self) -> RuntimeCapabilities:
        return RuntimeCapabilities(
            isolation=IsolationLevel.PROCESS,
            network_isolatable=False,
            filesystem_isolated=False,
            resource_limits=_IS_LINUX,  # rlimit only works on Linux
            reproducible=False,
        )

    @property
    def supported_languages(self) -> list[str]:
        return ["python"]

    def health_check(self) -> bool:
        try:
            r = subprocess.run(
                ["python3", "-c", "print('ok')"],
                capture_output=True, timeout=5, text=True,
            )
            return r.returncode == 0
        except Exception:
            return False

    def execute(
        self,
        code: str,
        language: str = "python",
        resources: ResourceProfile | None = None,
        env: dict[str, str] | None = None,
    ) -> ExecutionResult:
        if language not in self.supported_languages:
            return ExecutionResult(
                stdout="", stderr=f"Unsupported language: {language}",
                exit_code=1, execution_time_ms=0,
            )

        resources = resources or ResourceProfile()
        exec_env = {**os.environ, **(env or {})}
        for key in ("PYTHONSTARTUP", "PYTHONPATH"):
            exec_env.pop(key, None)

        with tempfile.TemporaryDirectory(prefix="mo_exec_") as tmpdir:
            code_file = os.path.join(tmpdir, "code.py")
            with open(code_file, "w") as f:
                f.write(code)

            # Reserve 50MB overhead for Python interpreter itself
            _INTERPRETER_OVERHEAD_MB = 50
            mem_bytes = (resources.max_memory_mb + _INTERPRETER_OVERHEAD_MB) * 1024 * 1024
            cpu_seconds = resources.max_cpu_seconds

            def _set_limits():
                # RLIMIT_AS only works on Linux; macOS raises in preexec_fn
                if _IS_LINUX:
                    resource.setrlimit(resource.RLIMIT_AS, (mem_bytes, mem_bytes))
                    # RLIMIT_NPROC is per-UID, not per-process, so it would block
                    # subprocess.run() in executed code. Use RLIMIT_CPU instead.
                resource.setrlimit(resource.RLIMIT_CPU, (cpu_seconds, cpu_seconds))

            start = time.monotonic()
            started_at = datetime.now(timezone.utc)
            try:
                proc = subprocess.run(
                    ["python3", "-u", code_file],
                    capture_output=True,
                    timeout=resources.max_wall_seconds,
                    text=True,
                    cwd=tmpdir,
                    env=exec_env,
                    preexec_fn=_set_limits,
                )
                elapsed_ms = (time.monotonic() - start) * 1000

                stdout = proc.stdout
                truncated = False
                if len(stdout.encode()) > resources.max_output_bytes:
                    stdout = stdout[:resources.max_output_bytes]
                    truncated = True

                return ExecutionResult(
                    stdout=stdout,
                    stderr=proc.stderr,
                    exit_code=proc.returncode,
                    execution_time_ms=round(elapsed_ms, 2),
                    truncated=truncated,
                    started_at=started_at,
                )
            except subprocess.TimeoutExpired:
                elapsed_ms = (time.monotonic() - start) * 1000
                return ExecutionResult(
                    stdout="", stderr=f"Execution timed out after {resources.max_wall_seconds}s",
                    exit_code=137, execution_time_ms=round(elapsed_ms, 2),
                    started_at=started_at,
                )
            except Exception as e:
                elapsed_ms = (time.monotonic() - start) * 1000
                return ExecutionResult(
                    stdout="", stderr=str(e),
                    exit_code=1, execution_time_ms=round(elapsed_ms, 2),
                    started_at=started_at,
                )
