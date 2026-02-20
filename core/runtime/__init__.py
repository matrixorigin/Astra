"""Code execution runtime — isolated environments that run code."""

from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from datetime import datetime


@dataclass
class ResourceProfile:
    """Resource limits for code execution."""
    max_memory_mb: int = 256
    max_cpu_seconds: int = 30
    max_wall_seconds: int = 60
    max_output_bytes: int = 1_048_576  # 1MB
    network_enabled: bool = False


# Named profiles for common use cases
PROFILE_LIGHTWEIGHT = ResourceProfile(max_memory_mb=64, max_cpu_seconds=5, max_wall_seconds=10)
PROFILE_DATA_ANALYSIS = ResourceProfile(max_memory_mb=1024, max_cpu_seconds=60, max_wall_seconds=120)


@dataclass
class ExecutionResult:
    """Result of a code execution."""
    stdout: str
    stderr: str
    exit_code: int
    execution_time_ms: float
    truncated: bool = False  # True if stdout hit max_output_bytes
    started_at: datetime | None = None  # UTC timestamp when execution began (for PITR)


class Runtime(ABC):
    """ABC for code execution runtimes.

    A runtime is an isolated environment that takes code + env vars + resource limits,
    runs it, returns stdout/stderr/exit_code. It knows nothing about data, security,
    or orchestration.
    """

    @abstractmethod
    def execute(
        self,
        code: str,
        language: str,
        resources: ResourceProfile = field(default_factory=ResourceProfile),
        env: dict[str, str] | None = None,
    ) -> ExecutionResult: ...

    @abstractmethod
    def health_check(self) -> bool: ...

    @property
    @abstractmethod
    def supported_languages(self) -> list[str]: ...
