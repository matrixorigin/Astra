"""Background job API endpoints."""

from typing import Annotated

from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel, Field

from api.dependencies import get_current_user
from core.jobs.backend import JobRequirements, JobStatus
from core.jobs.router import JobRouter

router = APIRouter()
_router = JobRouter()


class JobRequest(BaseModel):
    job_type: str = Field(description="Registered job type (e.g. feedback_trainer)")
    inputs: dict = Field(default_factory=dict)
    gpu_required: bool = False
    timeout_seconds: int = 3600
    conda_env: str | None = None


class JobResponse(BaseModel):
    job_id: str
    status: str
    result: dict | None = None
    error: str | None = None
    progress: float = 0.0


class JobCompletionWebhook(BaseModel):
    job_id: str
    status: str
    result: dict | None = None
    error: str | None = None


@router.post("/jobs", response_model=JobResponse)
async def submit_job(
    request: JobRequest,
    current_user: Annotated[dict, Depends(get_current_user)],
):
    req = JobRequirements(
        gpu_required=request.gpu_required,
        timeout_seconds=request.timeout_seconds,
        conda_env=request.conda_env,
    )
    backend = _router.select(req)
    job_id = await backend.submit(request.job_type, request.inputs, req)
    return JobResponse(job_id=job_id, status=JobStatus.PENDING)


@router.get("/jobs/{job_id}", response_model=JobResponse)
async def get_job(
    job_id: str,
    current_user: Annotated[dict, Depends(get_current_user)],
):
    backend = _router.backends["local"]  # TODO: store job→backend mapping
    try:
        result = await backend.get_status(job_id)
    except KeyError:
        raise HTTPException(status_code=404, detail="Job not found")
    return JobResponse(
        job_id=result.job_id,
        status=result.status,
        result=result.result,
        error=result.error,
        progress=result.progress,
    )


@router.delete("/jobs/{job_id}")
async def cancel_job(
    job_id: str,
    current_user: Annotated[dict, Depends(get_current_user)],
):
    backend = _router.backends["local"]
    try:
        result = await backend.get_status(job_id)
    except KeyError:
        raise HTTPException(status_code=404, detail="Job not found")
    if result.status in (JobStatus.COMPLETED, JobStatus.FAILED, JobStatus.CANCELLED):
        raise HTTPException(status_code=409, detail=f"Job already {result.status.value}")
    cancelled = await backend.cancel(job_id)
    if not cancelled:
        # Race: job finished between get_status and cancel
        raise HTTPException(status_code=409, detail="Job already finished")
    return {"job_id": job_id, "status": "cancelled"}


@router.post("/jobs/webhook")
async def job_completion_webhook(
    payload: JobCompletionWebhook,
):
    """Webhook called when a job completes. Resumes the waiting agent run."""
    from api.database import SessionLocal
    from core.agent.run_engine import RunEngine
    engine = RunEngine(SessionLocal)
    result = payload.result or {}
    if payload.error:
        result["error"] = payload.error
    resumed = await engine.on_job_completed(payload.job_id, result)
    return {"resumed": resumed, "job_id": payload.job_id}
