"""Chat API endpoints — unified conversation entry point with durable AgentRun."""

import json
import threading
import time
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


# Unified per-session cache: {"history": list[dict], "tools": list[dict], "ts": float}
# Single LRU ensures history and tools are evicted together.
# TTL (24h) evicts idle sessions even if LRU capacity is not reached.
_SESSION_TTL = 86400  # 24 hours in seconds


class _SessionCache(_LRUDict):
    """LRU dict with per-entry TTL for session data.

    Values are dicts with keys: history (list), tools (list), ts (float).
    The ts field is managed automatically — callers should not set it.
    Uses OrderedDict.__getitem__ (not super().__getitem__) inside self._lock
    to avoid redundant RLock re-acquisition from _LRUDict.__getitem__.
    """

    def __init__(self, maxsize: int, ttl: int = _SESSION_TTL):
        super().__init__(maxsize)
        self._ttl = ttl

    def get(self, key, default=None):
        with self._lock:
            if key not in self:
                return default
            entry = OrderedDict.__getitem__(self, key)
            if time.monotonic() - entry.get("ts", 0) > self._ttl:
                OrderedDict.__delitem__(self, key)
                return default
            entry["ts"] = time.monotonic()
            self.move_to_end(key)
            return entry

    def __setitem__(self, key, value):
        if not isinstance(value, dict):
            raise TypeError(f"_SessionCache values must be dict, got {type(value).__name__}")
        value.setdefault("ts", time.monotonic())
        super().__setitem__(key, value)


_session_cache: _SessionCache = _SessionCache(_MAX_CACHED_SESSIONS)


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
) -> tuple[list[dict[str, Any]], str | None]:
    """Build LLM messages from edge turn data + server-side history.

    Returns (messages, context_capture_id).  context_capture_id is the snapshot
    saved by PromptAssembler BEFORE the LLM call; on turn 2+ it comes from
    incremental memory refresh.
    """
    cached = _session_cache.get(session_id) or {}
    history = cached.get("history")
    cached_sections = cached.get("sections")
    context_capture_id: str | None = None

    # Recover from DB if not in memory (server restart).
    # Also recovers sections so incremental refresh works on subsequent turns.
    if history is None:
        history, cached_sections = _recover_history_from_db(db, user_id, session_id, agent_id)

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
        context_capture_id = assembled.snapshot_id
        cached_sections = assembled.sections
        logger.debug("Assembled prompt: %d tokens, snapshot=%s", sum(assembled.token_breakdown.values()), assembled.snapshot_id)

        if history and force_rebuild_system:
            # Replace system message, keep conversation history intact
            history[0] = {"role": "system", "content": system}
        else:
            history = [{"role": "system", "content": system}]
    elif cached_sections:
        # Turn 2+: incremental memory refresh
        user_query = next((m.get("content", "") for m in messages if m.get("role") == "user"), "")
        if user_query:
            from core.context.prompt_assembler import PromptAssembler
            try:
                refreshed = PromptAssembler(SessionLocal).refresh_memory(
                    session_id=session_id,
                    user_id=user_id,
                    user_query=user_query,
                    current_sections=cached_sections,
                )
                history[0] = {"role": "system", "content": refreshed.system_message}
                context_capture_id = refreshed.snapshot_id
                cached_sections = refreshed.sections
            except Exception as e:
                logger.debug("Memory refresh failed (non-fatal): %s", e)

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

    entry = _session_cache.get(session_id) or {}
    entry["history"] = history
    if cached_sections:
        entry["sections"] = cached_sections
    _session_cache[session_id] = entry
    return history, context_capture_id


# Max conversation events to recover on server restart.
# Inlined into SQL via f-string (not parameterized) because MySQL-compatible DBs
# (including MatrixOne) may quote parameterized LIMIT values as strings:
# `LIMIT '50'` → syntax error. SQLAlchemy bindparam() has the same issue.
# Safe: this is a module-level int constant, not user input.
_MAX_RECOVERY_EVENTS = 50


def _recover_history_from_db(
    db: Session, user_id: str, session_id: str, agent_id: str | None = None,
) -> tuple[list[dict[str, Any]], dict[str, str] | None]:
    """Rebuild conversation history from persisted events (for server restart recovery).

    Recovers user_query, llm_response, tool_call, and tool_result events
    to produce a valid OpenAI message sequence:
        user → assistant(tool_calls) → tool(result) → assistant → ...

    Returns (history, sections).  sections is the prompt section dict from
    PromptAssembler so that subsequent turns can do incremental refresh.
    """
    try:
        rows = db.execute(
            text(f"""
                SELECT event_type, content, metadata FROM conversation_events
                WHERE session_id = :sid
                  AND event_type IN ('user_query', 'llm_response', 'tool_call', 'tool_result')
                ORDER BY created_at ASC LIMIT {_MAX_RECOVERY_EVENTS}
            """),
            {"sid": session_id},
        ).fetchall()
        if not rows:
            return [], None

        first_query = next((r[1] for r in rows if r[0] == "user_query"), "")
        from core.context.prompt_assembler import PromptAssembler
        assembled = PromptAssembler(SessionLocal).assemble(
            agent_id=agent_id, user_query=first_query,
            session_id=session_id, user_id=user_id,
        )
        history: list[dict[str, Any]] = [
            {"role": "system", "content": assembled.system_message}
        ]

        # State machine for reconstructing OpenAI message sequences:
        #   tool_call events accumulate in pending_tool_calls.
        #   The first tool_result flushes them as one assistant message.
        #   Subsequent tool_results in the same batch just append tool messages.
        #   If tool_call was lost, we synthesize from tool_result metadata.
        pending_tool_calls: list[dict[str, Any]] = []
        # True after we've emitted the assistant+tool_calls for the current
        # batch — subsequent tool_results just append tool messages.
        in_tool_batch = False

        for row in rows:
            etype, content = row[0], row[1] or ""
            meta = row[2] if len(row) > 2 else None
            if isinstance(meta, str):
                try:
                    meta = json.loads(meta)
                except (json.JSONDecodeError, TypeError):
                    meta = {}
            meta = meta or {}

            if etype == "user_query":
                in_tool_batch = False
                history.append({"role": "user", "content": content})
            elif etype == "tool_call":
                try:
                    tc_data = json.loads(content) if isinstance(content, str) else {}
                except (json.JSONDecodeError, TypeError):
                    tc_data = {}
                pending_tool_calls.append({
                    "id": tc_data.get("tool_call_id", meta.get("tool_call_id", "")),
                    "type": "function",
                    "function": {
                        "name": tc_data.get("name", meta.get("name", "")),
                        "arguments": tc_data.get("arguments", "{}"),
                    },
                })
            elif etype == "tool_result":
                tool_call_id = meta.get("tool_call_id", "")
                tool_name = meta.get("name", "")
                if pending_tool_calls:
                    # First tool_result: flush all accumulated tool_calls as
                    # one assistant message.
                    history.append({"role": "assistant", "content": "", "tool_calls": pending_tool_calls})
                    pending_tool_calls = []
                    in_tool_batch = True
                elif not in_tool_batch:
                    # tool_call was lost (truncated by _MAX_RECOVERY_EVENTS).
                    # Synthesize from metadata to keep the sequence valid.
                    if not tool_call_id:
                        continue  # Cannot construct valid pair — skip.
                    history.append({"role": "assistant", "content": "", "tool_calls": [{
                        "id": tool_call_id, "type": "function",
                        "function": {"name": tool_name, "arguments": "{}"},
                    }]})
                    in_tool_batch = True
                # Append tool result message.
                try:
                    result_data = json.loads(content) if isinstance(content, str) else {}
                except (json.JSONDecodeError, TypeError):
                    result_data = {}
                history.append({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": result_data.get("result", content)[:4000] if isinstance(result_data, dict) else str(content)[:4000],
                })
            elif etype == "llm_response":
                in_tool_batch = False
                if pending_tool_calls:
                    history.append({"role": "assistant", "content": "", "tool_calls": pending_tool_calls})
                    pending_tool_calls = []
                history.append({"role": "assistant", "content": content})

        # Intentionally discard trailing pending_tool_calls: incomplete tool
        # call sequences (e.g. run cancelled mid-tool) should not be sent to
        # the LLM — they would produce an invalid message sequence.
        return history, assembled.sections
    except SQLAlchemyError as e:
        logger.debug("History recovery failed: %s", e)
        return [], None


def _persist_turn_events(
    user_id: str,
    session_id: str,
    messages: list[dict[str, Any]],
    tool_results: list[dict[str, Any]] | None,
    full_text: str,
    tool_calls: list[dict[str, Any]],
    context_capture_id: str | None = None,
) -> None:
    """Persist events for this turn: user query, tool results, LLM response.

    Also writes decision audit, skill selection, observations, and implicit feedback
    via TurnHooks. All writes are best-effort — failures are logged but never block.

    Context snapshot is NOT saved here — it is saved BEFORE the LLM call by
    PromptAssembler (the correct timing). The snapshot ID is passed in via
    context_capture_id so DecisionAudit can reference it.
    """
    from uuid_utils import uuid7

    user_content = next((m["content"] for m in messages if m.get("role") == "user"), None)

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
                meta = {"source": "edge", "tool_call_id": tr.get("tool_call_id"), "name": tr.get("name", "")}
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

        # Persist LLM tool calls so _recover_history_from_db can reconstruct
        # the full assistant(tool_calls) → tool(result) message sequence.
        if tool_calls:
            for tc in tool_calls:
                tc_id = tc.get("id", "")
                tc_func = tc.get("function", {})
                el.create_stream_event(
                    user_id=user_id, session_id=session_id,
                    event_type="tool_call",
                    content=json.dumps({"tool_call_id": tc_id, "name": tc_func.get("name", ""), "arguments": tc_func.get("arguments", "{}")}),
                    parent_event_id=parent_event_id,
                    causal_chain_id=causal_chain_id,
                    metadata={"tool_call_id": tc_id, "name": tc_func.get("name", "")},
                )

        # Persist LLM response (tool_call names are already in tool_call events)
        if full_text or tool_calls:
            el.create_llm_response(
                user_id=user_id, session_id=session_id,
                content=full_text,
                agent_id="dev-agent", agent_version="0.1.0",
                parent_event_id=parent_event_id,
                causal_chain_id=causal_chain_id,
            )

        # Post-turn hooks (decision audit, skill selection, observer, feedback)
        from core.agent.turn_hooks import TurnHooks
        hooks = TurnHooks(SessionLocal, llm_client=_get_shared_llm_client())

        if parent_event_id:
            hooks.record_decision_audit(session_id, parent_event_id, tool_calls, full_text, context_capture_id)
            hooks.record_skill_selection(session_id, user_content or "", tool_calls)

        if user_content:
            hooks.run_observer(session_id, user_id, messages)
            hooks.detect_implicit_feedback(user_content, messages, parent_event_id)

    except Exception as e:
        logger.warning("Event persistence failed (non-fatal): %s", e)


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
    entry = _session_cache.get(session_id) or {}
    if request.edge_tools:
        cached = entry.get("tools", [])
        if cached and _tool_names(request.edge_tools) != _tool_names(cached):
            tools_changed = True
        entry["tools"] = request.edge_tools
        _session_cache[session_id] = entry
    tools_schema = entry.get("tools", [])

    # Build conversation messages with context enrichment
    db = SessionLocal()
    try:
        llm_messages, snapshot_id = _build_turn_messages(
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
            llm = LLMClient(SessionLocal, user_id=user_id)

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
            _entry = _session_cache.get(session_id) or {}
            _entry.setdefault("history", []).append(assistant_msg)
            _session_cache[session_id] = _entry

            # Persist events (non-blocking, best-effort)
            _persist_turn_events(
                user_id, session_id,
                request.messages, request.tool_results,
                full_text, tool_calls,
                context_capture_id=snapshot_id,
            )

            # Pre-completion firewall verification (must arrive before turn_complete
            # because edge clients may close the connection on turn_complete).
            # Runs in a thread to avoid blocking the event loop — verify_response
            # is synchronous and may do DB queries + claim extraction.
            firewall_warning: dict[str, Any] | None = None
            if full_text and snapshot_id:
                try:
                    import asyncio
                    from core.context.manager import ContextManager
                    from core.verification.firewall import HallucinationFirewall
                    ctx_mgr = ContextManager(SessionLocal)
                    fw = HallucinationFirewall(SessionLocal, context_manager=ctx_mgr)
                    result = await asyncio.to_thread(fw.verify_response, full_text, snapshot_id)
                    if not result.safe_to_deliver:
                        firewall_warning = {'type': 'warning', 'message': 'Response may contain unverified claims', 'claims_failed': result.claims_failed}
                except Exception as e:
                    logger.debug("Firewall verification skipped: %s", e)

            if firewall_warning:
                yield f"data: {json.dumps(firewall_warning)}\n\n"

            yield f"data: {json.dumps({'type': 'turn_complete', 'has_tool_calls': len(tool_calls) > 0})}\n\n"

        except Exception as e:
            logger.error("chat_turn error: %s", e, exc_info=True)
            yield f"data: {json.dumps({'type': 'error', 'message': str(e)})}\n\n"

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "Connection": "keep-alive", "X-Accel-Buffering": "no"},
    )
