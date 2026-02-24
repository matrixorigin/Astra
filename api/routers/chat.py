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
# Cache edge_tools per session so subsequent turns reuse them
_session_tools: dict[str, list[dict[str, Any]]] = {}


def _enrich_system_prompt(
    db: Session,
    user_id: str,
    session_id: str,
    user_query: str,
    project_rules: str | None,
) -> str:
    """Build enriched system prompt with context from cloud services."""
    sections = ["You are a development assistant. Use the available tools to help the user."]

    if project_rules:
        sections.append(f"# Project Rules\n{project_rules}")

    # Context enrichment: memory search + few-shot + observations
    try:
        from core.context.manager import ContextManager
        ctx_mgr = ContextManager(db)
        ctx = ctx_mgr.build_context(session_id=session_id, query=user_query)
        if ctx.selected_events:
            history_lines = []
            for ev in ctx.selected_events[:10]:
                role = "User" if ev.get("event_type") == "user_query" else "Agent"
                content = ev.get("content", "")
                if len(content) > 300:
                    content = content[:300] + "..."
                history_lines.append(f"{role}: {content}")
            if history_lines:
                sections.append("## Relevant Context\n" + "\n".join(history_lines))
    except Exception as e:
        logger.debug(f"Context enrichment skipped: {e}")

    try:
        from core.context.few_shot import FewShotRetriever
        fsr = FewShotRetriever(db)
        examples = fsr.retrieve(user_query)
        few_shot = fsr.format_for_prompt(examples)
        if few_shot:
            sections.append(few_shot)
    except Exception as e:
        logger.debug(f"Few-shot retrieval skipped: {e}")

    try:
        from core.memory.observer import Observer
        from core.llm.client import LLMClient
        obs = Observer(db, llm_client=LLMClient(db=db))
        observations = obs.get_observations(user_id, session_id)
        obs_section = obs.format_for_context(observations)
        if obs_section:
            sections.append(obs_section)
    except Exception as e:
        logger.debug(f"Observer skipped: {e}")

    return "\n\n".join(sections)


def _build_turn_messages(
    db: Session,
    user_id: str,
    session_id: str,
    messages: list[dict[str, Any]],
    tool_results: list[dict[str, Any]] | None,
    project_rules: str | None,
) -> list[dict[str, Any]]:
    """Build LLM messages from edge turn data + server-side history."""
    history = _turn_histories.get(session_id)

    # Recover from DB if not in memory (server restart)
    if history is None:
        history = _recover_history_from_db(db, session_id)

    # First turn: build enriched system prompt
    if not history:
        user_query = next((m.get("content", "") for m in messages if m.get("role") == "user"), "")
        system = _enrich_system_prompt(db, user_id, session_id, user_query, project_rules)
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


def _recover_history_from_db(db: Session, session_id: str) -> list[dict[str, Any]]:
    """Rebuild conversation history from persisted events (for server restart recovery)."""
    try:
        rows = db.execute(
            text("""
                SELECT event_type, content FROM conversation_events
                WHERE session_id = :sid AND event_type IN ('user_query', 'llm_response')
                ORDER BY created_at ASC LIMIT 50
            """),
            {"sid": session_id},
        ).fetchall()
        if not rows:
            return []
        history: list[dict[str, Any]] = [
            {"role": "system", "content": "You are a development assistant. Use the available tools to help the user."}
        ]
        for row in rows:
            etype, content = row[0], row[1] or ""
            if etype == "user_query":
                history.append({"role": "user", "content": content})
            elif etype == "llm_response":
                history.append({"role": "assistant", "content": content})
        return history
    except Exception as e:
        logger.debug(f"History recovery failed: {e}")
        return []


def _persist_turn_events(
    db: Session,
    user_id: str,
    session_id: str,
    messages: list[dict[str, Any]],
    tool_results: list[dict[str, Any]] | None,
    full_text: str,
    tool_calls: list[dict[str, Any]],
) -> str | None:
    """Persist events for this turn: user query, tool results, LLM response."""
    context_capture_id = None
    try:
        from core.events.event_logger import EventLogger
        from uuid_utils import uuid7
        el = EventLogger(db)

        # Persist user query (first user message only)
        user_content = next((m["content"] for m in messages if m.get("role") == "user"), None)
        parent_event_id = None
        causal_chain_id = str(uuid7())  # always generate a chain ID
        if user_content:
            user_ev = el.create_user_query(user_id=user_id, session_id=session_id, content=user_content)
            parent_event_id = user_ev.event_id
            causal_chain_id = user_ev.causal_chain_id

        # Persist tool results from edge (tagged source: "edge")
        if tool_results:
            for tr in tool_results:
                el.create_stream_event(
                    user_id=user_id, session_id=session_id,
                    event_type="tool_result",
                    content=json.dumps({"name": tr.get("name", ""), "result": tr.get("result", "")[:2000]}),
                    parent_event_id=parent_event_id,
                    causal_chain_id=causal_chain_id,
                    metadata={"source": "edge", "tool_call_id": tr.get("tool_call_id")},
                )

        # Persist LLM response
        if full_text or tool_calls:
            tc_names = [tc.get("function", {}).get("name", "") for tc in tool_calls] if tool_calls else []
            response_content = full_text
            if tc_names:
                response_content += f"\n[tool_calls: {', '.join(tc_names)}]"
            el.create_llm_response(
                user_id=user_id, session_id=session_id,
                content=response_content,
                agent_id="dev-agent", agent_version="0.1.0",
                parent_event_id=parent_event_id,
                causal_chain_id=causal_chain_id,
            )

        # Context snapshot + decision audit
        if parent_event_id:
            try:
                from core.context.manager import ContextManager
                ctx_mgr = ContextManager(db)
                ctx = ctx_mgr.build_context(session_id=session_id, query=user_content or "")
                context_capture_id = ctx_mgr.save_snapshot(ctx, session_id, parent_event_id)
            except Exception as e:
                logger.debug(f"Context snapshot skipped: {e}")

    except Exception as e:
        logger.warning(f"Event persistence failed (non-fatal): {e}")

    return context_capture_id


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

    # Cache edge_tools on first turn, reuse on subsequent turns
    if request.edge_tools:
        _session_tools[session_id] = request.edge_tools
    tools_schema = _session_tools.get(session_id, [])

    # Build conversation messages with context enrichment
    llm_messages = _build_turn_messages(
        db, user_id, session_id,
        request.messages, request.tool_results, request.project_rules,
    )

    model = request.model

    async def event_generator():
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
                        tool_calls.append(chunk["data"])
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

            # Persist events (non-blocking, best-effort)
            _persist_turn_events(
                db, user_id, session_id,
                request.messages, request.tool_results,
                full_text, tool_calls,
            )

            yield f"data: {json.dumps({'type': 'turn_complete', 'has_tool_calls': len(tool_calls) > 0})}\n\n"

        except Exception as e:
            logger.error(f"chat_turn error: {e}", exc_info=True)
            yield f"data: {json.dumps({'type': 'error', 'message': str(e)})}\n\n"

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "Connection": "keep-alive", "X-Accel-Buffering": "no"},
    )
