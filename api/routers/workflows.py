"""Workflow API endpoints."""

from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel, Field
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user
from api.models import WorkflowDefinition, WorkflowRun

router = APIRouter()


class WorkflowDefResponse(BaseModel):
    workflow_id: str
    name: str
    version: str
    description: str | None = None
    definition: dict
    is_active: bool = True


class WorkflowRunResponse(BaseModel):
    run_id: str
    workflow_id: str
    agent_run_id: str | None = None
    status: str
    waiting_for: str | None = None
    current_step_idx: int = 0
    step_results: dict = Field(default_factory=dict)
    error: str | None = None


@router.get("/workflows", response_model=list[WorkflowDefResponse])
def list_workflows(
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    rows = db.query(WorkflowDefinition).filter(WorkflowDefinition.is_active == 1).all()
    return [WorkflowDefResponse(
        workflow_id=r.workflow_id, name=r.name, version=r.version,
        description=r.description, definition=r.definition, is_active=bool(r.is_active),
    ) for r in rows]


@router.get("/workflows/runs/{run_id}", response_model=WorkflowRunResponse)
def get_workflow_run(
    run_id: str,
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    row = db.query(WorkflowRun).filter(WorkflowRun.run_id == run_id).first()
    if not row:
        raise HTTPException(status_code=404, detail="Workflow run not found")
    return WorkflowRunResponse(
        run_id=row.run_id, workflow_id=row.workflow_id,
        agent_run_id=row.agent_run_id, status=row.status,
        waiting_for=row.waiting_for, current_step_idx=row.current_step_idx,
        step_results=row.step_results or {}, error=row.error,
    )


@router.post("/workflows/runs/{run_id}/resolve")
async def resolve_workflow_wait(
    run_id: str,
    result: dict,
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    """Resolve a workflow's wait step (e.g. human approval)."""
    row = db.query(WorkflowRun).filter(WorkflowRun.run_id == run_id).first()
    if not row or row.status != "waiting":
        raise HTTPException(status_code=404, detail="Workflow run not found or not waiting")
    handle = row.waiting_for

    if not handle:
        raise HTTPException(status_code=400, detail="No wait handle")

    from core.agent.async_tools import resume_workflow
    resumed = await resume_workflow(handle, result)
    if not resumed:
        raise HTTPException(status_code=409, detail="Could not resume workflow")
    return {"run_id": run_id, "status": "resumed"}
