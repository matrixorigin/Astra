"""Job backend ABC and shared types."""

from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from enum import Enum


class JobStatus(str, Enum):
    PENDING = "pending"
    RUNNING = "running"
    COMPLETED = "completed"
    FAILED = "failed"
    CANCELLED = "cancelled"


@dataclass
class JobResult:
    job_id: str
    status: JobStatus
    result: dict | None = None
    error: str | None = None
    progress: float = 0.0


@dataclass
class JobRequirements:
    """Resource requirements for a background job."""

    gpu_required: bool = False
    min_cpus: int = 1
    min_memory_gb: float = 2.0
    timeout_seconds: int = 3600
    conda_env: str | None = None
    env_vars: dict[str, str] = field(default_factory=dict)


class JobBackend(ABC):
    """Abstract backend for background job execution."""

    @abstractmethod
    async def submit(self, job_type: str, inputs: dict, requirements: JobRequirements) -> str:
        """Submit job, return job_id."""

    @abstractmethod
    async def get_status(self, job_id: str) -> JobResult:
        """Get job status and result. Raises KeyError if job_id unknown."""

    @abstractmethod
    async def cancel(self, job_id: str) -> bool:
        """Cancel a running job. Returns True if cancelled, False if already finished.
        Raises KeyError if job_id unknown."""

    @abstractmethod
    async def wait(self, job_id: str, timeout: float | None = None) -> JobResult:
        """Wait for job completion."""

    async def shutdown(self) -> None:
        """Graceful shutdown — cancel running tasks, wait for cleanup."""
