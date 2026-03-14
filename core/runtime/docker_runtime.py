"""Docker-based code execution runtime.

Runs untrusted code inside ephemeral containers with strict resource limits.
Implements the same Runtime ABC as SubprocessRuntime.
"""

from __future__ import annotations

import json
import logging
import time
from datetime import datetime, timezone

import docker
from docker.errors import ContainerError, ImageNotFound, APIError

from core.runtime import (
    ExecutionResult,
    ResourceProfile,
    Runtime,
    RuntimeCapabilities,
    IsolationLevel,
)

logger = logging.getLogger(__name__)

DEFAULT_IMAGE = "python:3.11-slim"
_NO_NETWORK = "none"


class DockerRuntime(Runtime):
    """Execute code in ephemeral Docker containers.

    Each execution creates a fresh container that is removed after completion.
    No filesystem state persists between executions.
    """

    def __init__(self, image: str = DEFAULT_IMAGE):
        self.image = image
        self._client: docker.DockerClient | None = None

    @property
    def client(self) -> docker.DockerClient:
        if self._client is None:
            self._client = docker.from_env()
        return self._client

    @property
    def capabilities(self) -> RuntimeCapabilities:
        return RuntimeCapabilities(
            isolation=IsolationLevel.CONTAINER,
            network_isolatable=True,
            filesystem_isolated=True,
            resource_limits=True,
            reproducible=True,
        )

    @property
    def supported_languages(self) -> list[str]:
        return ["python"]

    def health_check(self) -> bool:
        try:
            self.client.ping()
            return True
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
                stdout="",
                stderr=f"Unsupported language: {language}",
                exit_code=1,
                execution_time_ms=0,
            )

        resources = resources or ResourceProfile()
        mem_limit = f"{resources.max_memory_mb}m"
        network_mode = "bridge" if resources.network_enabled else _NO_NETWORK

        # Wrapper: write code to tmp, exec with timeout
        wrapper = json.dumps(code)
        cmd = [
            "python3",
            "-c",
            f"import signal,sys; signal.alarm({resources.max_cpu_seconds}); exec({wrapper})",
        ]

        start = time.monotonic()
        started_at = datetime.now(timezone.utc)

        try:
            container = self.client.containers.run(
                self.image,
                cmd,
                detach=True,
                mem_limit=mem_limit,
                memswap_limit=mem_limit,  # no swap
                cpu_period=100_000,
                cpu_quota=100_000,  # 1 CPU
                network_mode=network_mode,
                pids_limit=64,
                read_only=True,
                tmpfs={"/tmp": "size=64m"},
                environment=env or {},
                # Security: drop all capabilities, no new privileges
                cap_drop=["ALL"],
                security_opt=["no-new-privileges"],
            )

            try:
                result = container.wait(timeout=resources.max_wall_seconds)
                exit_code = result.get("StatusCode", 1)
                stdout = container.logs(stdout=True, stderr=False).decode(errors="replace")
                stderr = container.logs(stdout=False, stderr=True).decode(errors="replace")
            except Exception:
                # Timeout — kill container
                try:
                    container.kill()
                except Exception:
                    pass
                elapsed_ms = (time.monotonic() - start) * 1000
                return ExecutionResult(
                    stdout="",
                    stderr=f"Execution timed out after {resources.max_wall_seconds}s",
                    exit_code=137,
                    execution_time_ms=round(elapsed_ms, 2),
                    started_at=started_at,
                )
            finally:
                try:
                    container.remove(force=True)
                except Exception:
                    pass

            elapsed_ms = (time.monotonic() - start) * 1000

            truncated = False
            if len(stdout.encode()) > resources.max_output_bytes:
                stdout = stdout[: resources.max_output_bytes]
                truncated = True

            return ExecutionResult(
                stdout=stdout,
                stderr=stderr,
                exit_code=exit_code,
                execution_time_ms=round(elapsed_ms, 2),
                truncated=truncated,
                started_at=started_at,
            )

        except ImageNotFound:
            logger.info(f"Pulling image {self.image}...")
            try:
                self.client.images.pull(self.image)
                return self.execute(code, language, resources, env)
            except Exception as e:
                return ExecutionResult(
                    stdout="",
                    stderr=f"Failed to pull image: {e}",
                    exit_code=1,
                    execution_time_ms=0,
                )
        except APIError as e:
            elapsed_ms = (time.monotonic() - start) * 1000
            return ExecutionResult(
                stdout="",
                stderr=f"Docker API error: {e}",
                exit_code=1,
                execution_time_ms=round(elapsed_ms, 2),
                started_at=started_at,
            )
