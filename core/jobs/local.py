"""Local job backend — runs jobs as subprocesses."""

import asyncio
import json
import sys
from uuid import uuid4

from core.jobs.backend import JobBackend, JobRequirements, JobResult, JobStatus
from core.logging_config import get_logger

logger = get_logger(__name__)


class LocalJobBackend(JobBackend):
    """Execute background jobs as local subprocesses."""

    def __init__(self) -> None:
        self._tasks: dict[str, asyncio.Task] = {}
        self._results: dict[str, JobResult] = {}

    async def submit(self, job_type: str, inputs: dict, requirements: JobRequirements) -> str:
        job_id = str(uuid4())
        self._results[job_id] = JobResult(job_id=job_id, status=JobStatus.PENDING)
        self._tasks[job_id] = asyncio.create_task(self._run(job_id, job_type, inputs, requirements))
        return job_id

    async def get_status(self, job_id: str) -> JobResult:
        if job_id not in self._results:
            return JobResult(job_id=job_id, status=JobStatus.FAILED, error="Unknown job")
        return self._results[job_id]

    async def cancel(self, job_id: str) -> bool:
        task = self._tasks.get(job_id)
        if task and not task.done():
            task.cancel()
            self._results[job_id] = JobResult(job_id=job_id, status=JobStatus.CANCELLED)
            return True
        return False

    async def wait(self, job_id: str, timeout: float | None = None) -> JobResult:
        task = self._tasks.get(job_id)
        if not task:
            return await self.get_status(job_id)
        try:
            await asyncio.wait_for(asyncio.shield(task), timeout=timeout)
        except asyncio.TimeoutError:
            pass
        return self._results[job_id]

    async def _run(self, job_id: str, job_type: str, inputs: dict, req: JobRequirements) -> None:
        self._results[job_id] = JobResult(job_id=job_id, status=JobStatus.RUNNING)
        try:
            cmd = self._build_cmd(job_type, inputs, req)
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            try:
                stdout, stderr = await asyncio.wait_for(
                    proc.communicate(), timeout=req.timeout_seconds
                )
            except asyncio.TimeoutError:
                proc.kill()
                await proc.communicate()
                self._results[job_id] = JobResult(
                    job_id=job_id, status=JobStatus.FAILED, error="Timeout"
                )
                return

            if proc.returncode != 0:
                self._results[job_id] = JobResult(
                    job_id=job_id, status=JobStatus.FAILED, error=stderr.decode()[-2000:]
                )
            else:
                try:
                    result = json.loads(stdout.decode())
                except (json.JSONDecodeError, UnicodeDecodeError):
                    result = {"output": stdout.decode()[-2000:]}
                self._results[job_id] = JobResult(
                    job_id=job_id, status=JobStatus.COMPLETED, result=result, progress=1.0
                )
        except asyncio.CancelledError:
            self._results[job_id] = JobResult(job_id=job_id, status=JobStatus.CANCELLED)
        except Exception as e:
            logger.error(f"Job {job_id} failed: {e}")
            self._results[job_id] = JobResult(
                job_id=job_id, status=JobStatus.FAILED, error=str(e)
            )

    @staticmethod
    def _build_cmd(job_type: str, inputs: dict, req: JobRequirements) -> list[str]:
        runner_args = [
            sys.executable, "-m", "core.jobs.runner",
            "--job-type", job_type,
            "--inputs", json.dumps(inputs),
        ]
        if req.conda_env:
            return ["conda", "run", "-n", req.conda_env, "--no-capture-output"] + runner_args
        return runner_args
