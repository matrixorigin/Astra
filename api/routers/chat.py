"""Chat API endpoints — unified conversation entry point with durable AgentRun."""

import json
from typing import Annotated, Any

from fastapi import APIRouter, Depends, HTTPException, Query
from fastapi.responses import StreamingResponse
from pydantic import BaseModel, Field
from sqlalchemy import text
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user
from core.logging_config import get_logger

logger = get_logger(__name__)
router = APIRouter()


# ---------------------------------------------------------------------------
# Request / Response models
# ---------------------------------------------------------------------------

class ChatRequest(BaseModel):
    """Chat request — session_id optional (auto-created if omitted)."""
    message: str = Field(description="User message")
    session_id: str | None = Field(default=None, description="Session ID (auto-created if omitted)")
    agent_id: str | None = Field(default=None, description="Agent ID")
    model: str | None = Field(default=None, description="Model to use for this request")
    context: dict | None = Field(default=None, description="Optional context")
    max_candidates: int = Field(default=5, description="Max skill candidates")


class ChatTurnRequest(BaseModel):
    """Edge-cloud /chat/turn request — one LLM turn in the agentic loop."""
    messages: list[dict[str, Any]] = Field(description="Conversation messages from edge")
    session_id: str | None = Field(default=None, description="Session ID (auto-created on first turn)")
    tool_results: list[dict[str, Any]] | None = Field(default=None, description="Tool execution results from edge")
    project_rules: str | None = Field(default=None, description="Project rules (sent on first turn)")
    agent_id: str | None = Field(default=None, description="Agent ID")
    model: str | None = Field(default=None, description="Model override")
    edge_tools: list[dict[str, Any]] | None = Field(default=None, description="Edge tool schemas (OpenAI format)")


class ChatResponse(BaseModel):
    """Chat response — always returns run_id."""
    session_id: str
    run_id: str
    status: str


class RunStatusResponse(BaseModel):
    run_id: str
    session_id: str
    status: str
    waiting_for: str | None = None
    events_count: int = 0


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _ensure_session(db: Session, user_id: str, session_id: str | None, agent_id: str | None) -> str:
    """Return existing session_id or create a new one."""
    if session_id:
        row = db.execute(
            text("SELECT session_id FROM sessions WHERE session_id = :sid"),
            {"sid": session_id},
        ).first()
        if not row:
            raise HTTPException(status_code=404, detail="Session not found")
        return session_id

    from core.events.session_manager import SessionManager
    mgr = SessionManager(db)
    session = mgr.create_session(user_id=user_id, metadata={"agent_id": agent_id} if agent_id else None)
    return session.session_id


def _build_chat_loop(db: Session):
    """Build ChatLoop with all dependencies."""
    from core.agent.chat_loop import ChatLoop
    from core.agent.executor import AgentExecutor
    from core.code_executor import CodeExecutor
    from core.context.manager import ContextManager
    from core.events.event_logger import EventLogger
    from core.llm.client import LLMClient
    from core.runtime import IsolationLevel, create_runtime
    from core.skills.builtin import register_builtin_skills
    from core.skills.pipeline import SkillPipeline
    from core.skills.registry import SkillRegistry
    from core.verification.firewall import HallucinationFirewall

    event_logger = EventLogger(db)
    llm_client = LLMClient(db=db)

    # Wire GateTrigger so skill/prompt changes auto-trigger regression gate
    # Disable in tests to avoid DB session conflicts
    import os
    if os.environ.get('DISABLE_GATE_TRIGGER'):
        gate_trigger = None
    else:
        from api.database import SessionLocal as _gate_session_factory
        from core.evaluation.gate_trigger import GateTrigger
        gate_trigger = GateTrigger(db_factory=_gate_session_factory)

    skill_registry = SkillRegistry(db, gate_trigger=gate_trigger)
    code_executor = CodeExecutor(
        runtime=create_runtime(min_isolation=IsolationLevel.PROCESS),
        db=db,
    )
    register_builtin_skills(skill_registry, db, code_executor=code_executor)
    context_manager = ContextManager(db, gate_trigger=gate_trigger)
    selector = SkillPipeline(db, llm_client, audit=True, learning=True)
    selector.reload_skills(registry=skill_registry)

    from config.settings import get_settings
    from core.skills.credential_manager import CredentialManager
    from core.skills.skill_manager import SkillManager
    skill_mgr = SkillManager(db, CredentialManager(get_settings().secret_key))
    executor = AgentExecutor(db, skill_registry, skill_manager=skill_mgr)

    firewall = HallucinationFirewall(db, context_manager)

    loop = ChatLoop(
        selector=selector,
        executor=executor,
        llm_client=llm_client,
        event_logger=event_logger,
        context_manager=context_manager,
        firewall=firewall,
    )

    from core.memory.observer import Observer
    loop.set_observer(Observer(db, llm_client=llm_client))

    return loop


def _get_engine(db: Session):
    from core.agent.run_engine import RunEngine
    return RunEngine(db)


# ---------------------------------------------------------------------------
# Endpoints
# ---------------------------------------------------------------------------

@router.post("/chat", response_model=ChatResponse)
async def chat(
    request: ChatRequest,
    current_user: Annotated[dict, Depends(get_current_user)],
    db: Annotated[Session, Depends(get_db_session)],
):
    """Create an AgentRun. Returns run_id immediately.

    Poll /chat/runs/{run_id} or stream /chat/runs/{run_id}/stream for progress.
    """
    user_id = current_user["user_id"]
    session_id = _ensure_session(db, user_id, request.session_id, request.agent_id)

    engine = _get_engine(db)
    
    # Pass model to context if specified
    context = request.context or {}
    if request.model:
        context["model"] = request.model
    
    run = engine.create_run(
        session_id=session_id,
        user_id=user_id,
        user_input=request.message,
        agent_id=request.agent_id or "dev-agent",  # Pass agent_id for model lookup
        context=context,
    )

    import asyncio
    task = asyncio.create_task(engine.start_run(run))
    from core.agent.run_engine import _run_tasks
    _run_tasks[run.run_id] = task

    return ChatResponse(session_id=session_id, run_id=run.run_id, status=run.status.value)


@router.post("/chat/stream")
async def chat_stream(
    request: ChatRequest,
    current_user: Annotated[dict, Depends(get_current_user)],
    db: Annotated[Session, Depends(get_db_session)],
):
    """Stream chat response as SSE. Returns run_id in first event."""
    user_id = current_user["user_id"]
    session_id = _ensure_session(db, user_id, request.session_id, request.agent_id)

    engine = _get_engine(db)
    
    # Pass model to context if specified
    context = request.context or {}
    if request.model:
        context["model"] = request.model
    
    run = engine.create_run(
        session_id=session_id,
        user_id=user_id,
        user_input=request.message,
        agent_id=request.agent_id or "dev-agent",  # Pass agent_id for model lookup
        context=context,
    )

    import asyncio
    task = asyncio.create_task(engine.start_run(run))
    from core.agent.run_engine import _run_tasks
    _run_tasks[run.run_id] = task

    async def event_generator():
        yield f"data: {json.dumps({'event_type': 'session_info', 'data': {'session_id': session_id, 'run_id': run.run_id}})}\n\n"

        try:
            async for event in engine.stream_run_events(run.run_id):
                yield f"data: {json.dumps(event)}\n\n"
        except Exception as e:
            logger.error(f"Stream error: {e}", exc_info=True)
            yield f"data: {json.dumps({'event_type': 'run_error', 'data': {'error': str(e)}})}\n\n"

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "Connection": "keep-alive", "X-Accel-Buffering": "no"},
    )


@router.get("/chat/runs/{run_id}", response_model=RunStatusResponse)
async def get_run_status(
    run_id: str,
    current_user: Annotated[dict, Depends(get_current_user)],
    db: Annotated[Session, Depends(get_db_session)],
):
    """Get run status and progress."""
    engine = _get_engine(db)
    run = engine.get_run(run_id)
    if not run:
        run = engine.restore_run(run_id)
    if not run:
        raise HTTPException(status_code=404, detail="Run not found")
    if run.user_id != current_user["user_id"]:
        raise HTTPException(status_code=403, detail="Not authorized to view this run")

    events = engine.get_run_events(run_id)
    return RunStatusResponse(
        run_id=run.run_id,
        session_id=run.session_id,
        status=run.status.value,
        waiting_for=run.waiting_for,
        events_count=len(events),
    )


@router.get("/chat/runs/{run_id}/stream")
async def stream_run(
    run_id: str,
    current_user: Annotated[dict, Depends(get_current_user)],
    db: Annotated[Session, Depends(get_db_session)],
    last_index: int = Query(default=0, description="Resume from event index (for reconnection)"),
):
    """Stream run events as SSE. Supports reconnection via last_index."""
    engine = _get_engine(db)
    run = engine.get_run(run_id) or engine.restore_run(run_id)
    if not run:
        raise HTTPException(status_code=404, detail="Run not found")
    if run.user_id != current_user["user_id"]:
        raise HTTPException(status_code=403, detail="Not authorized to view this run")

    async def event_generator():
        async for event in engine.stream_run_events(run_id, last_index=last_index):
            yield f"data: {json.dumps(event)}\n\n"

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "Connection": "keep-alive", "X-Accel-Buffering": "no"},
    )


@router.delete("/chat/runs/{run_id}")
async def cancel_run(
    run_id: str,
    current_user: Annotated[dict, Depends(get_current_user)],
    db: Annotated[Session, Depends(get_db_session)],
):
    """Cancel a running or waiting run."""
    engine = _get_engine(db)
    # Verify ownership
    run = engine.get_run(run_id) or engine.restore_run(run_id)
    if not run:
        raise HTTPException(status_code=404, detail="Run not found")
    if run.user_id != current_user["user_id"]:
        raise HTTPException(status_code=403, detail="Not authorized to cancel this run")
    if not engine.cancel_run(run_id):
        raise HTTPException(status_code=409, detail="Run already finished")
    return {"run_id": run_id, "status": "cancelled"}


# ---------------------------------------------------------------------------
# /chat/turn — Edge-Cloud agentic loop endpoint
# ---------------------------------------------------------------------------

# In-memory conversation history per session (production: persist in MatrixOne)
_turn_histories: dict[str, list[dict[str, Any]]] = {}


def _build_turn_messages(
    session_id: str,
    messages: list[dict[str, Any]],
    tool_results: list[dict[str, Any]] | None,
    project_rules: str | None,
) -> list[dict[str, Any]]:
    """Build LLM messages from edge turn data + server-side history."""
    history = _turn_histories.get(session_id, [])

    # First turn: initialize with system prompt
    if not history:
        system = "You are a development assistant. Use the available tools to help the user."
        if project_rules:
            system += f"\n\n# Project Rules\n{project_rules}"
        history = [{"role": "system", "content": system}]

    # Append new user messages from edge
    for msg in messages:
        if msg.get("role") and msg.get("content"):
            history.append(msg)

    # Append tool results as tool messages (OpenAI format)
    if tool_results:
        for tr in tool_results:
            history.append({
                "role": "tool",
                "tool_call_id": tr["tool_call_id"],
                "content": tr.get("result", ""),
            })

    _turn_histories[session_id] = history
    return history


@router.post("/chat/turn")
async def chat_turn(
    request: ChatTurnRequest,
    current_user: Annotated[dict, Depends(get_current_user)],
    db: Annotated[Session, Depends(get_db_session)],
):
    """One LLM turn in the edge-cloud agentic loop.

    Edge sends messages + tool_results → cloud does context enrichment + LLM call →
    returns SSE stream of text_delta, tool_call, usage, turn_complete events.
    """
    user_id = current_user["user_id"]
    session_id = _ensure_session(db, user_id, request.session_id, request.agent_id)

    # Build conversation messages
    llm_messages = _build_turn_messages(
        session_id, request.messages, request.tool_results, request.project_rules,
    )

    # Resolve tools: use edge_tools if provided, else empty
    tools_schema = request.edge_tools or []

    # Resolve model
    model = request.model

    async def event_generator():
        # Session info (always first)
        yield f"data: {json.dumps({'type': 'session_info', 'session_id': session_id})}\n\n"

        try:
            from core.llm.client import LLMClient
            llm = LLMClient(db=db)

            full_text = ""
            tool_calls: list[dict[str, Any]] = []

            if tools_schema:
                async for chunk in llm.chat_with_tools_stream(
                    llm_messages, tools_schema, model=model,
                ):
                    if chunk["type"] == "text":
                        full_text += chunk["content"]
                        yield f"data: {json.dumps({'type': 'text_delta', 'content': chunk['content']})}\n\n"
                    elif chunk["type"] == "tool_call":
                        tc = chunk["data"]
                        tool_calls.append(tc)
                    elif chunk["type"] == "usage":
                        yield f"data: {json.dumps({'type': 'usage', 'prompt_tokens': chunk.get('prompt', 0), 'completion_tokens': chunk.get('completion', 0), 'cache_read_tokens': chunk.get('cache_read', 0)})}\n\n"
            else:
                async for chunk in llm.chat_stream(
                    llm_messages, user_id, session_id, model=model,
                ):
                    if chunk["type"] == "text":
                        full_text += chunk["content"]
                        yield f"data: {json.dumps({'type': 'text_delta', 'content': chunk['content']})}\n\n"

            # Emit accumulated tool calls
            for tc in tool_calls:
                args = tc.get("function", {}).get("arguments", "{}")
                try:
                    parsed_args = json.loads(args) if isinstance(args, str) else args
                except json.JSONDecodeError:
                    parsed_args = {}
                yield f"data: {json.dumps({'type': 'tool_call', 'id': tc.get('id', ''), 'name': tc.get('function', {}).get('name', ''), 'arguments': parsed_args})}\n\n"

            # Append assistant message to history
            assistant_msg: dict[str, Any] = {"role": "assistant", "content": full_text}
            if tool_calls:
                assistant_msg["tool_calls"] = tool_calls
            _turn_histories.setdefault(session_id, []).append(assistant_msg)

            yield f"data: {json.dumps({'type': 'turn_complete', 'has_tool_calls': len(tool_calls) > 0})}\n\n"

        except Exception as e:
            logger.error(f"chat_turn error: {e}", exc_info=True)
            yield f"data: {json.dumps({'type': 'error', 'message': str(e)})}\n\n"

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "Connection": "keep-alive", "X-Accel-Buffering": "no"},
    )
