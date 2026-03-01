"""Chat API endpoints — unified conversation entry point with durable AgentRun."""

import asyncio
import json
import os
import threading
import time
from collections import OrderedDict
from collections.abc import AsyncIterator
from typing import Annotated, Any, Literal

from fastapi import APIRouter, Depends, HTTPException, Query
from fastapi.responses import StreamingResponse
from pydantic import BaseModel, ConfigDict, Field
from sqlalchemy.exc import SQLAlchemyError
from sqlalchemy.orm import Session

from api.sse_errors import SSE_HEADERS, status_to_error_code

from api.database import SessionLocal
from api.dependencies import get_current_user
from core.history_utils import (
    merge_tool_results_into_history as _merge_tool_results_into_history,
    append_recovered_events as _append_recovered_events,
)
from core.logging_config import get_logger
from core.verification.tool_quality import (
    assess_tool_result as _assess_tool_result,
    annotate_tool_result as _annotate_tool_result,
)

logger = get_logger(__name__)
router = APIRouter()

# ---------------------------------------------------------------------------
# SSE Heartbeat (§3.1 of edge-cloud-execution.md)
# ---------------------------------------------------------------------------

HEARTBEAT_INTERVAL_S = 15
SERVER_TURN_TIMEOUT_S = 240

_HEARTBEAT_SENTINEL = object()


import re as _re


def _try_repair_tool_args(tc_name: str, raw: str) -> dict | None:
    """Best-effort repair of malformed tool-call JSON from the LLM.

    Returns parsed dict on success, None if unrecoverable.
    Common failures:
      1. Trailing comma before closing brace  ``{"a": 1,}``
      2. Unescaped control chars inside string values (literal newlines/tabs)
      3. Truncated JSON missing closing braces/quotes
      4. Single-quoted strings
    """
    s = raw.strip()
    if not s:
        return None

    # 1. Single quotes → double quotes (only outermost; naive but covers common case)
    if s.startswith("{'") or ", '" in s:
        s = s.replace("'", '"')

    # 2. Trailing commas:  ,} or ,]
    s = _re.sub(r",\s*([}\]])", r"\1", s)

    # 3. Unescaped literal newlines/tabs inside strings — escape them
    #    Walk char-by-char to only fix inside quoted regions.
    fixed: list[str] = []
    in_str = False
    i = 0
    while i < len(s):
        ch = s[i]
        if ch == '"' and (i == 0 or s[i - 1] != '\\'):
            in_str = not in_str
            fixed.append(ch)
        elif in_str and ch == '\n':
            fixed.append('\\n')
        elif in_str and ch == '\t':
            fixed.append('\\t')
        elif in_str and ch == '\r':
            fixed.append('\\r')
        else:
            fixed.append(ch)
        i += 1
    s = "".join(fixed)

    # 4. Try parsing now
    try:
        return json.loads(s)
    except json.JSONDecodeError:
        pass

    # 5. Truncated — try closing open braces/brackets/quotes
    #    Count unmatched openers and append closers.
    depth_brace = s.count('{') - s.count('}')
    depth_bracket = s.count('[') - s.count(']')
    if in_str:
        s += '"'
    s += ']' * max(depth_bracket, 0)
    s += '}' * max(depth_brace, 0)
    try:
        result = json.loads(s)
        logger.info("Repaired truncated tool_call JSON for %s", tc_name)
        return result
    except json.JSONDecodeError:
        return None


def _sse_ping() -> str:
    return f"data: {json.dumps({'type': 'ping', 'ts': int(time.time() * 1000)})}\n\n"


async def _with_heartbeat(sse_generator: AsyncIterator[str]) -> AsyncIterator[str]:
    """Wrap an SSE generator with periodic ping events."""
    queue: asyncio.Queue[str | BaseException | object] = asyncio.Queue(maxsize=1000)

    async def _drain() -> None:
        try:
            async for line in sse_generator:
                await queue.put(line)
        except asyncio.CancelledError:
            raise
        except BaseException as exc:
            await queue.put(exc)
        finally:
            await queue.put(_HEARTBEAT_SENTINEL)

    task = asyncio.create_task(_drain())
    try:
        while True:
            try:
                item = await asyncio.wait_for(
                    queue.get(), timeout=HEARTBEAT_INTERVAL_S,
                )
            except asyncio.TimeoutError:
                yield _sse_ping()
                continue
            if item is _HEARTBEAT_SENTINEL:
                break
            if isinstance(item, BaseException):
                raise item
            yield item
    finally:
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass
        # No explicit aclose() needed: _drain() consumes sse_generator via
        # async-for, which guarantees aclose() on both normal exit and
        # cancellation (CancelledError propagates through the for-body and
        # triggers the implicit finally of the async-for protocol).


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
    explain: bool = Field(default=False, description="Return per-step execution trace (like EXPLAIN ANALYZE)")


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

# Skill version cache: {skill_name: (version | None, timestamp)}.
# TTL-based per-entry cache avoids repeated DB queries on high-frequency turns.
# Versions change rarely (deploy-time), so a 60s TTL is safe.
# None means the skill is not in the registry (negative cache).
#
# Thread safety: accessed from background persist threads. CPython GIL makes
# individual dict read/write atomic. Worst case under concurrency: duplicate
# DB query on simultaneous cache miss — harmless. If free-threaded Python is
# adopted, wrap with a threading.Lock.
_SKILL_VERSION_CACHE: dict[str, tuple[str | None, float]] = {}
_SKILL_VERSION_TTL = float(os.environ.get("SKILL_VERSION_CACHE_TTL", "60"))  # seconds
_TOOL_QUALITY_ENABLED = os.environ.get("ENABLE_TOOL_QUALITY_FIREWALL", "true").lower() == "true"


def _resolve_skill_versions(names: set[str]) -> dict[str, str]:
    """Return {skill_name: latest_version} for the given names, with TTL cache."""
    now = time.monotonic()
    result: dict[str, str] = {}
    miss: set[str] = set()

    for n in names:
        entry = _SKILL_VERSION_CACHE.get(n)
        if entry and (now - entry[1]) < _SKILL_VERSION_TTL:
            if entry[0] is not None:
                result[n] = entry[0]
            # else: negative cache hit — skill not in registry, skip
        else:
            miss.add(n)

    if miss:
        from api.models.skill import SkillRegistry as SR
        with SessionLocal() as db:
            rows = db.query(SR.skill_name, SR.version).filter(
                SR.skill_name.in_(miss), SR.is_active == 1,
            ).order_by(SR.skill_name, SR.version.desc()).all()
            found: set[str] = set()
            for name, ver in rows:
                if name not in found:  # first row per skill = latest (ORDER BY version DESC)
                    result[name] = ver
                    found.add(name)
                    _SKILL_VERSION_CACHE[name] = (ver, now)
            # Negative cache: remember unregistered names to avoid repeated DB misses
            for name in miss - found:
                _SKILL_VERSION_CACHE[name] = (None, now)

    return result


def _ensure_session(db: Session, user_id: str, session_id: str | None, agent_id: str | None) -> str:
    """Return existing session_id or create a new one."""
    if session_id:
        from api.models.agent import Session as SessionModel
        row = db.query(SessionModel.session_id).filter(SessionModel.session_id == session_id).first()
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

    # Auth/validation errors are caught by exception handlers in main.py
    # (Depends(get_current_user) and Pydantic fire before this handler runs).
    # The generator-internal try/except handles errors during streaming.
    async def event_generator():
        try:
            user_id = current_user["user_id"]
            db = SessionLocal()
            try:
                session_id = _ensure_session(db, user_id, request.session_id, request.agent_id)
            finally:
                db.close()

            engine = _get_engine()

            context = request.context or {}
            if request.model:
                context["model"] = request.model

            run = engine.create_run(
                session_id=session_id,
                user_id=user_id,
                user_input=request.message,
                agent_id=request.agent_id or "dev-agent",
                context=context,
            )

            import asyncio
            task = asyncio.create_task(engine.start_run(run))
            from core.agent.run_engine import _run_tasks
            _run_tasks[run.run_id] = task

            yield f"data: {json.dumps({'event_type': 'session_info', 'data': {'session_id': session_id, 'run_id': run.run_id}})}\n\n"

            async for event in engine.stream_agent_run_events(run.run_id):
                yield f"data: {json.dumps(event)}\n\n"
        except Exception as e:
            logger.error(f"Stream error: {e}", exc_info=True)
            code = status_to_error_code(e.status_code) if isinstance(e, HTTPException) else "INTERNAL_ERROR"
            msg = e.detail if isinstance(e, HTTPException) else str(e)
            yield f"data: {json.dumps({'type': 'error', 'message': msg, 'code': code, 'retryable': False})}\n\n"

    return StreamingResponse(
        _with_heartbeat(event_generator()),
        media_type="text/event-stream",
        headers=SSE_HEADERS,
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

    events = engine.get_agent_run_events(run_id)
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

    # Auth errors handled by exception handlers in main.py.
    # Generator handles run-lookup and streaming errors.
    async def event_generator():
        try:
            engine = _get_engine()
            run = engine.get_run(run_id) or engine.restore_run(run_id)
            if not run:
                raise HTTPException(status_code=404, detail="Run not found")
            if run.user_id != current_user["user_id"]:
                raise HTTPException(status_code=403, detail="Not authorized to view this run")

            async for event in engine.stream_agent_run_events(run_id, last_index=last_index):
                yield f"data: {json.dumps(event)}\n\n"
        except Exception as e:
            logger.error(f"Stream run error: {e}", exc_info=True)
            code = status_to_error_code(e.status_code) if isinstance(e, HTTPException) else "INTERNAL_ERROR"
            msg = e.detail if isinstance(e, HTTPException) else str(e)
            yield f"data: {json.dumps({'type': 'error', 'message': msg, 'code': code, 'retryable': False})}\n\n"

    return StreamingResponse(
        _with_heartbeat(event_generator()),
        media_type="text/event-stream",
        headers=SSE_HEADERS,
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


def _flush_persist_threads(timeout: float = 30.0) -> None:
    """Join all pending persistence threads. Used by tests for deterministic assertions.

    Raises TimeoutError if any thread does not finish within *timeout* seconds
    so that tests fail loudly instead of silently reading incomplete data.
    """
    while _persist_threads:
        t = _persist_threads.pop(0)
        t.join(timeout=timeout)
        if t.is_alive():
            raise TimeoutError(
                f"Persist thread {t.name} still alive after {timeout}s — "
                "DB assertions would read incomplete data"
            )


def _tool_names(tools: list[dict[str, Any]]) -> set[str]:
    """Extract tool names from OpenAI-format tool schemas for change detection."""
    return {t.get("function", {}).get("name", "") for t in tools}


def _peek_session_entry(session_id: str) -> dict[str, Any] | None:
    """Read-only lookup — returns None if not cached. No LRU side-effects."""
    return _session_cache.get(session_id)


def _verify_session_owner(user_id: str, session_id: str, db: Session | None = None) -> None:
    """Verify user owns the session. Raises HTTPException on failure.

    Accepts an optional *db* session to avoid opening a second connection
    when the caller already holds one (e.g. _build_reflect_evidence).
    """
    def _check(conn: Session) -> None:
        from api.models.agent import Session as SessionModel
        row = conn.query(SessionModel.user_id).filter(SessionModel.session_id == session_id).first()
        if not row:
            raise HTTPException(status_code=404, detail="Session not found")
        if row[0] != user_id:
            raise HTTPException(status_code=403, detail="Not authorized")

    if db is not None:
        _check(db)
    else:
        with SessionLocal() as conn:
            _check(conn)


_ReflectFocus = Literal["auto", "skill_failure", "unexpected_result", "data_quality", "tool_selection", "history"]


def _escape_like(text: str) -> str:
    """Escape LIKE wildcards (%, _) in user-supplied text."""
    return text.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")


def _gather_tool_selection(
    session_id: str, question: str, db: Any,
    hints: list[str], result: dict[str, Any],
) -> None:
    """Gather cloud skills, edge tools, and usage counts into *result*."""
    # Cloud skills from in-memory registry.
    # SkillCatalog._skills is the only way to iterate in-memory Skill
    # instances — no public iterator exists.  Accessed read-only here.
    try:
        registry = _get_shared_skill_registry()
        cloud_skills = []
        seen_skills: set[str] = set()
        if registry:
            for key, skill in registry._skills.items():
                if "@" in key or skill.name in seen_skills:
                    continue
                seen_skills.add(skill.name)
                schema = skill.to_openai_schema()
                cloud_skills.append({
                    "name": skill.name,
                    "description": skill.description,
                    "parameters": schema.get("function", {}).get("parameters", {}),
                })
        result["cloud_skills"] = cloud_skills
    except Exception:
        logger.debug("Failed to load cloud skills for tool_selection", exc_info=True)
        result["cloud_skills"] = []

    entry = _peek_session_entry(session_id)
    result["edge_tools"] = [
        {"name": t.get("function", {}).get("name", "?"),
         "description": t.get("function", {}).get("description", "")[:80]}
        for t in (entry.get("tools", []) if entry else [])
    ]

    # Tool usage counts from events
    from api.models.agent import Event as EventModel
    usage_rows = (
        db.query(EventModel.content)
        .filter(EventModel.session_id == session_id, EventModel.event_type == "tool_call")
        .order_by(EventModel.created_at.desc()).limit(50).all()
    )
    tool_usage: dict[str, int] = {}
    for (c,) in usage_rows:
        try:
            name = json.loads(c).get("name", "unknown") if c else "unknown"
            tool_usage[name] = tool_usage.get(name, 0) + 1
        except (json.JSONDecodeError, TypeError):
            pass
    result["tool_usage_counts"] = tool_usage

    unused = {s["name"] for s in result.get("cloud_skills", [])} - set(tool_usage)
    if unused:
        hints.append(f"Cloud skills available but never called: {', '.join(sorted(unused))}")

    if question:
        for s in result.get("cloud_skills", []):
            if any(w in s["name"] for w in question.lower().split()):
                hints.append(f"Skill '{s['name']}' params: {json.dumps(s['parameters'])[:200]}")


def _gather_history(
    session_id: str, user_id: str, question: str, db: Any,
    result: dict[str, Any],
) -> None:
    """Find similar queries from past sessions using multi-keyword AND match."""
    from api.models.agent import Event as EventModel

    cur_query = db.query(EventModel.content).filter(
        EventModel.session_id == session_id, EventModel.event_type == "user_query",
    ).order_by(EventModel.created_at.desc()).first()
    cur_text = (cur_query[0] if cur_query else question) or ""

    if not cur_text:
        result["related_history"] = []
        return

    keywords = [w for w in cur_text.lower().split() if len(w) > 3][:3]
    # TODO: strip punctuation, filter stop words, handle non-space-delimited
    # languages (Chinese, Japanese) — current impl is English-whitespace-only.
    if not keywords:
        result["related_history"] = []
        return

    # Build AND filter: every keyword must appear in the query content.
    # Escape LIKE wildcards to prevent user input from altering match semantics.
    q = db.query(EventModel.session_id, EventModel.content, EventModel.created_at).filter(
        EventModel.user_id == user_id,
        EventModel.event_type == "user_query",
        EventModel.session_id != session_id,
    )
    for kw in keywords:
        escaped = _escape_like(kw)
        q = q.filter(EventModel.content.like(f"%{escaped}%", escape="\\"))

    past_rows = q.order_by(EventModel.created_at.desc()).limit(5).all()
    result["related_history"] = [
        {"session_id": r[0], "query": (r[1] or "")[:200], "ts": str(r[2])}
        for r in past_rows
    ]


def _build_reflect_evidence(
    session_id: str, user_id: str, focus: _ReflectFocus, last_n: int,
    question: str = "",
) -> dict[str, Any]:
    """Unified diagnostic evidence: events, skill decisions, tool selection, cross-session history.

    Dispatches to focused sub-functions (_gather_tool_selection, _gather_history)
    that each handle one concern.  All DB queries share a single session.
    """
    result: dict[str, Any] = {"session_id": session_id, "focus": focus}
    hints: list[str] = []

    with SessionLocal() as db:
        # 1. Event trail — server-side events with timing and token usage
        from api.models.agent import Event as EventModel
        rows = (
            db.query(
                EventModel.event_type, EventModel.content, EventModel.event_metadata,
                EventModel.created_at, EventModel.llm_model_used, EventModel.skill_name,
                EventModel.token_usage,
            )
            .filter(EventModel.session_id == session_id)
            .order_by(EventModel.created_at.desc())
            .limit(int(last_n))
            .all()
        )

        events = []
        fail_counts: dict[str, int] = {}
        # Token accumulators
        total_prompt = 0
        total_completion = 0
        llm_calls = 0
        cost_by_model: dict[str, dict[str, int]] = {}  # model → {prompt, completion, calls}

        for r in reversed(rows):
            evt = {"type": r[0], "ts": str(r[3]) if r[3] else None}
            if r[4]:
                evt["model"] = r[4]
            if r[5]:
                evt["skill"] = r[5]

            # Accumulate token usage from LLM responses
            if r[0] == "llm_response" and r[6]:
                usage = r[6] if isinstance(r[6], dict) else {}
                try:
                    if isinstance(r[6], str):
                        usage = json.loads(r[6])
                except (json.JSONDecodeError, TypeError):
                    usage = {}
                p = usage.get("prompt_tokens", usage.get("prompt", 0)) or 0
                c = usage.get("completion_tokens", usage.get("completion", 0)) or 0
                total_prompt += p
                total_completion += c
                llm_calls += 1
                model = r[4] or "unknown"
                entry = cost_by_model.setdefault(model, {"prompt": 0, "completion": 0, "calls": 0})
                entry["prompt"] += p
                entry["completion"] += c
                entry["calls"] += 1
            # Parse content for tool_result success/failure
            if r[0] == "tool_result" and r[1]:
                try:
                    content = json.loads(r[1])
                    evt["tool_name"] = content.get("name", "")
                    result_str = str(content.get("result", ""))[:200]
                    evt["result_preview"] = result_str
                    if "Error" in result_str or "error" in result_str:
                        evt["failed"] = True
                        name = content.get("name", "unknown")
                        fail_counts[name] = fail_counts.get(name, 0) + 1
                except (json.JSONDecodeError, TypeError):
                    pass
            elif r[0] == "tool_call" and r[1]:
                try:
                    content = json.loads(r[1])
                    evt["tool_name"] = content.get("name", "")
                except (json.JSONDecodeError, TypeError):
                    pass
            events.append(evt)
        result["event_summary"] = events

        # Auto-detect focus from events: scan for the most relevant signal.
        if focus == "auto":
            has_failure = any(e.get("failed") for e in events)
            has_missing_provenance = any(
                e.get("type") == "tool_result" and e.get("result_preview")
                and "data_source" not in e.get("result_preview", "")
                for e in events
            )
            if has_failure:
                focus = "skill_failure"
            elif has_missing_provenance:
                focus = "data_quality"
            else:
                focus = "unexpected_result"
            result["focus"] = focus

        # Repeated failure hint
        for name, count in fail_counts.items():
            if count >= 2:
                hints.append(f"Skill '{name}' failed {count} times in this session")

        # 2. Skill selection history — candidate scores, reasoning, outcomes
        from api.models.skill import SkillSelectionEvent
        sel_rows = (
            db.query(
                SkillSelectionEvent.skill_name, SkillSelectionEvent.selected_skills,
                SkillSelectionEvent.selection_reasoning, SkillSelectionEvent.execution_success,
                SkillSelectionEvent.execution_time_ms, SkillSelectionEvent.created_at,
            )
            .filter(SkillSelectionEvent.session_id == session_id)
            .order_by(SkillSelectionEvent.created_at.desc())
            .limit(5)
            .all()
        )
        result["skill_history"] = [
            {
                "skill": r[0], "selected": r[1], "reasoning": (r[2] or "")[:200],
                "success": bool(r[3]) if r[3] is not None else None,
                "time_ms": r[4], "ts": str(r[5]) if r[5] else None,
            }
            for r in sel_rows
        ]

        # 3. Past lessons — procedural memories relevant to this session
        try:
            from core.memory.store import MemoryStore
            from core.memory.types import MemoryType
            store = MemoryStore(SessionLocal)
            memories = store.list_active(user_id, MemoryType.PROCEDURAL)
            result["past_lessons"] = [m.content for m in memories[:5]]
            # Match hint
            for m in memories[:5]:
                for name in fail_counts:
                    if name in m.content:
                        hints.append(f"Past lesson matches: {m.content[:150]}")
                        break
        except Exception:
            result["past_lessons"] = []

        # 4. Implicit feedback signals
        try:
            from api.models.context import PromptFeedback
            user_event_ids = [
                r[0] for r in
                db.query(EventModel.event_id)
                .filter(EventModel.session_id == session_id, EventModel.event_type == "user_query")
                .all()
            ]
            if user_event_ids:
                fb_rows = (
                    db.query(PromptFeedback.user_comment, PromptFeedback.created_at)
                    .filter(PromptFeedback.llm_request_id.in_(user_event_ids))
                    .order_by(PromptFeedback.created_at.desc())
                    .limit(5)
                    .all()
                )
            else:
                fb_rows = []
            result["feedback_signals"] = [
                {"signal": r[0], "ts": str(r[1]) if r[1] else None}
                for r in fb_rows
            ]
        except Exception:
            result["feedback_signals"] = []

        # 5. Data quality hints — check tool results for missing provenance
        for evt in events:
            if evt.get("type") == "tool_result" and evt.get("result_preview"):
                preview = evt.get("result_preview", "")
                if "data_source" not in preview and evt.get("tool_name"):
                    hints.append(f"Tool '{evt['tool_name']}' result has no data_source provenance")
                    break  # one hint is enough

        # 6. Tool selection: cloud skills, edge tools, usage counts
        if focus in ("tool_selection", "auto"):
            _gather_tool_selection(session_id, question, db, hints, result)

        # 7. Cross-session history: similar queries from past sessions
        if focus in ("history", "auto"):
            _gather_history(session_id, user_id, question, db, result)

        # 8. Token summary — aggregated from LLM response events
        result["token_summary"] = {
            "total_prompt_tokens": total_prompt,
            "total_completion_tokens": total_completion,
            "total_tokens": total_prompt + total_completion,
            "llm_calls": llm_calls,
            "by_model": {
                model: {"prompt_tokens": v["prompt"], "completion_tokens": v["completion"], "calls": v["calls"]}
                for model, v in cost_by_model.items()
            },
        }

        # 9. Tool quality summary — from tool_result_quality events (if firewall enabled)
        try:
            tq_rows = (
                db.query(EventModel.event_metadata)
                .filter(
                    EventModel.session_id == session_id,
                    EventModel.event_type == "tool_result_quality",
                )
                .order_by(EventModel.created_at.desc())
                .limit(20)
                .all()
            )
            quality_items = []
            for (meta,) in tq_rows:
                if not meta:
                    continue
                m = meta if isinstance(meta, dict) else {}
                try:
                    if isinstance(meta, str):
                        m = json.loads(meta)
                except (json.JSONDecodeError, TypeError):
                    continue
                grade = m.get("quality_grade", "")
                if grade and grade != "complete":
                    quality_items.append({
                        "tool": m.get("tool_name", "unknown"),
                        "grade": grade,
                        "score": m.get("quality_score"),
                        "missing_fields": m.get("missing_fields", []),
                    })
            result["tool_quality_summary"] = quality_items
        except Exception:
            result["tool_quality_summary"] = []

        if total_prompt > 50000:
            hints.append(f"High token usage: {total_prompt + total_completion:,} total tokens across {llm_calls} LLM calls")

    result["diagnosis_hints"] = hints
    return result


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

    # ── Tool Result Quality Firewall (pre-LLM gate) ─────────────────────
    # Assess and annotate tool results BEFORE they enter the context window
    # so the LLM can respond honestly about data limitations.
    tool_quality_assessments: list[dict[str, Any]] = []
    if _TOOL_QUALITY_ENABLED and tool_results:
        for tr in tool_results:
            tr_name = tr.get("name", "")
            tr_data = tr.get("result", "")
            assessment = _assess_tool_result(tr_name, tr_data)
            if assessment.needs_annotation:
                # Annotate in-place so merged history carries the signal
                annotated = _annotate_tool_result(tr, assessment)
                tr.update(annotated)
            tool_quality_assessments.append({
                "tool_name": assessment.tool_name,
                "score": assessment.score,
                "grade": assessment.grade,
                "signals": assessment.signals,
                "stale": assessment.stale,
            })
    entry["tool_quality_assessments"] = tool_quality_assessments

    # History integrity: merge incoming tool_results into the correct
    # position in history, then heal any remaining orphaned tool_calls.
    # This unified operation handles all edge-cloud failure combinations:
    # edge disconnect, cloud restart, partial results, etc.
    consumed = _merge_tool_results_into_history(history, tool_results)

    # Append new user messages from edge
    for msg in messages:
        if msg.get("role") and msg.get("content"):
            history.append(msg)

    # Append unconsumed tool_results only if their tool_call_id exists in the
    # last assistant message's tool_calls (normal in-memory path).  Unknown
    # IDs are dropped — appending them would create orphaned tool messages
    # that violate the OpenAI message sequence contract.
    if tool_results:
        last_tc_ids: set[str] = set()
        for m in reversed(history):
            if m.get("role") == "assistant" and m.get("tool_calls"):
                last_tc_ids = {tc["id"] for tc in m["tool_calls"]}
                break
        for tr in tool_results:
            tc_id = tr.get("tool_call_id", "") if isinstance(tr, dict) else ""
            if tc_id not in consumed and tc_id in last_tc_ids:
                result = tr.get("result", "")
                history.append({
                    "role": "tool",
                    "tool_call_id": tc_id,
                    "content": str(result) if not isinstance(result, str) else result,
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
        from api.models.agent import Event as EventModel
        snap_row = (
            db.query(EventModel.content, EventModel.created_at)
            .filter(EventModel.session_id == session_id,
                    EventModel.event_type == "session_history_snapshot")
            .order_by(EventModel.created_at.desc())
            .first()
        )
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
                    _event_types = ('user_query', 'llm_response', 'tool_call', 'tool_result')
                    post_rows = (
                        db.query(EventModel.event_type, EventModel.content, EventModel.event_metadata)
                        .filter(
                            EventModel.session_id == session_id,
                            EventModel.event_type.in_(_event_types),
                            EventModel.created_at > snap_ts,
                        )
                        .order_by(EventModel.created_at.asc())
                        .limit(_MAX_RECOVERY_EVENTS)
                        .all()
                    )
                    if post_rows:
                        history = _append_recovered_events(history, post_rows)

                return history, assembled.sections
    except Exception as e:
        logger.debug("Snapshot recovery failed, falling back to events: %s", e)

    # Fallback: event-by-event reconstruction
    try:
        from api.models.agent import Event as EventModel
        _event_types = ('user_query', 'llm_response', 'tool_call', 'tool_result')
        rows = (
            db.query(EventModel.event_type, EventModel.content, EventModel.event_metadata)
            .filter(
                EventModel.session_id == session_id,
                EventModel.event_type.in_(_event_types),
            )
            .order_by(EventModel.created_at.asc())
            .limit(_MAX_RECOVERY_EVENTS)
            .all()
        )
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
    token_usage: dict[str, int] | None = None,
    llm_params: dict[str, Any] | None = None,
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

    # Resolve skill versions for tool events (best-effort, single query).
    # Returns the latest active version per skill name.
    # Uses a module-level TTL cache to avoid repeated DB hits on high-frequency turns.
    skill_versions: dict[str, str] = {}
    try:
        all_names = set()
        for tr in (tool_results or []):
            all_names.add(tr.get("name", ""))
        for tc in (tool_calls or []):
            all_names.add(tc.get("function", {}).get("name", ""))
        all_names.discard("")
        if all_names:
            skill_versions = _resolve_skill_versions(all_names)
    except SQLAlchemyError:
        logger.warning("Skill version resolution failed", exc_info=True)

    # Phase 1: persist user query
    try:
        if user_content:
            user_ev = el.create_user_query(user_id=user_id, session_id=session_id, content=user_content)
            parent_event_id = user_ev.event_id
            causal_chain_id = user_ev.causal_chain_id
    except Exception as e:
        logger.warning("Phase 1 (user_query) failed: %s", e)

    # Phase 2: persist tool results from edge + backfill selection metrics
    try:
        if tool_results:
            for tr in tool_results:
                tr_name = tr.get("name", "")
                meta = {"source": "edge", "tool_call_id": tr.get("tool_call_id"), "name": tr_name}
                if tr_name == "get_agent_info":
                    meta["introspection"] = True
                el.create_stream_event(
                    user_id=user_id, session_id=session_id,
                    event_type="tool_result",
                    content=json.dumps({"name": tr_name, "result": tr.get("result", "")[:2000]}),
                    parent_event_id=parent_event_id,
                    causal_chain_id=causal_chain_id,
                    metadata=meta,
                    skill_name=tr_name,
                    skill_version=skill_versions.get(tr_name),
                )
            # Backfill execution metrics on the most recent skill_selection_event
            try:
                from core.agent.turn_hooks import TurnHooks
                _bh = TurnHooks(SessionLocal)
                _bh.backfill_selection_metrics(session_id, tool_results)
            except Exception:
                logger.debug("Backfill selection metrics failed", exc_info=True)
    except Exception as e:
        logger.warning("Phase 2 (tool_results) failed: %s", e)

    # Phase 2b: persist tool result quality assessments
    try:
        if _TOOL_QUALITY_ENABLED:
            assessments = _session_cache.get(session_id, {}).get("tool_quality_assessments", [])
            for qa in assessments:
                if qa["grade"] != "complete":
                    el.create_stream_event(
                        user_id=user_id, session_id=session_id,
                        event_type="tool_result_quality",
                        content=json.dumps(qa),
                        parent_event_id=parent_event_id,
                        causal_chain_id=causal_chain_id,
                        metadata={
                            "tool_name": qa["tool_name"],
                            "quality_score": qa["score"],
                            "quality_grade": qa["grade"],
                            "signals": qa["signals"],
                            "stale": qa["stale"],
                        },
                    )
    except Exception as e:
        logger.warning("Phase 2b (tool_quality) failed: %s", e, exc_info=True)

    # Phase 3: persist tool calls + LLM response + history snapshot
    try:
        if tool_calls:
            for tc in tool_calls:
                tc_id = tc.get("id", "")
                tc_func = tc.get("function", {})
                tc_name = tc_func.get("name", "")
                el.create_stream_event(
                    user_id=user_id, session_id=session_id,
                    event_type="tool_call",
                    content=json.dumps({"tool_call_id": tc_id, "name": tc_name, "arguments": tc_func.get("arguments", "{}")}),
                    parent_event_id=parent_event_id,
                    causal_chain_id=causal_chain_id,
                    metadata={"tool_call_id": tc_id, "name": tc_name},
                    skill_name=tc_name,
                    skill_version=skill_versions.get(tc_name),
                )

        # Track LLM response event_id for snapshot linking
        llm_response_event_id: str | None = None
        if full_text or tool_calls:
            llm_resp_ev = el.create_llm_response(
                user_id=user_id, session_id=session_id,
                content=full_text,
                agent_id="dev-agent", agent_version="0.1.0",
                parent_event_id=parent_event_id,
                causal_chain_id=causal_chain_id,
                llm_model_used=model_used,
                token_usage=token_usage,
                llm_params=llm_params,
            )
            llm_response_event_id = llm_resp_ev.event_id

        if history and turn_count > 0 and turn_count % _SNAPSHOT_TURN_INTERVAL == 0:
            el.create_stream_event(
                user_id=user_id, session_id=session_id,
                event_type="session_history_snapshot",
                content=json.dumps(history),
                parent_event_id=parent_event_id,
                causal_chain_id=causal_chain_id,
                metadata={"turn_count": turn_count},
            )

        # Link event IDs to context snapshot so audit can trace
        # snapshot → user query (request) → LLM response.
        # parent_event_id = user query event; llm_response_event_id = LLM reply.
        if context_capture_id and parent_event_id:
            from core.context.manager import ContextManager
            ContextManager.update_snapshot_llm_ids(
                SessionLocal,
                context_capture_id,
                llm_request_id=parent_event_id,
                llm_response_id=llm_response_event_id,
            )
    except Exception as e:
        logger.warning("Phase 3 (llm_response/snapshot) failed: %s", e)

    # Phase 4: post-turn hooks (decision audit, skill selection, observer, feedback)
    try:
        from core.agent.turn_hooks import TurnHooks
        hooks = TurnHooks(SessionLocal, llm_client=_get_shared_llm_client(), embed_fn=_get_shared_embed_fn())

        if parent_event_id:
            hooks.record_ctx_decision_audits(
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

        hooks.detect_reflection_learning(session_id, user_id, tool_calls, tool_results)
    except Exception as e:
        logger.warning("Phase 4 (TurnHooks) failed: %s", e)


# Lazy-initialized shared LLM client for background tasks (Observer).
# Avoids constructing a new LLMClient per turn (expensive: DB queries + provider init).
_shared_llm_client = None
_shared_llm_lock = threading.Lock()
_shared_embed_fn = _UNSET = object()
_shared_embed_lock = threading.Lock()


def _get_shared_llm_client():
    """Get or create a shared LLMClient for background tasks."""
    global _shared_llm_client
    if _shared_llm_client is None:
        with _shared_llm_lock:
            if _shared_llm_client is None:
                from core.llm.client import LLMClient
                _shared_llm_client = LLMClient(SessionLocal)
    return _shared_llm_client


# Lazy-initialized shared SkillRegistry for cloud skill execution in /chat/turn.
_shared_skill_registry = None
_shared_skill_registry_lock = threading.Lock()


def _get_shared_skill_registry():
    """Get or create a shared SkillRegistry with builtin cloud skills."""
    global _shared_skill_registry
    if _shared_skill_registry is None:
        with _shared_skill_registry_lock:
            if _shared_skill_registry is None:
                from core.skills.registry import SkillRegistry
                from core.skills.builtin import register_builtin_skills
                from core.code_executor import CodeExecutor
                from core.runtime import IsolationLevel, create_runtime
                registry = SkillRegistry(SessionLocal)
                code_executor = CodeExecutor(
                    runtime=create_runtime(min_isolation=IsolationLevel.PROCESS),
                    db_factory=SessionLocal,
                )
                register_builtin_skills(registry, SessionLocal, code_executor=code_executor)
                _shared_skill_registry = registry
    return _shared_skill_registry


def _get_cloud_skill_schemas(registry) -> list[dict[str, Any]]:
    """Get OpenAI tool schemas for all in-memory cloud skills.

    Accesses registry._skills directly — SkillCatalog has no public iterator
    for in-memory Skill instances.  Read-only traversal.
    """
    schemas = []
    seen: set[str] = set()
    for key, skill in registry._skills.items():
        if "@" in key:  # skip versioned aliases
            continue
        if skill.name in seen:
            continue
        seen.add(skill.name)
        try:
            schemas.append(skill.to_openai_schema())
        except Exception as e:
            logger.debug("Failed to get schema for cloud skill %s: %s", skill.name, e)
    return schemas


async def _execute_cloud_skill(registry, tc_name: str, tc_args: dict[str, Any]) -> str:
    """Execute a cloud skill server-side and return result as string."""
    from core.skills.base import Skill as SkillBase
    from core.exceptions import SkillNotFoundError
    try:
        skill = registry.get(tc_name)
    except SkillNotFoundError:
        return json.dumps({"error": f"Cloud skill '{tc_name}' not found"})
    try:
        if hasattr(skill, '_input_cls') and skill._input_cls is not None:
            validated = skill.validate_input(tc_args)
            output = await skill.execute(validated)
            if hasattr(output, 'model_dump'):
                data = output.model_dump(exclude={"cost"}, exclude_none=True)
                return json.dumps(data, ensure_ascii=False, default=str)
            return str(getattr(output, 'result', output))
        else:
            return await skill.execute(**tc_args)
    except Exception as e:
        logger.warning("Cloud skill %s failed: %s", tc_name, e)
        retryable = "rate" in str(e).lower() or "timeout" in type(e).__name__.lower()
        return json.dumps({"error": f"{type(e).__name__}: {e}", "retryable": retryable})


def _get_shared_embed_fn():
    """Get or create a shared embed_fn for memory pipeline."""
    global _shared_embed_fn
    if _shared_embed_fn is _UNSET:
        with _shared_embed_lock:
            if _shared_embed_fn is _UNSET:
                try:
                    from core.context.embeddings import get_embedding_client
                    _shared_embed_fn = get_embedding_client().embed
                except Exception:
                    _shared_embed_fn = None
    return _shared_embed_fn


@router.get("/chat/session/{session_id}/reflect")
async def reflect_session(
    session_id: str,
    current_user: Annotated[dict, Depends(get_current_user)],
    focus: _ReflectFocus = Query(default="auto", description="Focus: auto, skill_failure, unexpected_result, data_quality, tool_selection, history"),
    last_n: int = Query(default=20, ge=1, le=100),
    question: str = Query(default="", description="Optional: what to investigate (for tool_selection focus)"),
):
    """Unified diagnostic endpoint: event trails, skill decisions, tool selection, cross-session history."""
    user_id = current_user["user_id"]
    _verify_session_owner(user_id, session_id)

    import asyncio
    return await asyncio.to_thread(
        _build_reflect_evidence, session_id, user_id, focus, last_n, question,
    )


# Keep decision-trace as alias for backward compatibility
@router.get("/chat/session/{session_id}/decision-trace")
async def decision_trace(
    session_id: str,
    current_user: Annotated[dict, Depends(get_current_user)],
    question: str = Query(default="", description="What to investigate"),
):
    """Alias for reflect with focus=tool_selection. Prefer /reflect."""
    user_id = current_user["user_id"]
    _verify_session_owner(user_id, session_id)

    import asyncio
    return await asyncio.to_thread(
        _build_reflect_evidence, session_id, user_id, "tool_selection", 20, question,
    )


@router.post("/chat/turn")
async def chat_turn(
    request: ChatTurnRequest,
    current_user: Annotated[dict, Depends(get_current_user)],
):
    """One LLM turn in the edge-cloud agentic loop.

    Edge sends messages + tool_results → cloud does context enrichment + LLM call →
    returns SSE stream of text_delta, tool_call, usage, turn_complete events.
    """

    # Auth/validation errors are caught by exception handlers in main.py.
    # The generator-internal try/except handles session lookup, LLM, and DB errors.
    async def event_generator():
        try:
            _turn_start = time.monotonic()
            user_id = current_user["user_id"]
            db = SessionLocal()
            try:
                session_id = _ensure_session(db, user_id, request.session_id, request.agent_id)
            finally:
                db.close()

            # Detect tool changes: compare new edge_tools with cached set.
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

            # Merge cloud skill schemas into tools_schema so LLM can call them.
            # Cloud skills are executed server-side (not sent to edge).
            # Only inject when edge is in tool-calling mode (sent edge_tools).
            cloud_skill_names: set[str] = set()
            cloud_registry = None
            merged_tools_schema = tools_schema
            if tools_schema:
                try:
                    cloud_registry = _get_shared_skill_registry()
                    cloud_schemas = _get_cloud_skill_schemas(cloud_registry)
                    edge_tool_names = _tool_names(tools_schema)
                    cloud_schemas = [s for s in cloud_schemas
                                    if s.get("function", {}).get("name", "") not in edge_tool_names]
                    cloud_skill_names = {s.get("function", {}).get("name", "") for s in cloud_schemas}
                    if cloud_schemas:
                        merged_tools_schema = tools_schema + cloud_schemas
                except Exception as e:
                    logger.debug("Cloud skill loading skipped: %s", e)

            import asyncio

            def _build_sync():
                db = SessionLocal()
                try:
                    return _build_turn_messages(
                        db, user_id, session_id,
                        request.messages, request.tool_results, request.project_rules,
                        agent_id=request.agent_id,
                        edge_tools=merged_tools_schema,
                        edge_profile=request.edge_profile.model_dump(exclude_none=True) if request.edge_profile else None,
                        force_rebuild_system=tools_changed,
                        username=current_user.get("username"),
                    )
                finally:
                    db.close()

            llm_messages, snapshot_id = await asyncio.to_thread(_build_sync)
            _get_shared_embed_fn()

            model = request.model
            task_hint = _classify_task(request.messages)

            yield f"data: {json.dumps({'type': 'session_info', 'session_id': session_id})}\n\n"

            # ── Quality Badge SSE (§5.4) — emit before LLM response ──────
            if _TOOL_QUALITY_ENABLED:
                for qa in _get_or_create_session_entry(session_id).get("tool_quality_assessments", []):
                    if qa["grade"] != "complete":
                        yield f"data: {json.dumps({'type': 'tool_result_quality', 'tool_name': qa['tool_name'], 'grade': qa['grade'], 'score': qa['score'], 'signals': qa['signals']})}\n\n"

            llm = _get_shared_llm_client()
            with llm.request_context(user_id=user_id):

                full_text = ""
                tool_calls: list[dict[str, Any]] = []
                usage: dict[str, int] = {}
                llm_params: dict[str, Any] = {
                    k: v for k, v in {
                        "temperature": llm.config.get("temperature", 0.7),
                        "max_tokens": llm.config.get("max_tokens"),
                    }.items() if v is not None
                }

                _deadline = _turn_start + SERVER_TURN_TIMEOUT_S
                _explain_steps: list[dict[str, Any]] = []
                _total_prompt_tokens = 0
                _total_completion_tokens = 0
                _has_usage = False

                async def _next_with_timeout(aiter: AsyncIterator) -> Any:
                    remaining = _deadline - time.monotonic()
                    if remaining <= 0:
                        raise asyncio.TimeoutError
                    return await asyncio.wait_for(aiter.__anext__(), timeout=remaining)

                # Inner loop: if LLM calls cloud skills, execute them server-side
                # and feed results back to LLM. Repeat until LLM returns only
                # edge tool_calls or a final text answer.
                _MAX_CLOUD_LOOPS = 5
                _current_llm_messages = llm_messages

                for _cloud_loop in range(_MAX_CLOUD_LOOPS + 1):
                    _loop_text = ""
                    _loop_tool_calls: list[dict[str, Any]] = []
                    _llm_start = time.monotonic()

                    stream: AsyncIterator = (
                        llm.chat_with_tools_stream(
                            _current_llm_messages, merged_tools_schema, model=model, task_hint=task_hint,
                        ) if merged_tools_schema else
                        llm.chat_stream(
                            _current_llm_messages, user_id, session_id, model=model,
                        )
                    )
                    _timed_out = False
                    try:
                        while True:
                            chunk = await _next_with_timeout(stream)
                            if chunk["type"] == "text":
                                _loop_text += chunk["content"]
                                yield f"data: {json.dumps({'type': 'text_delta', 'content': chunk['content']})}\n\n"
                            elif chunk["type"] == "tool_call":
                                _loop_tool_calls.append(chunk["data"])
                            elif chunk["type"] == "tool_call_start":
                                yield f"data: {json.dumps({'type': 'tool_call_start', 'name': chunk['name']})}\n\n"
                            elif chunk["type"] == "usage":
                                p_tok = chunk.get("prompt", 0)
                                c_tok = chunk.get("completion", 0)
                                _total_prompt_tokens += p_tok
                                _total_completion_tokens += c_tok
                                _has_usage = True
                                usage = {"prompt": p_tok, "completion": c_tok, "total": p_tok + c_tok}
                                yield f"data: {json.dumps({'type': 'usage', 'prompt_tokens': p_tok, 'completion_tokens': c_tok, 'cache_read_tokens': chunk.get('cache_read', 0)})}\n\n"
                    except StopAsyncIteration:
                        pass
                    except (asyncio.TimeoutError, TimeoutError):
                        yield f"data: {json.dumps({'type': 'error', 'message': 'Turn exceeded server time limit', 'code': 'turn_timeout', 'retryable': False})}\n\n"
                        _timed_out = True

                    _llm_elapsed = time.monotonic() - _llm_start
                    if request.explain:
                        # Use None for token counts when provider didn't send usage data,
                        # so the client can distinguish "zero tokens" from "unknown".
                        if _has_usage:
                            _step_p = _total_prompt_tokens - sum(s.get("in", 0) for s in _explain_steps if s["step"] == "llm" and s.get("in") is not None)
                            _step_c = _total_completion_tokens - sum(s.get("out", 0) for s in _explain_steps if s["step"] == "llm" and s.get("out") is not None)
                        else:
                            _step_p = None
                            _step_c = None
                        _explain_steps.append({
                            "step": "llm", "loop": _cloud_loop,
                            "duration_ms": round(_llm_elapsed * 1000),
                            "in": _step_p, "out": _step_c,
                            "tool_calls": len(_loop_tool_calls),
                        })

                    full_text += _loop_text

                    if _timed_out:
                        return

                    if not _loop_tool_calls:
                        # No tool calls — final answer, exit loop.
                        break

                    # Partition tool_calls into cloud vs edge.
                    cloud_tcs = []
                    edge_tcs = []
                    for tc in _loop_tool_calls:
                        tc_name = tc.get("function", {}).get("name", "")
                        if tc_name in cloud_skill_names:
                            cloud_tcs.append(tc)
                        else:
                            edge_tcs.append(tc)

                    if not cloud_tcs:
                        # All tool_calls are edge — pass through to client.
                        tool_calls = _loop_tool_calls
                        break

                    # Execute cloud skills server-side.
                    if not cloud_registry:
                        # Registry unavailable — treat as edge tool_calls.
                        tool_calls = _loop_tool_calls
                        break

                    # Execute cloud skills server-side.
                    # Build assistant message with tool_calls for conversation history.
                    assistant_msg_loop: dict[str, Any] = {"role": "assistant"}
                    if _loop_text:
                        assistant_msg_loop["content"] = _loop_text
                    assistant_msg_loop["tool_calls"] = [
                        {"id": tc.get("id", ""), "type": "function", "function": tc.get("function", {})}
                        for tc in cloud_tcs
                    ]
                    _current_llm_messages = _current_llm_messages + [assistant_msg_loop]

                    for tc in cloud_tcs:
                        tc_name = tc.get("function", {}).get("name", "?")
                        tc_id = tc.get("id", "")
                        args_raw = tc.get("function", {}).get("arguments", "") or "{}"
                        try:
                            tc_args = json.loads(args_raw) if isinstance(args_raw, str) else args_raw
                        except json.JSONDecodeError:
                            tc_args = {}

                        yield f"data: {json.dumps({'type': 'tool_call_start', 'name': tc_name})}\n\n"
                        _skill_start = time.monotonic()
                        cloud_result = await _execute_cloud_skill(cloud_registry, tc_name, tc_args)
                        if request.explain:
                            _explain_steps.append({
                                "step": "cloud_skill", "name": tc_name,
                                "duration_ms": round((time.monotonic() - _skill_start) * 1000),
                                "in_bytes": len(json.dumps(tc_args)),
                                "out_bytes": len(cloud_result),
                            })
                        # Record cloud skill execution as event for decision_trace visibility.
                        try:
                            from api.models.agent import Event as EventModel
                            _db = SessionLocal()
                            _db.add(EventModel(
                                event_id=str(__import__('uuid').uuid4()),
                                session_id=session_id,
                                user_id=user_id,
                                agent_id=request.agent_id or "edge",
                                event_type="tool_call",
                                content=json.dumps({"name": tc_name, "arguments": tc_args, "source": "cloud"}),
                                causal_chain_id=session_id,
                            ))
                            _db.commit()
                            _db.close()
                        except Exception:
                            pass
                        yield f"data: {json.dumps({'type': 'cloud_tool_result', 'name': tc_name, 'result': cloud_result[:500]})}\n\n"
                        # Quality badge for cloud tool results
                        if _TOOL_QUALITY_ENABLED:
                            _cqa = _assess_tool_result(tc_name, cloud_result)
                            if _cqa.needs_annotation:
                                yield f"data: {json.dumps({'type': 'tool_result_quality', 'tool_name': tc_name, 'grade': _cqa.grade, 'score': _cqa.score, 'signals': _cqa.signals[:5]})}\n\n"
                                cloud_result = _annotate_tool_result({"result": cloud_result}, _cqa)["result"]
                        # Append tool result to messages for next LLM call.
                        _current_llm_messages = _current_llm_messages + [
                            {"role": "tool", "tool_call_id": tc_id, "content": cloud_result}
                        ]

                    # If there are also edge tool_calls, emit them and break.
                    # The edge will execute them and send results in the next /chat/turn.
                    if edge_tcs:
                        tool_calls = edge_tcs
                        break

                    # All tool_calls were cloud — loop back to LLM with results.
                    continue
                else:
                    # Exhausted cloud loop limit — break out with whatever we have.
                    logger.warning("Cloud skill loop limit reached (%d)", _MAX_CLOUD_LOOPS)

                # Emit accumulated edge tool calls to client.
                for tc in tool_calls:
                    tc_name = tc.get("function", {}).get("name", "?")
                    if tc.get("_truncated"):
                        logger.warning("tool_call %s truncated by max_tokens", tc_name)
                        parsed_args = {"_parse_error": (
                            "Your output was truncated by max_tokens before the tool_call "
                            "arguments were complete. The JSON is cut off and cannot be parsed. "
                            "Please retry with a shorter approach — for example, write smaller "
                            "sections of code at a time instead of the entire file at once."
                        )}
                    else:
                        args = tc.get("function", {}).get("arguments", "") or "{}"
                        try:
                            parsed_args = json.loads(args) if isinstance(args, str) else args
                        except json.JSONDecodeError:
                            parsed_args = _try_repair_tool_args(tc_name, args)
                            if parsed_args is None:
                                logger.warning("Malformed tool_call arguments for %s: %s",
                                               tc_name, args[:200])
                                parsed_args = {"_parse_error": f"Malformed arguments JSON: {args[:200]}"}
                    yield f"data: {json.dumps({'type': 'tool_call', 'id': tc.get('id', ''), 'name': tc_name, 'arguments': parsed_args})}\n\n"

                # Update session cache
                _entry = _get_or_create_session_entry(session_id)
                assistant_msg: dict[str, Any] = {"role": "assistant", "content": full_text}
                if tool_calls:
                    assistant_msg["tool_calls"] = tool_calls
                _entry.setdefault("history", []).append(assistant_msg)
                _entry["turn_count"] = _entry.get("turn_count", 0) + 1
                _session_cache[session_id] = _entry
                current_turn_count = _entry["turn_count"]
                current_history = list(_entry.get("history", []))

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
                token_usage=usage if usage else None,
                llm_params=llm_params,
                history=copy.deepcopy(current_history),
                turn_count=current_turn_count,
            )
            _t = threading.Thread(target=_persist_turn_events, kwargs=_persist_args, daemon=True)
            _persist_threads.append(_t)
            _t.start()

            # Pre-completion firewall verification (must arrive before turn_complete
            # because edge clients may close the connection on turn_complete).
            firewall_warning: dict[str, Any] | None = None
            _agg_tool_quality: float | None = None
            if _TOOL_QUALITY_ENABLED:
                _qas = _get_or_create_session_entry(session_id).get("tool_quality_assessments", [])
                if _qas:
                    _agg_tool_quality = sum(q["score"] for q in _qas) / len(_qas)
            if full_text and snapshot_id:
                try:
                    import asyncio
                    from core.context.manager import ContextManager
                    from core.verification.firewall import HallucinationFirewall
                    ctx_mgr = ContextManager(SessionLocal)
                    fw = HallucinationFirewall(SessionLocal, context_manager=ctx_mgr)
                    result = await asyncio.to_thread(fw.verify_response, full_text, snapshot_id,
                                                      tool_quality_score=_agg_tool_quality)
                    if not result.safe_to_deliver:
                        firewall_warning = {'type': 'warning', 'message': 'Response may contain unverified claims', 'claims_failed': result.claims_failed}
                except Exception as e:
                    logger.debug("Firewall verification skipped: %s", e)

            if firewall_warning:
                yield f"data: {json.dumps(firewall_warning)}\n\n"

            if request.explain:
                _total_ms = round((time.monotonic() - _turn_start) * 1000)
                explain_event = {
                    "type": "explain",
                    "total_ms": _total_ms,
                    "prompt_tokens": _total_prompt_tokens if _has_usage else None,
                    "completion_tokens": _total_completion_tokens if _has_usage else None,
                    "steps": _explain_steps,
                }
                yield f"data: {json.dumps(explain_event)}\n\n"

            yield f"data: {json.dumps({'type': 'turn_complete', 'has_tool_calls': len(tool_calls) > 0})}\n\n"

        except Exception as e:
            logger.error("chat_turn error: %s", e, exc_info=True)
            from core.llm.client import BudgetExceededError
            from core.exceptions import LLMRateLimitError, LLMTimeoutError, TransientError
            from sqlalchemy.exc import SQLAlchemyError
            err: dict[str, Any] = {"type": "error", "message": str(e)}
            if isinstance(e, HTTPException):
                err["message"] = e.detail
                err.update(code=status_to_error_code(e.status_code), retryable=False)
            elif isinstance(e, BudgetExceededError):
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
        _with_heartbeat(event_generator()),
        media_type="text/event-stream",
        headers=SSE_HEADERS,
    )
