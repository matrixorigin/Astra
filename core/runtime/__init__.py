"""Code execution runtime — isolated environments that run code."""

from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum


class IsolationLevel(str, Enum):
    """How strongly the runtime isolates code from the host."""

    NONE = "none"  # No isolation (e.g. eval in-process)
    PROCESS = "process"  # Separate process, rlimit (SubprocessRuntime)
    CONTAINER = "container"  # Docker container
    MICROVM = "microvm"  # Firecracker / gVisor


@dataclass(frozen=True)
class RuntimeCapabilities:
    """Self-describing capabilities of a runtime.

    Upper layers (CodeExecutor, SecurityGuard) use this to make decisions:
    - Skip AST security checks if isolation >= container
    - Reject network-dependent code if network_isolatable is False
    - Warn user if filesystem_isolated is False
    """

    isolation: IsolationLevel
    network_isolatable: bool  # Can disable network per-execution?
    filesystem_isolated: bool  # Code cannot access host filesystem?
    resource_limits: bool  # Enforces memory/CPU limits?
    reproducible: bool  # Same code + env → same result? (no host state leakage)


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
PROFILE_DATA_ANALYSIS = ResourceProfile(
    max_memory_mb=1024, max_cpu_seconds=60, max_wall_seconds=120
)


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

    @property
    @abstractmethod
    def capabilities(self) -> RuntimeCapabilities: ...

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


def create_runtime(
    *,
    min_isolation: IsolationLevel = IsolationLevel.PROCESS,
    require_network_isolation: bool = False,
    image: str | None = None,
) -> Runtime:
    """Create the best available runtime matching the requested capabilities.

    Selection order (strongest first): Firecracker → Docker → Subprocess.
    Raises RuntimeError if no runtime satisfies the constraints.
    """
    # Try Firecracker (strongest isolation)
    try:
        from core.runtime.firecracker_runtime import FirecrackerRuntime

        rt = FirecrackerRuntime()
        if rt.health_check() and _satisfies(rt, min_isolation, require_network_isolation):
            return rt
    except Exception:
        pass

    # Try Docker
    if min_isolation <= IsolationLevel.CONTAINER:
        try:
            from core.runtime.docker_runtime import DockerRuntime

            rt = DockerRuntime(image=image or "python:3.11-slim")
            if rt.health_check() and _satisfies(rt, min_isolation, require_network_isolation):
                return rt
        except Exception:
            pass

    # Try Subprocess
    if min_isolation <= IsolationLevel.PROCESS:
        from core.runtime.subprocess_runtime import SubprocessRuntime

        rt = SubprocessRuntime()
        if _satisfies(rt, min_isolation, require_network_isolation):
            return rt

    raise RuntimeError(
        f"No runtime available for isolation={min_isolation.value}, "
        f"network_isolation={require_network_isolation}"
    )


def _satisfies(rt: Runtime, min_iso: IsolationLevel, need_net_iso: bool) -> bool:
    cap = rt.capabilities
    if _iso_rank(cap.isolation) < _iso_rank(min_iso):
        return False
    if need_net_iso and not cap.network_isolatable:
        return False
    return True


_ISO_ORDER = {
    IsolationLevel.NONE: 0,
    IsolationLevel.PROCESS: 1,
    IsolationLevel.CONTAINER: 2,
    IsolationLevel.MICROVM: 3,
}


def _iso_rank(level: IsolationLevel) -> int:
    return _ISO_ORDER.get(level, 0)
