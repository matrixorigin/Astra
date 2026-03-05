"""Chat API endpoints — unified conversation entry point with durable AgentRun."""

import asyncio
import json
import os
import threading
import time
from collections import OrderedDict
from collections.abc import AsyncIterator
from typing import Annotated, Any, Literal, Protocol, runtime_checkable

from fastapi import APIRouter, Depends, HTTPException, Query
from fastapi.responses import StreamingResponse
from pydantic import BaseModel, ConfigDict, Field
from sqlalchemy.exc import SQLAlchemyError
from sqlalchemy.orm import Session
from uuid_utils import uuid7

from api.database import SessionLocal
from api.dependencies import get_current_user
from api.sse_errors import SSE_HEADERS, status_to_error_code
from core.history_utils import (
    append_recovered_events as _append_recovered_events,
)
from core.history_utils import (
    merge_tool_results_into_history as _merge_tool_results_into_history,
)
from core.logging_config import get_logger
from core.verification.tool_quality import (
    annotate_tool_result as _annotate_tool_result,
)
from core.verification.tool_quality import (
    assess_tool_result as _assess_tool_result,
)

logger = get_logger(__name__)
router = APIRouter()

# ---------------------------------------------------------------------------
# SSE Heartbeat (§3.1 of edge-cloud-execution.md)
# ---------------------------------------------------------------------------

HEARTBEAT_INTERVAL_S = 15
SERVER_TURN_TIMEOUT_S = 240

# ---------------------------------------------------------------------------
# History Compaction (prevents context overflow)
# ---------------------------------------------------------------------------
# Target token limit for history compaction. Set conservatively to leave room
# for response tokens. This is a fallback — ideally we'd use the model's actual
# context_window, but that requires passing model info through the call chain.
# 100K tokens works for 128K models (DeepSeek, GPT-4), leaves 28K for response.
# For smaller models (64K), the LLM client's _check_context_overflow will catch
# overflow before the API call.
_HISTORY_COMPACTION_LIMIT = 100000

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

    # 4. Bare-word values: e.g. "key": advice → "key": "advice"
    #    Skip JSON literals true/false/null.
    def _quote_bare(m):
        word = m.group(1)
        if word in ("true", "false", "null"):
            return m.group(0)
        return f': "{word}"{m.group(2)}'
    s = _re.sub(r':\s*([a-zA-Z_]\w*)(\s*[,}\]])', _quote_bare, s)

    # 5. Try parsing now
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
    explain: bool | str = Field(default=False, description="Execution trace: true for normal, 'verbose' for detailed with content previews")


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
    from core.skills.catalog import SkillCatalog
    from core.verification.firewall import HallucinationFirewall

    # Create EventPipeline for async writes (feature-flagged)
    pipeline = None
    try:
        from core.events.event_logger import _PIPELINE_ENABLED
        from core.events.pipeline import EventPipeline
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

    skill_registry = SkillCatalog(db_factory, gate_trigger=gate_trigger)
    code_executor = CodeExecutor(
        runtime=create_runtime(min_isolation=IsolationLevel.PROCESS),
        db_factory=db_factory,
    )
    register_builtin_skills(skill_registry, db_factory, code_executor=code_executor)
    context_manager = ContextManager(db_factory, gate_trigger=gate_trigger)
    # Removed: SkillPipeline module deleted - using SkillCatalog directly
    selector = skill_registry  # SkillCatalog implements get_tools_schema()

    # Removed: credential_manager and skill_manager modules deleted
    # Removed: skill_manager module deleted
    skill_mgr = None  # Stubbed out
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


def _transform_event_for_client(event: dict) -> dict:
    """Transform internal event format to client-expected format.

    Internal (from RunEngine): {"event_type": "text_delta", "data": {"chunk": "..."}, ...}
    Client (SSE):              {"type": "text_delta", "content": "...", ...}

    Fields are explicitly picked per event type to avoid leaking internal
    structure.  Unknown event types are passed through with just the type
    field (no data spread) and logged at debug level.
    """
    event_type = event.get("event_type", "")
    data = event.get("data", {})

    # ── Text ──
    if event_type == "text_delta":
        return {"type": "text_delta", "content": data.get("chunk", "")}
    if event_type == "text_done":
        return {"type": "text_done", "full_text": data.get("full_text", "")}

    # ── Reasoning / CoT ──
    if event_type == "reasoning_message_content":
        return {"type": "reasoning_message_content", "content": data.get("content", "")}
    if event_type == "thinking_delta":
        return {"type": "thinking_delta", "content": data.get("chunk", "")}
    if event_type == "thinking_done":
        return {"type": "thinking_done"}

    # ── Tool use ──
    if event_type == "tool_call_start":
        return {"type": "tool_call_start", "tool": data.get("tool", ""), "call_id": data.get("call_id", "")}
    if event_type == "tool_result":
        return {"type": "tool_result", "call_id": data.get("call_id", ""), "result": data.get("result", "")}

    # ── Lifecycle ──
    if event_type == "run_started":
        return {"type": "run_started"}
    if event_type == "run_finished":
        return {"type": "run_finished"}
    if event_type == "run_error":
        return {"type": "error", "message": data.get("error", "Unknown error"), "code": "RUN_ERROR"}

    # ── Planning ──
    if event_type == "plan_created":
        return {"type": "plan_created", "plan": data.get("plan", {})}
    if event_type == "plan_step_start":
        return {"type": "plan_step_start", "step": data.get("step", "")}
    if event_type == "plan_step_done":
        return {"type": "plan_step_done", "step": data.get("step", ""), "result": data.get("result", "")}
    if event_type == "plan_revised":
        return {"type": "plan_revised", "plan": data.get("plan", {})}

    # ── Multi-agent ──
    if event_type == "agent_delegated":
        return {"type": "agent_delegated", "agent_id": data.get("agent_id", ""), "task": data.get("task", "")}
    if event_type == "agent_progress":
        return {"type": "agent_progress", "agent_id": data.get("agent_id", ""), "progress": data.get("progress", "")}
    if event_type == "agent_completed":
        return {"type": "agent_completed", "agent_id": data.get("agent_id", ""), "result": data.get("result", "")}

    # ── Keepalive ──
    if event_type == "keepalive":
        return {"type": "ping"}

    # Unknown event — pass through type only, don't spread internal data
    logger.debug("Unknown internal event_type for client: %s", event_type)
    return {"type": event_type}


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

            yield f"data: {json.dumps({'type': 'session_info', 'session_id': session_id, 'run_id': run.run_id})}\n\n"

            async for event in engine.stream_agent_run_events(run.run_id):
                # Transform internal event format to client-expected format
                client_event = _transform_event_for_client(event)
                yield f"data: {json.dumps(client_event)}\n\n"
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
# Max chars for tool result stored in DB for audit/reflect diagnostics.
from core.context.compaction import MAX_TOOL_RESULT_AUDIT_CHARS as _AUDIT_CHARS


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


_ReflectFocus = Literal["auto", "skill_failure", "unexpected_result", "data_quality", "tool_selection", "history", "performance"]


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
    """Thin wrapper kept for backward compatibility with existing tests.

    Delegates to :class:`core.agent.reflect_service.ReflectService`.
    """
    from core.agent.reflect_service import ReflectService
    svc = ReflectService(
        db_factory=SessionLocal,
        skill_registry=_get_shared_skill_registry(),
        peek_session=_peek_session_entry,
    )
    return svc.build_evidence(session_id, user_id, focus, last_n, question)


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
    explain: bool = False,
    verbose: bool = False,
) -> tuple[list[dict[str, Any]], str | None, dict[str, Any] | None]:
    """Build LLM messages from edge turn data + server-side history.

    Returns (messages, context_capture_id, memory_stats).
    context_capture_id is the snapshot saved by PromptAssembler BEFORE the LLM call;
    on turn 2+ it comes from incremental memory refresh.
    memory_stats is populated when explain=True.
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
    memory_stats: dict[str, Any] | None = None
    if not history or force_rebuild_system:
        user_query = next((m.get("content", "") for m in messages if m.get("role") == "user"), "")

        from core.context.prompt_assembler import EdgeContext, PromptAssembler
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
            explain=explain,
            verbose=verbose,
        )
        system = assembled.system_message
        context_capture_id = assembled.snapshot_id
        cached_sections = assembled.sections
        memory_stats = assembled.memory_stats
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
                        explain=explain,
                        verbose=verbose,
                    )
                    history[0] = {"role": "system", "content": refreshed.system_message}
                    context_capture_id = refreshed.snapshot_id
                    cached_sections = refreshed.sections
                    memory_stats = refreshed.memory_stats
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

    # ── History Compaction ──────────────────────────────────────────────
    # Compact history to avoid context overflow. See _HISTORY_COMPACTION_LIMIT
    # at file top for rationale on the 100K limit.
    from core.context.compaction import compact, needs_compaction
    if needs_compaction(history, _HISTORY_COMPACTION_LIMIT):
        history = compact(history, _HISTORY_COMPACTION_LIMIT)
        logger.debug("History compacted to fit context window")

    entry["history"] = history
    if cached_sections:
        entry["sections"] = cached_sections
    _session_cache[session_id] = entry
    return history, context_capture_id, memory_stats


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
    agent_id: str | None = None,
    turn_chain_id: str | None = None,
    user_query_event_id: str | None = None,
    cloud_tool_results: list[dict[str, Any]] | None = None,
) -> None:
    """Persist events for this turn: user query, tool results, LLM response.

    Also writes decision audit, skill selection, observations, implicit feedback
    via TurnHooks, and periodic history snapshots. All writes are best-effort —
    failures are logged but never block.

    Context snapshot is NOT saved here — it is saved BEFORE the LLM call by
    PromptAssembler (the correct timing). The snapshot ID is passed in via
    context_capture_id so DecisionAudit can reference it.
    """
    from core.events.event_logger import EventLogger

    user_content = next((m["content"] for m in messages if m.get("role") == "user"), None)
    el = EventLogger(SessionLocal)
    parent_event_id = user_query_event_id  # default to pre-generated ID for continuation turns
    causal_chain_id = turn_chain_id or str(uuid7())

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
            user_ev = el.create_user_query(
                user_id=user_id, session_id=session_id, content=user_content,
                causal_chain_id=causal_chain_id,
                event_id=user_query_event_id,
            )
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
                    content=json.dumps({"name": tr_name, "result": tr.get("result", "")[:_AUDIT_CHARS]}),
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
                tc_content: dict[str, Any] = {"tool_call_id": tc_id, "name": tc_name, "arguments": tc_func.get("arguments", "{}")}
                if tc.get("_source") == "cloud":
                    tc_content["source"] = "cloud"
                el.create_stream_event(
                    user_id=user_id, session_id=session_id,
                    event_type="tool_call",
                    content=json.dumps(tc_content),
                    parent_event_id=parent_event_id,
                    causal_chain_id=causal_chain_id,
                    metadata={"tool_call_id": tc_id, "name": tc_name},
                    skill_name=tc_name,
                    skill_version=skill_versions.get(tc_name),
                )

        # Track LLM response event_id for snapshot linking
        llm_response_event_id: str | None = None

        # Persist cloud tool results (server-side skill execution results)
        if cloud_tool_results:
            for ctr in cloud_tool_results:
                ctr_name = ctr.get("name", "")
                el.create_stream_event(
                    user_id=user_id, session_id=session_id,
                    event_type="tool_result",
                    content=json.dumps({"name": ctr_name, "result": ctr.get("result", "")[:_AUDIT_CHARS]}),
                    parent_event_id=parent_event_id,
                    causal_chain_id=causal_chain_id,
                    metadata={
                        "source": "cloud",
                        "tool_call_id": ctr.get("tool_call_id"),
                        "name": ctr_name,
                    },
                    skill_name=ctr_name,
                    skill_version=skill_versions.get(ctr_name),
                )

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
            # Compact snapshot: strip verbose tool result content to reduce storage.
            # Recovery only needs message structure (roles, tool_call_ids), not full results.
            compact_history = []
            for msg in history:
                if msg.get("role") == "tool":
                    content = msg.get("content", "")
                    compact_history.append({
                        **msg,
                        "content": content[:500] + " [truncated]" if len(content) > 500 else content,
                    })
                else:
                    compact_history.append(msg)
            el.create_stream_event(
                user_id=user_id, session_id=session_id,
                event_type="session_history_snapshot",
                content=json.dumps(compact_history),
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
            hooks.record_skill_selection(session_id, user_content or "", tool_calls, agent_id=agent_id, skill_versions=skill_versions)

            # Backfill execution metrics when tool_results and selection happen in the same turn
            # (cloud skills: user_query → tool_call → tool_result all in one persist call).
            # Phase 2 backfill targets the *previous* turn's selection event, so it misses
            # same-turn results. This second backfill catches the just-created selection row.
            if tool_results and tool_calls:
                try:
                    hooks.backfill_selection_metrics(session_id, tool_results)
                except Exception:
                    logger.debug("Same-turn backfill failed", exc_info=True)

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

    # Phase 5: update session activity (event_count, last_active_at)
    try:
        _sdb = SessionLocal()
        try:
            from datetime import datetime, timezone

            from sqlalchemy import text as _text
            _count = _sdb.execute(
                _text("SELECT COUNT(*) FROM agent_events WHERE session_id = :sid"),
                {"sid": session_id},
            ).scalar() or 0
            _sdb.execute(
                _text("""
                    UPDATE agent_sessions
                    SET event_count = :cnt, last_active_at = :now, updated_at = :now
                    WHERE session_id = :sid
                """),
                {"cnt": _count, "sid": session_id, "now": datetime.now(timezone.utc)},
            )
            _sdb.commit()
        finally:
            _sdb.close()
    except Exception as e:
        logger.warning("Phase 5 (session activity) failed: %s", e)


# Lazy-initialized shared LLM client for background tasks (Observer).
# Avoids constructing a new LLMClient per turn (expensive: DB queries + provider init).
_shared_llm_client = None
_shared_llm_lock = threading.Lock()
_shared_embed_fn = _UNSET = object()
_shared_embed_lock = threading.Lock()


def _get_session_tool_registry(
    session_id: str,
    edge_tools: list[dict[str, Any]],
) -> "ToolRegistry":
    """Build a ToolRegistry for one session, populated with edge tools.

    Cloud skills are added by the caller after this returns.
    The registry is rebuilt per-turn (cheap — just dict inserts).
    """
    from core.skills.tool_registry import ToolRegistry, ToolSource
    embed_fn = _get_shared_embed_fn()
    registry = ToolRegistry(embed_fn=embed_fn)
    for schema in (edge_tools or []):
        registry.register_schema(schema, ToolSource.EDGE)
    return registry


def _get_shared_llm_client():
    """Get or create a shared LLMClient for background tasks."""
    global _shared_llm_client
    if _shared_llm_client is None:
        with _shared_llm_lock:
            if _shared_llm_client is None:
                from core.llm.client import LLMClient
                _shared_llm_client = LLMClient(SessionLocal)
    return _shared_llm_client


# Lazy-initialized shared SkillCatalog for cloud skill execution in /chat/turn.
_shared_skill_registry = None
_shared_skill_registry_lock = threading.Lock()


def _get_shared_skill_registry():
    """Get or create a shared SkillCatalog with builtin cloud skills."""
    global _shared_skill_registry
    if _shared_skill_registry is None:
        with _shared_skill_registry_lock:
            if _shared_skill_registry is None:
                from core.code_executor import CodeExecutor
                from core.runtime import IsolationLevel, create_runtime
                from core.skills.builtin import register_builtin_skills
                from core.skills.catalog import SkillCatalog
                registry = SkillCatalog(SessionLocal)
                code_executor = CodeExecutor(
                    runtime=create_runtime(min_isolation=IsolationLevel.PROCESS),
                    db_factory=SessionLocal,
                )
                register_builtin_skills(registry, SessionLocal, code_executor=code_executor)
                _shared_skill_registry = registry
    return _shared_skill_registry


def _update_snapshot_tool_tokens(snapshot_id: str, actual_tool_tokens: int) -> None:
    """Update snapshot's tool_schemas token count after high-confidence optimization."""
    db = SessionLocal()
    try:
        from sqlalchemy import text
        # Get current token_budget
        row = db.execute(
            text("SELECT token_budget FROM ctx_snapshots WHERE context_capture_id = :cid"),
            {"cid": snapshot_id}
        ).fetchone()
        if row and row[0]:
            import json
            budget = json.loads(row[0]) if isinstance(row[0], str) else row[0]
            budget["tool_schemas"] = actual_tool_tokens
            db.execute(
                text("UPDATE ctx_snapshots SET token_budget = :budget WHERE context_capture_id = :cid"),
                {"budget": json.dumps(budget), "cid": snapshot_id}
            )
            db.commit()
    except Exception:
        db.rollback()
    finally:
        db.close()


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
    focus: _ReflectFocus = Query(default="auto", description="Focus: auto, skill_failure, unexpected_result, data_quality, tool_selection, history, performance"),
    last_n: int = Query(default=20, ge=1, le=100),
    question: str = Query(default="", description="Optional: what to investigate (for tool_selection focus)"),
):
    """Unified diagnostic endpoint: event trails, skill decisions, tool selection, cross-session history."""
    user_id = current_user["user_id"]
    _verify_session_owner(user_id, session_id)

    from core.agent.reflect_service import ReflectService
    svc = ReflectService(
        db_factory=SessionLocal,
        skill_registry=_get_shared_skill_registry(),
        peek_session=_peek_session_entry,
    )

    import asyncio
    return await asyncio.to_thread(
        svc.build_evidence, session_id, user_id, focus, last_n, question,
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

    from core.agent.reflect_service import ReflectService
    svc = ReflectService(
        db_factory=SessionLocal,
        skill_registry=_get_shared_skill_registry(),
        peek_session=_peek_session_entry,
    )

    import asyncio
    return await asyncio.to_thread(
        svc.build_evidence, session_id, user_id, "tool_selection", 20, question,
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

            # Causal chain + user_query event_id for this turn — both generated
            # here (streaming time) so their uuid7 timestamps reflect when the
            # turn actually happened, not when the persist thread ran.
            #
            # Continuation turns (tool_results without new user_query) reuse the
            # previous turn's chain_id so the entire multi-step tool loop shares
            # one causal chain per user intent.
            _has_new_user_query = any(m.get("role") == "user" for m in request.messages)
            _prev_entry = _peek_session_entry(session_id)
            if not _has_new_user_query and request.tool_results and _prev_entry:
                _turn_chain_id = _prev_entry.get("turn_chain_id") or str(uuid7())
                _user_query_event_id = _prev_entry.get("user_query_event_id") or str(uuid7())
            else:
                _turn_chain_id = str(uuid7())
                _user_query_event_id = str(uuid7())

            # ── Unified Tool Registry ────────────────────────────────────
            # All tools (edge + cloud) go into one registry. Selection is
            # handled by the registry: pinned tools always included, dynamic
            # tools selected via intent/prefilter/embedding per request.
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

            cloud_skill_names: set[str] = set()
            cloud_registry = None
            _cloud_tool_calls_for_persist: list[dict[str, Any]] = []
            _cloud_tool_results_for_persist: list[dict[str, Any]] = []

            # Build unified registry from all sources
            from core.skills.tool_registry import ToolRegistry, ToolSource
            _turn_registry = _get_session_tool_registry(session_id, tools_schema)

            # Add cloud skills
            if tools_schema:
                try:
                    cloud_registry = _get_shared_skill_registry()
                    cloud_schemas = _get_cloud_skill_schemas(cloud_registry)
                    edge_tool_names = _tool_names(tools_schema)
                    for cs in cloud_schemas:
                        cs_name = cs.get("function", {}).get("name", "")
                        if cs_name and cs_name not in edge_tool_names:
                            cloud_skill_names.add(cs_name)
                            _turn_registry.register_schema(
                                cs, ToolSource.CLOUD, pinned=False,
                            )
                except Exception as e:
                    logger.debug("Cloud skill loading skipped: %s", e)

            import asyncio

            # ── Tool selection via unified registry ──────────────────────
            user_query = next(
                (m.get("content", "") for m in request.messages if m.get("role") == "user"),
                "",
            )
            _cached_entry = _peek_session_entry(session_id)
            _cached_history = (_cached_entry or {}).get("history") if _cached_entry else None

            if request.tool_results and not user_query:
                # Tool-result turn: keep only tools already in use
                used_names = {tr.get("name", "") for tr in request.tool_results if tr.get("name")}
                effective_tools_schema = [
                    t.schema for t in _turn_registry.all_tools()
                    if t.name in used_names
                ] or _turn_registry.get_all_schemas()
            else:
                effective_tools_schema = _turn_registry.select(
                    user_query=user_query,
                    messages=_cached_history or request.messages,
                )
            _high_confidence_skill = None

            def _build_sync():
                db = SessionLocal()
                try:
                    _explain_on = bool(request.explain)
                    _verbose_on = request.explain == "verbose"
                    return _build_turn_messages(
                        db, user_id, session_id,
                        request.messages, request.tool_results, request.project_rules,
                        agent_id=request.agent_id,
                        edge_tools=effective_tools_schema,
                        edge_profile=request.edge_profile.model_dump(exclude_none=True) if request.edge_profile else None,
                        force_rebuild_system=tools_changed,
                        username=current_user.get("username"),
                        explain=_explain_on,
                        verbose=_verbose_on,
                    )
                finally:
                    db.close()

            llm_messages, snapshot_id, _memory_stats = await asyncio.to_thread(_build_sync)
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
                _cloud_skill_failed = False
                _cloud_skill_error_msg = ""
                # Collect cloud loop intermediate messages for session history.
                # These are assistant+tool_calls and tool results that happen
                # server-side. Without them, future turns can't see what tools
                # were used (e.g. list_prs), causing tool selection failures.
                _cloud_loop_history: list[dict[str, Any]] = []

                for _cloud_loop in range(_MAX_CLOUD_LOOPS + 1):
                    _loop_text = ""
                    _loop_tool_calls: list[dict[str, Any]] = []
                    _llm_start = time.monotonic()

                    if _cloud_skill_failed:
                        logger.warning("Cloud loop: final LLM call via chat_stream (no tools), loop=%d", _cloud_loop)
                    stream: AsyncIterator = (
                        llm.chat_with_tools_stream(
                            _current_llm_messages, effective_tools_schema, model=model, task_hint=task_hint,
                        ) if effective_tools_schema and not _cloud_skill_failed else
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
                    except Exception as _stream_err:
                        if _cloud_skill_failed:
                            # Final LLM call failed — emit the skill error directly as text
                            logger.warning("Final LLM call after cloud skill failure raised: %s", _stream_err)
                            _fallback = _cloud_skill_error_msg or "The requested operation failed."
                            _loop_text += _fallback
                            yield f"data: {json.dumps({'type': 'text_delta', 'content': _fallback})}\n\n"
                        else:
                            raise

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
                        # If cloud skill failed and LLM returned empty text, use the error message directly.
                        if _cloud_skill_failed and not _loop_text.strip():
                            _fallback = _cloud_skill_error_msg or "The requested operation failed."
                            _loop_text = f"\n\n{_fallback}"
                            full_text += _loop_text
                            yield f"data: {json.dumps({'type': 'text_delta', 'content': _loop_text})}\n\n"
                        break

                    # If a previous cloud skill failed, ignore any new tool_calls from LLM.
                    if _cloud_skill_failed:
                        logger.warning("Cloud loop: LLM returned %d tool_calls after failure (ignored), text=%r",
                                       len(_loop_tool_calls), _loop_text[:200])
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
                    # _current_llm_messages gets the full message (LLM needs content
                    # for coherent conversation), but _cloud_loop_history omits the
                    # text content because it will already appear in the final
                    # assistant_msg via full_text.  Storing it in both places causes
                    # the preamble to appear twice in session history, which makes
                    # the LLM repeat itself with increasing severity each turn.
                    _tc_entries = [
                        {"id": tc.get("id", ""), "type": "function", "function": tc.get("function", {})}
                        for tc in cloud_tcs
                    ]
                    assistant_msg_loop: dict[str, Any] = {"role": "assistant"}
                    if _loop_text:
                        assistant_msg_loop["content"] = _loop_text
                    assistant_msg_loop["tool_calls"] = _tc_entries
                    _current_llm_messages = _current_llm_messages + [assistant_msg_loop]
                    # History copy: tool_calls only, no text (avoids duplication).
                    # _loop_text is already accumulated into full_text (line above)
                    # which goes into the final assistant_msg at session cache persist
                    # time, so omitting content here is safe and intentional.
                    _history_msg_loop: dict[str, Any] = {"role": "assistant", "content": None, "tool_calls": _tc_entries}
                    _cloud_loop_history.append(_history_msg_loop)

                    yield f"data: {json.dumps({'type': 'cloud_loop_progress', 'loop': _cloud_loop, 'cloud_skills': len(cloud_tcs), 'edge_skills': len(edge_tcs)})}\n\n"

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
                        # Collect cloud skill execution for deferred persistence
                        # (written by _persist_turn_events in correct order after user_query).
                        _cloud_tool_calls_for_persist.append({
                            "id": tc_id, "function": {"name": tc_name, "arguments": json.dumps(tc_args)},
                            "_source": "cloud",
                        })
                        _cloud_tool_results_for_persist.append({
                            "tool_call_id": tc_id,
                            "name": tc_name,
                            "result": cloud_result[:_AUDIT_CHARS],
                        })
                        yield f"data: {json.dumps({'type': 'cloud_tool_result', 'name': tc_name, 'result': cloud_result[:500]})}\n\n"
                        # Truncate before quality assessment (assess full, truncate for LLM)
                        from core.context.compaction import truncate_tool_result
                        _cloud_result_raw = cloud_result  # preserve for success check before annotation
                        # Quality badge for cloud tool results
                        if _TOOL_QUALITY_ENABLED:
                            _cqa = _assess_tool_result(tc_name, cloud_result)
                            if _cqa.needs_annotation:
                                yield f"data: {json.dumps({'type': 'tool_result_quality', 'tool_name': tc_name, 'grade': _cqa.grade, 'score': _cqa.score, 'signals': _cqa.signals[:5]})}\n\n"
                                cloud_result = _annotate_tool_result({"result": cloud_result}, _cqa)["result"]
                        # Append tool result to messages for next LLM call.
                        _tool_msg: dict[str, Any] = {"role": "tool", "tool_call_id": tc_id, "content": truncate_tool_result(cloud_result)}
                        _current_llm_messages = _current_llm_messages + [_tool_msg]
                        _cloud_loop_history.append(_tool_msg)

                        # If skill returned success=False, inject a hard stop to prevent LLM from
                        # retrying with different params or using bash/curl to work around the failure.
                        _cloud_skill_failed = False
                        try:
                            _cr_parsed = json.loads(_cloud_result_raw) if isinstance(_cloud_result_raw, str) else _cloud_result_raw
                            if isinstance(_cr_parsed, dict) and _cr_parsed.get("success") is False:
                                _cloud_skill_failed = True
                                _cloud_skill_error_msg = _cr_parsed.get("result", "Operation failed.")
                                logger.warning("Cloud skill %s returned success=False, stopping cloud loop", tc_name)
                                _current_llm_messages = _current_llm_messages + [{
                                    "role": "system",
                                    "content": (
                                        "The skill returned success=False. "
                                        "STOP. Do NOT call any more tools. Do NOT retry with different parameters. "
                                        "Do NOT use bash, curl, grep, or any other tool to work around this. "
                                        "Report the error directly to the user and ask them to clarify."
                                    ),
                                }]
                        except Exception:
                            logger.exception("Failed to parse cloud_result for success check")
                        if _cloud_skill_failed:
                            break

                    # If a cloud skill failed, stop the entire cloud loop — do not call LLM again.
                    # Instead, do one final LLM call with the hard-stop message to get a text reply.
                    if _cloud_skill_failed:
                        tool_calls = []
                        # Loop will continue → LLM sees tool_result + hard-stop → returns text only
                        logger.warning("Cloud loop: continuing for final LLM call (loop=%d)", _cloud_loop)
                        continue

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
                # Inject cloud skill intermediate messages (assistant+tool_calls,
                # tool results) so future turns see the full tool usage history.
                # This is critical for multi-turn continuity: without it,
                # ConversationState.previous_skill is None and LLM can't see
                # what tools were used in prior turns.
                if _cloud_loop_history:
                    _entry.setdefault("history", []).extend(_cloud_loop_history)
                assistant_msg: dict[str, Any] = {"role": "assistant", "content": full_text}
                if tool_calls:
                    assistant_msg["tool_calls"] = tool_calls
                _entry.setdefault("history", []).append(assistant_msg)
                _entry["turn_count"] = _entry.get("turn_count", 0) + 1
                # Store chain IDs so continuation turns (tool_results) reuse them
                _entry["turn_chain_id"] = _turn_chain_id
                _entry["user_query_event_id"] = _user_query_event_id
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
                full_text=full_text,
                tool_calls=copy.deepcopy(_cloud_tool_calls_for_persist + tool_calls),
                cloud_tool_results=copy.deepcopy(_cloud_tool_results_for_persist) or None,
                context_capture_id=snapshot_id, model_used=resolved_model,
                token_usage=usage if usage else None,
                llm_params=llm_params,
                history=copy.deepcopy(current_history),
                turn_count=current_turn_count,
                agent_id=request.agent_id,
                turn_chain_id=_turn_chain_id,
                user_query_event_id=_user_query_event_id,
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
                _tool_count = len(effective_tools_schema) if effective_tools_schema else 0
                _all_count = _turn_registry.size if _turn_registry else _tool_count
                explain_event: dict[str, Any] = {
                    "type": "explain",
                    "total_ms": _total_ms,
                    "prompt_tokens": _total_prompt_tokens if _has_usage else None,
                    "completion_tokens": _total_completion_tokens if _has_usage else None,
                    "tools_selected": _tool_count,
                    "tools_available": _all_count,
                    "tool_selection": _high_confidence_skill,
                    "tool_selection_fallback": None,
                    "steps": _explain_steps,
                }
                if _memory_stats:
                    explain_event["memory"] = _memory_stats
                yield f"data: {json.dumps(explain_event)}\n\n"

            # Task 4.4: Include execution_state for edge-cloud breaker sync.
            # The cloud turn endpoint doesn't run a ChatLoop, so it validates and
            # echoes back the edge's breaker state. The edge uses this to confirm
            # the cloud accepted its state (from_wire applies validation: max_rounds
            # capped at 20, unknown status → FAILURE). Future: cloud-side breaker
            # state could be merged here if the cloud tracks its own tool failures.
            _turn_complete_data: dict = {'type': 'turn_complete', 'has_tool_calls': len(tool_calls) > 0}
            if hasattr(request, 'execution_state') and request.execution_state:
                from core.agent.turn_state import TurnState
                _ts = TurnState.from_wire(request.execution_state, messages=[], tools_schema=[])
                _turn_complete_data['execution_state'] = _ts.to_wire()
            yield f"data: {json.dumps(_turn_complete_data)}\n\n"

        except Exception as e:
            logger.error("chat_turn error: %s", e, exc_info=True)
            from sqlalchemy.exc import SQLAlchemyError

            from core.exceptions import LLMRateLimitError, LLMTimeoutError, TransientError
            from core.llm.client import BudgetExceededError
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
