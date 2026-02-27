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
    explain: bool = Field(default=False, description="Return execution stats (like EXPLAIN ANALYZE)")


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
    explain: dict | None = Field(default=None, description="Execution stats when explain=true")


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

    from core.memory.typed_observer import TypedObserver
    loop.set_observer(TypedObserver(db_factory, llm_client=llm_client))

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


# Unified per-session cache entry schema:
#   {"history": list[dict], "tools": list[dict], "sections": dict[str,str],
#    "spend_usd": float, "turn_count": int, "ts": float}
# Single LRU ensures all fields are evicted together.
# TTL (24h) evicts idle sessions even if LRU capacity is not reached.
_SESSION_TTL = 86400  # 24 hours in seconds
# Persist a full history snapshot every N turns (reduces DB write volume).
_SNAPSHOT_TURN_INTERVAL = 3


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

# Background persistence threads — kept for test-time join via _flush_persist_threads().
_persist_threads: list[threading.Thread] = []


def _flush_persist_threads(timeout: float = 5.0) -> None:
    """Join all pending persistence threads. Used by tests for deterministic assertions."""
    while _persist_threads:
        t = _persist_threads.pop(0)
        t.join(timeout=timeout)


def _tool_names(tools: list[dict[str, Any]]) -> set[str]:
    """Extract tool names from OpenAI-format tool schemas for change detection."""
    return {t.get("function", {}).get("name", "") for t in tools}


def _peek_session_entry(session_id: str) -> dict[str, Any] | None:
    """Read-only lookup — returns None if not cached. No LRU side-effects."""
    return _session_cache.get(session_id)


def _get_or_create_session_entry(session_id: str) -> dict[str, Any]:
    """Get existing or create new cache entry (may evict LRU entries)."""
    entry = _session_cache.get(session_id)
    if entry is None:
        entry = {"history": None, "tools": [], "sections": None,
                 "spend_usd": 0.0, "turn_count": 0}
        _session_cache[session_id] = entry
    return entry


def _classify_task(messages: list[dict[str, Any]]) -> str | None:
    """Best-effort task hint for model routing (soft signal only).

    Returns "simple" | "code" | "reasoning" | None.  The model router MUST
    have a fallback for None — this heuristic is intentionally conservative
    and returns None when confidence is low.
    """
    # Use the LAST user message — in multi-turn tool loops the first message
    # may be from a previous intent.
    texts = [m.get("content", "") for m in messages if m.get("role") == "user"]
    text = texts[-1] if texts else ""
    if not text:
        return None
    lower = text.lower()
    # Code: only match when code artifacts are clearly present (fenced blocks,
    # or file extensions preceded by a word boundary like space/punctuation).
    if "```" in lower:
        return "code"
    import re
    if re.search(r'(?<!\w)\.(py|go|ts|js|rs|java|cpp|rb)\b', lower):
        return "code"
    # Reasoning: require word boundaries to avoid false positives like "classic".
    if re.search(r'\b(explain|analyze|reason|compare)\b', lower):
        return "reasoning"
    # Default: no hint — let the model router use its default.
    return None


def _heal_orphaned_tool_calls(history: list[dict[str, Any]]) -> None:
    """Scan history and inject placeholder tool messages for any assistant
    tool_calls that lack matching tool responses.

    OpenAI-compatible APIs require every tool_call to be followed (before the
    next non-tool message) by a tool message with the same tool_call_id.
    Edge may skip tool_results due to max-turns, crash, Ctrl-C, or network
    disconnect.  Cloud heals these gaps so the LLM API never rejects history.

    Mutates *history* in-place.  Inserts are done in reverse index order so
    earlier indices remain valid while we splice.
    """
    # Collect (insert_position, placeholder_msg) pairs.
    inserts: list[tuple[int, dict[str, Any]]] = []
    for i, msg in enumerate(history):
        if msg.get("role") != "assistant" or not msg.get("tool_calls"):
            continue
        expected = {tc["id"] for tc in msg["tool_calls"]}
        found: set[str] = set()
        for j in range(i + 1, len(history)):
            if history[j].get("role") == "tool":
                found.add(history[j].get("tool_call_id", ""))
            else:
                break
        missing = expected - found
        if missing:
            # Insert right after the last existing tool message (or after
            # the assistant message itself if there are none).
            insert_at = i + 1 + len(found)
            for tc in msg["tool_calls"]:
                if tc["id"] in missing:
                    inserts.append((insert_at, {
                        "role": "tool",
                        "tool_call_id": tc["id"],
                        "content": "[not executed -- edge disconnected]",
                    }))
                    insert_at += 1
    # Splice in reverse so indices stay valid.
    for pos, placeholder in reversed(inserts):
        history.insert(pos, placeholder)


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
    entry = _get_or_create_session_entry(session_id)
    history = entry.get("history")
    cached_sections = entry.get("sections")
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
        # Turn 2+: incremental memory refresh when new user query OR tool results arrive.
        user_query = next((m.get("content", "") for m in messages if m.get("role") == "user"), "")
        should_refresh = bool(user_query) or bool(tool_results)
        if should_refresh:
            # For tool-result-only turns (no new user query), use the last user
            # message from history so memory retrieval gets a semantically
            # meaningful query instead of a placeholder string.
            refresh_query = user_query
            if not refresh_query and history:
                refresh_query = next(
                    (m.get("content", "") for m in reversed(history) if m.get("role") == "user"),
                    "",
                )
            if refresh_query:
                from core.context.prompt_assembler import PromptAssembler
                try:
                    refreshed = PromptAssembler(SessionLocal).refresh_memory(
                        session_id=session_id,
                        user_id=user_id,
                        user_query=refresh_query,
                        current_sections=cached_sections,
                    )
                    history[0] = {"role": "system", "content": refreshed.system_message}
                    context_capture_id = refreshed.snapshot_id
                    cached_sections = refreshed.sections
                except Exception as e:
                    logger.debug("Memory refresh failed (non-fatal): %s", e)

    # History integrity: cloud guarantees a valid OpenAI message sequence
    # regardless of edge behavior.  Scan the *entire* history for orphaned
    # tool_calls (assistant has tool_calls but no matching tool messages
    # follow).  This handles: max-turns, crash, Ctrl-C, network disconnect,
    # and partial tool_results (edge sent results for some calls but not all).
    # We collect placeholders first, then splice them in reverse order so
    # indices stay valid.
    _heal_orphaned_tool_calls(history)

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


def _append_recovered_events(
    history: list[dict[str, Any]], rows: list,
) -> list[dict[str, Any]]:
    """Append DB event rows to an existing history list (OpenAI message format).

    Used by both snapshot post-fill and full event-by-event reconstruction.
    Handles tool_call batching: accumulates tool_call events, flushes them as
    one assistant message when the first tool_result arrives.
    """
    pending_tool_calls: list[dict[str, Any]] = []
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
                history.append({"role": "assistant", "content": "", "tool_calls": pending_tool_calls})
                pending_tool_calls = []
                in_tool_batch = True
            elif not in_tool_batch:
                if not tool_call_id:
                    continue
                history.append({"role": "assistant", "content": "", "tool_calls": [{
                    "id": tool_call_id, "type": "function",
                    "function": {"name": tool_name, "arguments": "{}"},
                }]})
                in_tool_batch = True
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

    return history


def _recover_history_from_db(
    db: Session, user_id: str, session_id: str, agent_id: str | None = None,
) -> tuple[list[dict[str, Any]], dict[str, str] | None]:
    """Rebuild conversation history from persisted events (for server restart recovery).

    Fast path: load latest session_history_snapshot if available.
    Fallback: reconstruct from individual events (user_query, llm_response, etc.).

    Returns (history, sections).  sections is the prompt section dict from
    PromptAssembler so that subsequent turns can do incremental refresh.
    """
    # Fast path: try snapshot first
    try:
        snap_row = db.execute(
            text("""
                SELECT content, created_at FROM conversation_events
                WHERE session_id = :sid AND event_type = 'session_history_snapshot'
                ORDER BY created_at DESC LIMIT 1
            """),
            {"sid": session_id},
        ).first()
        if snap_row and snap_row[0]:
            history = json.loads(snap_row[0])
            if isinstance(history, list) and history:
                # Use the LAST user message for memory retrieval — first_query
                # would be stale for long conversations.
                from core.context.prompt_assembler import PromptAssembler
                last_query = next(
                    (m.get("content", "") for m in reversed(history) if m.get("role") == "user"),
                    "",
                )
                assembled = PromptAssembler(SessionLocal).assemble(
                    agent_id=agent_id, user_query=last_query,
                    session_id=session_id, user_id=user_id,
                )
                # Replace system message with fresh one (may have updated agent config)
                history[0] = {"role": "system", "content": assembled.system_message}

                # Append events that arrived AFTER the snapshot (e.g. snapshot
                # was at turn 3 but conversation continued to turn 5).
                snap_ts = snap_row[1]
                if snap_ts:
                    post_rows = db.execute(
                        text(f"""
                            SELECT event_type, content, metadata FROM conversation_events
                            WHERE session_id = :sid
                              AND event_type IN ('user_query', 'llm_response', 'tool_call', 'tool_result')
                              AND created_at > :snap_ts
                            ORDER BY created_at ASC LIMIT {_MAX_RECOVERY_EVENTS}
                        """),
                        {"sid": session_id, "snap_ts": snap_ts},
                    ).fetchall()
                    if post_rows:
                        history = _append_recovered_events(history, post_rows)

                return history, assembled.sections
    except Exception as e:
        logger.debug("Snapshot recovery failed, falling back to events: %s", e)

    # Fallback: event-by-event reconstruction
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

        # Use the LAST user query for memory retrieval (most relevant context).
        last_query = ""
        for r in reversed(rows):
            if r[0] == "user_query":
                last_query = r[1] or ""
                break
        from core.context.prompt_assembler import PromptAssembler
        assembled = PromptAssembler(SessionLocal).assemble(
            agent_id=agent_id, user_query=last_query,
            session_id=session_id, user_id=user_id,
        )
        history: list[dict[str, Any]] = [
            {"role": "system", "content": assembled.system_message}
        ]
        history = _append_recovered_events(history, rows)
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
    model_used: str | None = None,
    history: list[dict[str, Any]] | None = None,
    turn_count: int = 0,
) -> None:
    """Persist events for this turn: user query, tool results, LLM response.

    Also writes decision audit, skill selection, observations, implicit feedback
    via TurnHooks, and periodic history snapshots. All writes are best-effort —
    failures are logged but never block.

    Context snapshot is NOT saved here — it is saved BEFORE the LLM call by
    PromptAssembler (the correct timing). The snapshot ID is passed in via
    context_capture_id so DecisionAudit can reference it.
    """
    from uuid_utils import uuid7
    from core.events.event_logger import EventLogger

    user_content = next((m["content"] for m in messages if m.get("role") == "user"), None)
    el = EventLogger(SessionLocal)
    parent_event_id = None
    causal_chain_id = str(uuid7())

    # Phase 1: persist user query
    try:
        if user_content:
            user_ev = el.create_user_query(user_id=user_id, session_id=session_id, content=user_content)
            parent_event_id = user_ev.event_id
            causal_chain_id = user_ev.causal_chain_id
    except Exception as e:
        logger.warning("Phase 1 (user_query) failed: %s", e)

    # Phase 2: persist tool results from edge
    try:
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
    except Exception as e:
        logger.warning("Phase 2 (tool_results) failed: %s", e)

    # Phase 3: persist tool calls + LLM response + history snapshot
    try:
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

        if full_text or tool_calls:
            el.create_llm_response(
                user_id=user_id, session_id=session_id,
                content=full_text,
                agent_id="dev-agent", agent_version="0.1.0",
                parent_event_id=parent_event_id,
                causal_chain_id=causal_chain_id,
            )

        if history and turn_count > 0 and turn_count % _SNAPSHOT_TURN_INTERVAL == 0:
            el.create_stream_event(
                user_id=user_id, session_id=session_id,
                event_type="session_history_snapshot",
                content=json.dumps(history),
                parent_event_id=parent_event_id,
                causal_chain_id=causal_chain_id,
                metadata={"turn_count": turn_count},
            )
    except Exception as e:
        logger.warning("Phase 3 (llm_response/snapshot) failed: %s", e)

    # Phase 4: post-turn hooks (decision audit, skill selection, observer, feedback)
    try:
        from core.agent.turn_hooks import TurnHooks
        hooks = TurnHooks(SessionLocal, llm_client=_get_shared_llm_client())

        if parent_event_id:
            hooks.record_decision_audit(
                session_id, parent_event_id, tool_calls, full_text,
                context_capture_id, model_used=model_used,
            )
            hooks.record_skill_selection(session_id, user_content or "", tool_calls)

        # Observer: only on final reply (no tool_calls, has text).
        # Intermediate turns (tool_call→tool_result cycles) have no meaningful
        # content for memory extraction. Aligns with Mastra/Claude Code approach.
        is_final_reply = bool(full_text) and not tool_calls
        if full_text and tool_calls:
            logger.debug(
                "Observer skipped: intermediate turn has text (%d chars) + %d tool_calls",
                len(full_text), len(tool_calls),
            )
        if is_final_reply:
            observer_messages: list[dict[str, Any]] = []
            if user_content:
                observer_messages.append({"role": "user", "content": user_content})
            observer_messages.append({"role": "assistant", "content": full_text})
            hooks.run_observer(session_id, user_id, observer_messages)

        if user_content:
            hooks.detect_implicit_feedback(user_content, messages, parent_event_id)
    except Exception as e:
        logger.warning("Phase 4 (TurnHooks) failed: %s", e)


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
    existing = _peek_session_entry(session_id)
    if request.edge_tools:
        cached = (existing or {}).get("tools", [])
        if cached and _tool_names(request.edge_tools) != _tool_names(cached):
            tools_changed = True
        entry = _get_or_create_session_entry(session_id)
        entry["tools"] = request.edge_tools
        _session_cache[session_id] = entry
    tools_schema = (existing or {}).get("tools", []) if not request.edge_tools else request.edge_tools

    # Build conversation messages with context enrichment.
    # Runs in a thread to avoid blocking the event loop — _build_turn_messages
    # does synchronous DB queries (recovery, PromptAssembler, snapshot save).
    import asyncio

    def _build_sync():
        db = SessionLocal()
        try:
            return _build_turn_messages(
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

    llm_messages, snapshot_id = await asyncio.to_thread(_build_sync)

    model = request.model
    task_hint = _classify_task(request.messages)

    async def event_generator():
        yield f"data: {json.dumps({'type': 'session_info', 'session_id': session_id})}\n\n"

        try:
            llm = _get_shared_llm_client()
            with llm.request_context(user_id=user_id):

                full_text = ""
                tool_calls: list[dict[str, Any]] = []

                if tools_schema:
                    async for chunk in llm.chat_with_tools_stream(
                        llm_messages, tools_schema, model=model, task_hint=task_hint,
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

                # Update session cache: append assistant message, increment turn_count
                _entry = _get_or_create_session_entry(session_id)
                assistant_msg: dict[str, Any] = {"role": "assistant", "content": full_text}
                if tool_calls:
                    assistant_msg["tool_calls"] = tool_calls
                _entry.setdefault("history", []).append(assistant_msg)
                _entry["turn_count"] = _entry.get("turn_count", 0) + 1
                _session_cache[session_id] = _entry
                current_turn_count = _entry["turn_count"]
                current_history = list(_entry.get("history", []))

                # Resolve actual model name for audit (not the user's request, but
                # what the router selected — may differ due to fallback chain).
                resolved_model = llm.resolve_model_name(model)

            # Persist events in background thread (non-blocking, best-effort).
            # Deep-copy mutable dicts to avoid sharing state with the main thread.
            import copy
            _persist_args = dict(
                user_id=user_id, session_id=session_id,
                messages=copy.deepcopy(request.messages),
                tool_results=copy.deepcopy(request.tool_results or []),
                full_text=full_text, tool_calls=copy.deepcopy(tool_calls),
                context_capture_id=snapshot_id, model_used=resolved_model,
                history=copy.deepcopy(current_history),
                turn_count=current_turn_count,
            )
            _t = threading.Thread(target=_persist_turn_events, kwargs=_persist_args, daemon=True)
            _persist_threads.append(_t)
            _t.start()

            # Pre-completion firewall verification (must arrive before turn_complete
            # because edge clients may close the connection on turn_complete).
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
            from core.llm.client import BudgetExceededError
            from core.exceptions import LLMRateLimitError, LLMTimeoutError, TransientError
            from sqlalchemy.exc import SQLAlchemyError
            err: dict[str, Any] = {"type": "error", "message": str(e)}
            if isinstance(e, BudgetExceededError):
                err.update(code="BUDGET_EXCEEDED", retryable=False)
            elif isinstance(e, LLMRateLimitError):
                err.update(code="LLM_RATE_LIMIT", retryable=True, retry_after_ms=5000)
            elif isinstance(e, (LLMTimeoutError, TransientError)):
                err.update(code="LLM_TIMEOUT", retryable=True, retry_after_ms=2000)
            elif isinstance(e, (SQLAlchemyError, ConnectionError, OSError)):
                err.update(code="SERVER_ERROR", retryable=True, retry_after_ms=1000)
            else:
                err.update(code="INTERNAL_ERROR", retryable=False)
            yield f"data: {json.dumps(err)}\n\n"

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "Connection": "keep-alive", "X-Accel-Buffering": "no"},
    )
