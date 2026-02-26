"""Chat API endpoints — unified conversation entry point with durable AgentRun."""

import json
import threading
from collections import OrderedDict
from typing import Annotated, Any

from fastapi import APIRouter, Depends, HTTPException, Query
from fastapi.responses import StreamingResponse
from pydantic import BaseModel, ConfigDict, Field
from sqlalchemy import text
from sqlalchemy.exc import SQLAlchemyError
from sqlalchemy.orm import Session

from api.database import SessionLocal
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


class EdgeProfileModel(BaseModel):
    """Validated edge profile schema."""
    # extra="ignore" for forward compatibility: older servers accept newer edge fields.
    # Trade-off: typos like "git_brach" are silently ignored. Mitigated by edge-side
    # validation in detect_edge_profile() which constructs the dict programmatically.
    model_config = ConfigDict(extra="ignore")

    cwd: str | None = None
    git_branch: str | None = None
    project_type: str | None = None
    languages: list[str] | None = None


class ChatTurnRequest(BaseModel):
    """Edge-cloud /chat/turn request — one LLM turn in the agentic loop."""
    messages: list[dict[str, Any]] = Field(description="Conversation messages from edge")
    session_id: str | None = Field(default=None, description="Session ID (auto-created on first turn)")
    tool_results: list[dict[str, Any]] | None = Field(default=None, description="Tool execution results from edge")
    project_rules: str | None = Field(default=None, description="Project rules (sent on first turn)")
    agent_id: str | None = Field(default=None, description="Agent ID")
    model: str | None = Field(default=None, description="Model override")
    edge_tools: list[dict[str, Any]] | None = Field(default=None, description="Edge tool schemas (OpenAI format)")
    edge_profile: EdgeProfileModel | None = Field(default=None, description="Edge project profile (cwd, git_branch, languages, project_type)")


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


def _build_chat_loop(db_factory):
    """Build ChatLoop with all dependencies.

    Accepts a db_factory (Callable → Session). All components receive the
    factory and create their own short-lived sessions.
    """
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

    # Create EventPipeline for async writes (feature-flagged)
    pipeline = None
    try:
        from core.events.pipeline import EventPipeline
        from core.events.event_logger import _PIPELINE_ENABLED
        if _PIPELINE_ENABLED:
            pipeline = EventPipeline(db_factory)
            pipeline.start()
    except Exception:
        pass

    event_logger = EventLogger(db_factory, pipeline=pipeline)
    llm_client = LLMClient(db_factory=db_factory)

    # Wire GateTrigger so skill/prompt changes auto-trigger regression gate
    # Disable in tests to avoid DB session conflicts
    import os
    if os.environ.get('DISABLE_GATE_TRIGGER'):
        gate_trigger = None
    else:
        from core.evaluation.gate_trigger import GateTrigger
        gate_trigger = GateTrigger(db_factory=db_factory)

    skill_registry = SkillRegistry(db_factory, gate_trigger=gate_trigger)
    code_executor = CodeExecutor(
        runtime=create_runtime(min_isolation=IsolationLevel.PROCESS),
        db_factory=db_factory,
    )
    register_builtin_skills(skill_registry, db_factory, code_executor=code_executor)
    context_manager = ContextManager(db_factory, gate_trigger=gate_trigger)
    selector = SkillPipeline(db_factory, llm_client, audit=True, learning=True)
    selector.reload_skills(registry=skill_registry)

    from config.settings import get_settings
    from core.skills.credential_manager import CredentialManager
    from core.skills.skill_manager import SkillManager
    skill_mgr = SkillManager(db_factory, CredentialManager(get_settings().secret_key))
    executor = AgentExecutor(db_factory, skill_registry, skill_manager=skill_mgr)

    firewall = HallucinationFirewall(db_factory, context_manager)

    loop = ChatLoop(
        selector=selector,
        executor=executor,
        llm_client=llm_client,
        event_logger=event_logger,
        context_manager=context_manager,
        firewall=firewall,
    )

    from core.memory.observer import Observer
    loop.set_observer(Observer(db_factory, llm_client=llm_client))

    return loop


def _get_engine():
    from api.database import SessionLocal
    from core.agent.run_engine import RunEngine
    return RunEngine(SessionLocal, chat_loop_factory=_build_chat_loop)


# ---------------------------------------------------------------------------
# Endpoints
# ---------------------------------------------------------------------------

@router.post("/chat", response_model=ChatResponse)
async def chat(
    request: ChatRequest,
    current_user: Annotated[dict, Depends(get_current_user)],
):
    """Create an AgentRun. Returns run_id immediately.

    Poll /chat/runs/{run_id} or stream /chat/runs/{run_id}/stream for progress.
    """
    user_id = current_user["user_id"]
    db = SessionLocal()
    try:
        session_id = _ensure_session(db, user_id, request.session_id, request.agent_id)
    finally:
        db.close()

    engine = _get_engine()
    
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
):
    """Stream chat response as SSE. Returns run_id in first event."""
    user_id = current_user["user_id"]
    db = SessionLocal()
    try:
        session_id = _ensure_session(db, user_id, request.session_id, request.agent_id)
    finally:
        db.close()

    engine = _get_engine()
    
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
):
    """Get run status and progress."""
    engine = _get_engine()
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
    last_index: int = Query(default=0, description="Resume from event index (for reconnection)"),
):
    """Stream run events as SSE. Supports reconnection via last_index."""
    engine = _get_engine()
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
):
    """Cancel a running or waiting run."""
    engine = _get_engine()
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

# In-memory conversation history per session (production: persist in MatrixOne).
# Uses LRU eviction to prevent unbounded growth on long-running servers.
# 1000 sessions ≈ 50-100MB RAM (each session is a list of message dicts).
# This is a structural limit, not deployment config — changing it affects
# memory footprint and eviction behavior, requiring load-test validation.
_MAX_CACHED_SESSIONS = 1000


class _LRUDict(OrderedDict):
    """Thread-safe OrderedDict with LRU eviction at a fixed capacity.

    FastAPI dispatches sync endpoints to a thread pool, so concurrent access
    to module-level dicts is possible.  All public operations are serialized
    under a reentrant lock.  We use RLock (not Lock) because __setitem__
    calls __contains__ internally while already holding the lock.
    """

    def __init__(self, maxsize: int):
        super().__init__()
        self._maxsize = maxsize
        self._lock = threading.RLock()

    def __contains__(self, key):
        # Intentionally does NOT call move_to_end(): existence checks should not
        # count as "access" for LRU purposes.  __setitem__ relies on this —
        # checking `if key in self` before overwrite must not refresh the entry.
        with self._lock:
            return super().__contains__(key)

    def __setitem__(self, key, value):
        with self._lock:
            if key in self:
                self.move_to_end(key)
            super().__setitem__(key, value)
            if len(self) > self._maxsize:
                self.popitem(last=False)

    def __getitem__(self, key):
        with self._lock:
            self.move_to_end(key)
            return super().__getitem__(key)

    def get(self, key, default=None):
        with self._lock:
            if key in self:
                self.move_to_end(key)
                return super().__getitem__(key)
            return default

    def setdefault(self, key, default=None):
        with self._lock:
            if key in self:
                self.move_to_end(key)
                return super().__getitem__(key)
            self[key] = default
            return default

    def pop(self, key, *args):
        with self._lock:
            return super().pop(key, *args)

    def __delitem__(self, key):
        with self._lock:
            super().__delitem__(key)

    def clear(self):
        with self._lock:
            super().clear()


_turn_histories: _LRUDict = _LRUDict(_MAX_CACHED_SESSIONS)
# Cache edge_tools per session so subsequent turns reuse them
_session_tools: _LRUDict = _LRUDict(_MAX_CACHED_SESSIONS)


def _tool_names(tools: list[dict[str, Any]]) -> set[str]:
    """Extract tool names from OpenAI-format tool schemas for change detection."""
    return {t.get("function", {}).get("name", "") for t in tools}


def _build_turn_messages(
    db: Session,
    user_id: str,
    session_id: str,
    messages: list[dict[str, Any]],
    tool_results: list[dict[str, Any]] | None,
    project_rules: str | None,
    agent_id: str | None = None,
    edge_tools: list[dict[str, Any]] | None = None,
    edge_profile: dict[str, Any] | None = None,
    force_rebuild_system: bool = False,
    username: str | None = None,
) -> list[dict[str, Any]]:
    """Build LLM messages from edge turn data + server-side history.

    When force_rebuild_system=True (mid-session tool change), only the system
    message (history[0]) is replaced — the rest of the conversation is preserved.
    """
    history = _turn_histories.get(session_id)

    # Recover from DB if not in memory (server restart)
    if history is None:
        history = _recover_history_from_db(db, user_id, session_id, agent_id)

    # Rebuild system prompt: either first turn (empty history) or forced by tool change.
    # On force_rebuild_system we replace history[0] in-place, preserving conversation.
    # NOTE: on mid-session rebuild, project_rules and edge_profile may be None (only
    # sent on turn 0). The rebuilt prompt will have updated tool info but stale/missing
    # project context. This is acceptable — the Self-Model tool section is the primary
    # reason for rebuild, and project context doesn't change mid-session.
    if not history or force_rebuild_system:
        user_query = next((m.get("content", "") for m in messages if m.get("role") == "user"), "")

        from core.context.prompt_assembler import PromptAssembler, EdgeContext
        edge_ctx = EdgeContext(
            project_rules=project_rules,
            edge_tools=edge_tools or [],
            edge_profile=edge_profile or {},
        )
        assembled = PromptAssembler(SessionLocal).assemble(
            agent_id=agent_id,
            user_query=user_query,
            session_id=session_id,
            user_id=user_id,
            edge_context=edge_ctx,
            username=username,
        )
        system = assembled.system_message
        logger.debug("Assembled prompt: %d tokens, snapshot=%s", sum(assembled.token_breakdown.values()), assembled.snapshot_id)

        if history and force_rebuild_system:
            # Replace system message, keep conversation history intact
            history[0] = {"role": "system", "content": system}
        else:
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


# Max conversation events to recover on server restart.
# Inlined into SQL via f-string (not parameterized) because MySQL-compatible DBs
# (including MatrixOne) may quote parameterized LIMIT values as strings:
# `LIMIT '50'` → syntax error. SQLAlchemy bindparam() has the same issue.
# Safe: this is a module-level int constant, not user input.
_MAX_RECOVERY_EVENTS = 50


def _recover_history_from_db(db: Session, user_id: str, session_id: str, agent_id: str | None = None) -> list[dict[str, Any]]:
    """Rebuild conversation history from persisted events (for server restart recovery)."""
    try:
        rows = db.execute(
            # safe: _MAX_RECOVERY_EVENTS is a module-level int constant, not user input
            text(f"""
                SELECT event_type, content FROM conversation_events
                WHERE session_id = :sid AND event_type IN ('user_query', 'llm_response')
                ORDER BY created_at ASC LIMIT {_MAX_RECOVERY_EVENTS}
            """),
            {"sid": session_id},
        ).fetchall()
        if not rows:
            return []

        # Rebuild system prompt via assembler.
        # NOTE: edge_context is not available on recovery (project_rules, edge_profile
        # are transient and not persisted). The recovered prompt will lack Self-Model
        # edge tool info and project context. This is an accepted limitation — the
        # edge will re-send these on the next fresh session.
        first_query = next((r[1] for r in rows if r[0] == "user_query"), "")
        from core.context.prompt_assembler import PromptAssembler
        assembled = PromptAssembler(SessionLocal).assemble(
            agent_id=agent_id, user_query=first_query,
            session_id=session_id, user_id=user_id,
        )
        history: list[dict[str, Any]] = [
            {"role": "system", "content": assembled.system_message}
        ]
        for row in rows:
            etype, content = row[0], row[1] or ""
            if etype == "user_query":
                history.append({"role": "user", "content": content})
            elif etype == "llm_response":
                history.append({"role": "assistant", "content": content})
        return history
    except SQLAlchemyError as e:
        logger.debug("History recovery failed: %s", e)
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
    """Persist events for this turn: user query, tool results, LLM response.

    Also writes decision audit, skill selection, observations, and implicit feedback
    via TurnHooks. All writes are best-effort — failures are logged but never block.

    Context snapshot is NOT saved here — it is saved BEFORE the LLM call by
    PromptAssembler (the correct timing). This fixes the duplicate-snapshot bug.
    """
    from uuid_utils import uuid7

    context_capture_id = None
    user_content = next((m["content"] for m in messages if m.get("role") == "user"), None)
    tc_names = [tc.get("function", {}).get("name", "") for tc in tool_calls] if tool_calls else []

    try:
        from core.events.event_logger import EventLogger
        el = EventLogger(SessionLocal)

        # Persist user query
        parent_event_id = None
        causal_chain_id = str(uuid7())
        if user_content:
            user_ev = el.create_user_query(user_id=user_id, session_id=session_id, content=user_content)
            parent_event_id = user_ev.event_id
            causal_chain_id = user_ev.causal_chain_id

        # Persist tool results from edge
        if tool_results:
            for tr in tool_results:
                meta = {"source": "edge", "tool_call_id": tr.get("tool_call_id")}
                if tr.get("name") == "get_agent_info":
                    meta["introspection"] = True
                el.create_stream_event(
                    user_id=user_id, session_id=session_id,
                    event_type="tool_result",
                    content=json.dumps({"name": tr.get("name", ""), "result": tr.get("result", "")[:2000]}),
                    parent_event_id=parent_event_id,
                    causal_chain_id=causal_chain_id,
                    metadata=meta,
                )

        # Persist LLM response
        if full_text or tool_calls:
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

        # Post-turn hooks via TurnHooks (decision audit, skill selection, observer, feedback)
        if parent_event_id:
            from core.agent.turn_hooks import TurnHooks
            hooks = TurnHooks(SessionLocal, llm_client=_get_shared_llm_client())
            hooks.record_decision_audit(session_id, parent_event_id, tool_calls, full_text, context_capture_id)
            hooks.record_skill_selection(session_id, user_content or "", tool_calls)

        if user_content:
            from core.agent.turn_hooks import TurnHooks
            hooks = TurnHooks(SessionLocal, llm_client=_get_shared_llm_client())
            hooks.run_observer(session_id, user_id, messages)

        if user_content and len(messages) >= 2:
            from core.agent.turn_hooks import TurnHooks
            hooks = TurnHooks(SessionLocal, llm_client=_get_shared_llm_client())
            hooks.detect_implicit_feedback(user_content, messages, parent_event_id)

    except Exception as e:
        logger.warning("Event persistence failed (non-fatal): %s", e)

    return context_capture_id


# Lazy-initialized shared LLM client for background tasks (Observer).
# Avoids constructing a new LLMClient per turn (expensive: DB queries + provider init).
_shared_llm_client = None
_shared_llm_lock = threading.Lock()


def _get_shared_llm_client():
    """Get or create a shared LLMClient for background tasks."""
    global _shared_llm_client
    if _shared_llm_client is None:
        with _shared_llm_lock:
            if _shared_llm_client is None:
                from core.llm.client import LLMClient
                _shared_llm_client = LLMClient(SessionLocal)
    return _shared_llm_client


@router.post("/chat/turn")
async def chat_turn(
    request: ChatTurnRequest,
    current_user: Annotated[dict, Depends(get_current_user)],
):
    """One LLM turn in the edge-cloud agentic loop.

    Edge sends messages + tool_results → cloud does context enrichment + LLM call →
    returns SSE stream of text_delta, tool_call, usage, turn_complete events.
    """
    user_id = current_user["user_id"]
    db = SessionLocal()
    try:
        session_id = _ensure_session(db, user_id, request.session_id, request.agent_id)
    finally:
        db.close()

    # Detect tool changes: compare new edge_tools with cached set.
    # On change, we rebuild the system prompt (with new Self-Model) but preserve
    # conversation history — see force_rebuild_system in _build_turn_messages.
    tools_changed = False
    if request.edge_tools:
        cached = _session_tools.get(session_id, [])
        if cached and _tool_names(request.edge_tools) != _tool_names(cached):
            tools_changed = True
        _session_tools[session_id] = request.edge_tools
    tools_schema = _session_tools.get(session_id, [])

    # Build conversation messages with context enrichment
    db = SessionLocal()
    try:
        llm_messages = _build_turn_messages(
            db, user_id, session_id,
            request.messages, request.tool_results, request.project_rules,
            agent_id=request.agent_id,
            edge_tools=request.edge_tools,
            edge_profile=request.edge_profile.model_dump(exclude_none=True) if request.edge_profile else None,
            force_rebuild_system=tools_changed,
            username=current_user.get("username"),
        )
    finally:
        db.close()

    model = request.model

    async def event_generator():
        yield f"data: {json.dumps({'type': 'session_info', 'session_id': session_id})}\n\n"

        try:
            from core.llm.client import LLMClient
            llm = LLMClient(SessionLocal)

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
