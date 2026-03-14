"""Trigger API — create/manage webhook + cron triggers that fire AgentRuns."""

from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel, Field
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user

router = APIRouter(prefix="/triggers")


class CreateTriggerRequest(BaseModel):
    trigger_type: str = Field(description="'webhook' or 'schedule'")
    name: str = Field(description="Human-readable name")
    agent_id: str = Field(default="dev-agent")
    user_input: str = Field(description="Message to send to the agent when fired")
    context: dict | None = None
    cron_expr: str | None = Field(default=None, description="Cron expression (for schedule)")
    session_id: str | None = Field(default=None, description="Reuse existing session")


class WebhookFireRequest(BaseModel):
    secret: str = Field(description="Webhook secret for authentication")
    payload: dict | None = None


@router.post("")
async def create_trigger(
    request: CreateTriggerRequest,
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    from core.agent.triggers import create_trigger

    try:
        return create_trigger(
            db,
            user_id=current_user["user_id"],
            agent_id=request.agent_id,
            trigger_type=request.trigger_type,
            name=request.name,
            user_input=request.user_input,
            context=request.context,
            cron_expr=request.cron_expr,
            session_id=request.session_id,
        )
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))


@router.get("")
async def list_triggers(
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    from core.agent.triggers import list_triggers

    return list_triggers(db, current_user["user_id"])


@router.delete("/{trigger_id}")
async def delete_trigger(
    trigger_id: str,
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    from core.agent.triggers import delete_trigger, get_trigger

    trig = get_trigger(db, trigger_id)
    if not trig:
        raise HTTPException(status_code=404, detail="Trigger not found")
    if trig["user_id"] != current_user["user_id"]:
        raise HTTPException(status_code=403, detail="Not authorized")
    delete_trigger(db, trigger_id)
    return {"trigger_id": trigger_id, "deleted": True}


@router.post("/{trigger_id}/fire")
async def fire_webhook(
    trigger_id: str,
    request: WebhookFireRequest,
    db: Session = Depends(get_db_session),
):
    """Fire a webhook trigger. No auth header needed — uses secret instead."""
    from core.agent.triggers import fire_trigger, get_trigger, verify_secret

    trig = get_trigger(db, trigger_id)
    if not trig:
        raise HTTPException(status_code=404, detail="Trigger not found")
    if trig["trigger_type"] != "webhook":
        raise HTTPException(status_code=400, detail="Not a webhook trigger")
    if not trig.get("secret") or not verify_secret(request.secret, trig["secret"]):
        raise HTTPException(status_code=403, detail="Invalid secret")
    try:
        from api.database import SessionLocal

        return fire_trigger(SessionLocal, trigger_id, payload=request.payload)
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))
