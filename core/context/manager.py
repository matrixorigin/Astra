"""Context management for LLM agent.

Implements intelligent context selection and assembly based on:
- Relevance scoring (semantic, temporal, causal)
- Token budget allocation
- Task-aware optimization
"""

import json
import logging as _logging
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from enum import Enum
from typing import Any

from core.db_consumer import DbConsumer, DbFactory
from core.exceptions import ContextError
from core.logging_config import get_logger
from skills.knowledge.api import update_access_tracking as _update_access_tracking
from sqlalchemy import text

logger = get_logger(__name__)

# Shared thread pool for async DB writes (snapshot, update_llm_ids)
_write_pool = ThreadPoolExecutor(max_workers=2, thread_name_prefix="ctx_snapshot")


class TaskType(str, Enum):
    """Task types for context optimization."""

    CODE_REVIEW = "code_review"
    PLANNING = "planning"
    DEBUGGING = "debugging"
    GENERAL = "general"


# Budget ratios per task type — aligned with design doc §2
# Each maps section → fraction of available tokens (after fixed allocations)
_BUDGET_RATIOS: dict[TaskType, dict[str, float]] = {
    TaskType.CODE_REVIEW: {"code": 0.50, "history": 0.20, "docs": 0.20, "logs": 0.10},
    TaskType.DEBUGGING:   {"logs": 0.40, "code": 0.30, "history": 0.20, "docs": 0.10},
    TaskType.PLANNING:    {"history": 0.50, "code": 0.20, "docs": 0.20, "logs": 0.10},
    TaskType.GENERAL:     {"history": 0.40, "code": 0.30, "docs": 0.20, "logs": 0.10},
}

# Keywords for auto-classification
_TASK_KEYWORDS: dict[TaskType, list[str]] = {
    TaskType.CODE_REVIEW: ["review", "code review", "PR", "pull request", "refactor", "clean up"],
    TaskType.DEBUGGING:   ["debug", "error", "bug", "fix", "traceback", "exception", "crash", "fail"],
    TaskType.PLANNING:    ["plan", "design", "architect", "roadmap", "strategy", "proposal"],
}


@dataclass
class ContextFragment:
    """A piece of context with metadata."""

    content: str
    tokens: int
    source: str  # 'event' | 'code' | 'doc' | 'skill'
    relevance_score: float
    metadata: dict[str, Any]


@dataclass
class Context:
    """Assembled context ready for LLM."""

    system_prompt: str
    skill_definitions: list[dict[str, Any]]
    selected_events: list[dict[str, Any]]
    code_context: list[dict[str, Any]]
    documentation: list[dict[str, Any]]

    total_tokens: int
    token_budget: dict[str, dict[str, int]]
    assembly_time_ms: int
    relevance_scores: dict[str, float]
    task_type: TaskType
    retrieved_events: list[dict[str, Any]] | None = None  # Raw retrieval for replay
    topic_shift_score: float = 0.0  # 0=same topic, 1=new topic; used by ChatLoop for STALE_CONTEXT feedback

    def to_prompt(self) -> str:
        """Convert context to LLM prompt."""
        parts = [self.system_prompt]

        if self.skill_definitions:
            parts.append("\n## Available Skills\n")
            for skill in self.skill_definitions:
                parts.append(f"- {skill['skill_name']}: {skill['description']}")

        if self.selected_events:
            parts.append("\n## Conversation History\n")
            for event in self.selected_events:
                parts.append(f"[{event['event_type']}] {event['content']}")

        if self.code_context:
            parts.append("\n## Code Context\n")
            for code in self.code_context:
                parts.append(f"File: {code['file']}\n```\n{code['content']}\n```")

        if self.documentation:
            parts.append("\n## Documentation\n")
            for doc in self.documentation:
                parts.append(doc["content"])

        return "\n".join(parts)


class ContextManager(DbConsumer):
    """Orchestrate context selection and assembly."""

    def __init__(
        self, db_factory: DbFactory, embedding_provider: str = "mock", gate_trigger=None,
    ):
        """Initialize context manager.

        Args:
            db_factory: Callable returning a new SQLAlchemy Session
            embedding_provider: Embedding provider (openai, mock)
            gate_trigger: GateTrigger for auto-firing regression gate on prompt changes
        """
        super().__init__(db_factory)

        # Initialize embedding service
        from core.context.embeddings import EmbeddingService

        self.embeddings = EmbeddingService(db_factory, provider=embedding_provider)

        # Initialize prompt manager
        from core.context.prompts import PromptManager

        self.prompts = PromptManager(db_factory, gate_trigger=gate_trigger)

        # Initialize relevance scorer
        from core.context.scorer import RelevanceScorer

        self.scorer = RelevanceScorer(db_factory, self.embeddings)

        logger.info(f"ContextManager initialized (embeddings={embedding_provider})")

    @staticmethod
    def classify_task(query: str) -> TaskType:
        """Auto-classify task type from query text using keyword matching."""
        q = query.lower()
        for task_type, keywords in _TASK_KEYWORDS.items():
            if any(kw in q for kw in keywords):
                return task_type
        return TaskType.GENERAL

    def build_context(
        self,
        session_id: str,
        query: str,
        max_tokens: int = 8000,
        task_type: TaskType | None = None,
        current_chain_id: str | None = None,
        use_hybrid_retrieval: bool = True,  # Design default: hybrid retrieval as primary path
        forced_retrieval: list[dict[str, Any]] | None = None,
    ) -> Context:
        """Build optimal context for current query.

        Args:
            session_id: Current session
            query: User query
            max_tokens: Maximum tokens allowed
            task_type: Type of task for optimization
            current_chain_id: Current causal chain for proximity scoring
            use_hybrid_retrieval: Use MatrixOne hybrid search (default True)
            forced_retrieval: Use these events instead of retrieving (for replay)

        Returns:
            Assembled context with retrieval metadata
        """
        start_time = time.time()

        try:
            # 0. Auto-classify if not specified
            if task_type is None:
                task_type = self.classify_task(query)

            # 1. Allocate token budget
            budget = self._allocate_budget(max_tokens, task_type)
            logger.debug(f"Token budget allocated: {budget}")

            # 2. Retrieve candidates (or use forced results for replay)
            if forced_retrieval is not None:
                candidates = forced_retrieval
                logger.info(f"Using forced retrieval: {len(candidates)} events (replay mode)")
            elif use_hybrid_retrieval:
                candidates = self._retrieve_hybrid(session_id, query, current_chain_id)
                logger.debug(f"Hybrid retrieval: {len(candidates)} candidate events")
            else:
                candidates = self._retrieve_candidates(session_id, query)
                logger.debug(f"Fallback retrieval: {len(candidates)} candidate events")

            # 3. Score and rank (skip if using forced retrieval with scores)
            if forced_retrieval is not None:
                # In replay mode: use forced retrieval results with their scores
                if all("relevance_score" in c for c in candidates):
                    scored = [(c, c["relevance_score"]) for c in candidates]
                else:
                    # Fallback: score them
                    scored = self._score_candidates(query, candidates, session_id, task_type)
            else:
                scored = self._score_candidates(query, candidates, session_id, task_type)
            logger.debug(f"Scored {len(scored)} candidates")

            # 4. Select within budget
            selected = self._select_within_budget(scored, budget)

            # 5. Assemble context
            context = self._assemble_context(
                selected, budget, task_type, 
                assembly_time_ms=int((time.time() - start_time) * 1000),
                retrieved_events=candidates,  # Store raw retrieval for replay
                topic_shift_score=getattr(self, "_last_topic_shift", 0.0),
            )

            logger.info(
                f"Context built: {context.total_tokens} tokens, "
                f"{len(context.selected_events)} events, "
                f"{context.assembly_time_ms}ms"
            )

            return context

        except Exception as e:
            logger.error(f"Failed to build context: {e}")
            raise ContextError(f"Context assembly failed: {e}") from e

    def _allocate_budget(self, total_tokens: int, task_type: TaskType) -> dict[str, dict[str, int]]:
        """Allocate token budget based on task type.

        Returns dict of section → {allocated: int, used: int} per design §2.
        Fixed allocations: system 500, skills 1000, reserve 500.
        Loads overrides from infra_configs table if available (set by ContextBudgetTuner).
        """
        fixed_tokens = 500 + 1000 + 500  # system + skills + reserve
        available = max(0, total_tokens - fixed_tokens)

        ratios = self._load_budget_ratios(task_type)
        budget: dict[str, dict[str, int]] = {
            "system":  {"allocated": 500,  "used": 0},
            "skills":  {"allocated": 1000, "used": 0},
            "reserve": {"allocated": 500,  "used": 0},
        }
        for section, ratio in ratios.items():
            budget[section] = {"allocated": int(available * ratio), "used": 0}
        return budget

    _budget_cache: dict[str, Any] | None = None
    _budget_cache_ts: float = 0.0
    _BUDGET_CACHE_TTL: float = 60.0  # seconds

    def _load_budget_ratios(self, task_type: TaskType) -> dict[str, float]:
        """Load budget ratios, preferring DB overrides over hardcoded defaults."""
        now = time.monotonic()
        if self._budget_cache is not None and (now - self._budget_cache_ts) < self._BUDGET_CACHE_TTL:
            if task_type.value in self._budget_cache:
                return self._budget_cache[task_type.value]
            return _BUDGET_RATIOS[task_type]

        try:
            import json
            with self._db() as db:
                from api.models import Config
                row = db.query(Config.value).filter(Config.key_name == "context_budget_ratios").first()
            if row:
                overrides = json.loads(row[0]) if isinstance(row[0], str) else row[0]
                self._budget_cache = overrides
                self._budget_cache_ts = now
                if task_type.value in overrides:
                    return overrides[task_type.value]
            else:
                self._budget_cache = {}
                self._budget_cache_ts = now
        except Exception as e:
            logger.debug("Failed to load budget overrides, using defaults: %s", e)
        return _BUDGET_RATIOS[task_type]


    def _retrieve_candidates(self, session_id: str, query: str) -> list[dict[str, Any]]:
        """Retrieve candidate events for context (fallback method)."""
        from api.models import Event
        with self._db() as db:
            # Projection: only load fields needed for scoring (skip embedding, snapshots, etc.)
            events = db.query(
                Event.event_id,
                Event.event_type,
                Event.content,
                Event.created_at,
                Event.parent_event_id,
                Event.causal_chain_id,
                Event.event_metadata,
            ).filter(
                Event.session_id == session_id
            ).order_by(Event.created_at.desc()).limit(100).all()

        # Safe after session close: Event has only Column() attrs, no lazy relationships.
        return [
            {
                "event_id": e.event_id,
                "event_type": e.event_type,
                "content": e.content,
                "created_at": e.created_at,  # Keep as datetime for scorer
                "parent_event_id": e.parent_event_id,
                "causal_chain_id": e.causal_chain_id,
                "metadata": e.event_metadata,
            }
            for e in events
        ]

    def _retrieve_hybrid(
        self, 
        session_id: str, 
        query: str, 
        current_chain_id: str | None = None
    ) -> list[dict[str, Any]]:
        """Retrieve events using MatrixOne hybrid search.
        
        Combines vector similarity, keyword matching, temporal decay, and causal proximity.
        """
        from core.context.hybrid_retrieval import HybridRetriever
        
        # Generate query embedding
        query_embedding = self.embeddings.embed_text(query)
        
        # Use hybrid retriever
        retriever = HybridRetriever(self._db_factory)
        events = retriever.retrieve_events(
            query_text=query,
            query_embedding=query_embedding,
            session_id=session_id,
            current_chain_id=current_chain_id,
            limit=50,  # Get more candidates for scoring
        )
        
        return events

    def retrieve_semantic_knowledge(
        self, user_id: str, query: str, limit: int = 5, min_confidence: float = 0.3
    ) -> list[dict[str, Any]]:
        """Retrieve relevant knowledge entries using hybrid (vector + keyword) search.

        Delegates to HybridRetriever.retrieve_knowledge for semantic scoring.
        Falls back to keyword matching if embedding or hybrid retrieval fails.

        Args:
            user_id: User whose knowledge to search
            query: Search query
            limit: Max results
            min_confidence: Minimum confidence threshold

        Returns:
            List of knowledge entries with relevance scores
        """
        # Primary path: hybrid retrieval (vector + keyword + confidence)
        try:
            from core.context.hybrid_retrieval import HybridRetriever
            query_embedding = self.embeddings.embed_text(query)
            retriever = HybridRetriever(self._db_factory)
            results = retriever.retrieve_knowledge(
                query_text=query,
                query_embedding=query_embedding,
                user_id=user_id,
                limit=limit,
                confidence_threshold=min_confidence,
            )
            if results:
                logger.debug("Hybrid knowledge retrieval: %d entries for: %s", len(results), query[:50])
                return results
        except Exception as e:
            logger.warning("Hybrid knowledge retrieval failed, falling back to keyword: %s", e)

        # Fallback: keyword matching
        return self._keyword_knowledge_fallback(user_id, query, limit, min_confidence)

    def _keyword_knowledge_fallback(
        self, user_id: str, query: str, limit: int, min_confidence: float
    ) -> list[dict[str, Any]]:
        """Keyword-based knowledge retrieval fallback."""
        from api.models import KnowledgeEntry

        try:
            with self._db() as db:
                entries = db.query(KnowledgeEntry).filter(
                    KnowledgeEntry.user_id == user_id,
                    KnowledgeEntry.confidence >= min_confidence,
                ).order_by(KnowledgeEntry.confidence.desc()).limit(limit * 2).all()
        except Exception as e:
            logger.warning("Keyword knowledge fallback failed: %s", e)
            return []

        results = []
        query_lower = query.lower()

        for entry in entries:
            relevance = 0.3
            if query_lower in entry.value.lower():
                relevance = 0.9
            elif query_lower in entry.key_name.lower():
                relevance = 0.7
            elif any(word in entry.value.lower() for word in query_lower.split() if len(word) > 3):
                relevance = 0.5

            results.append({
                "entry_id": entry.entry_id,
                "category": entry.category,
                "key_name": entry.key_name,
                "value": entry.value,
                "confidence": entry.confidence,
                "trust_tier": entry.trust_tier,
                "relevance": relevance,
                "created_at": entry.created_at,
            })

        results.sort(key=lambda x: x["relevance"] * x["confidence"], reverse=True)
        top = results[:limit]

        if top:
            _update_access_tracking(self._db, [r["entry_id"] for r in top])

        logger.debug("Keyword knowledge fallback: %d entries for: %s", len(top), query[:50])
        return top

    def _score_candidates(
        self, query: str, candidates: list[dict[str, Any]], session_id: str, task_type: TaskType
    ) -> list[tuple[dict[str, Any], float]]:
        """Score candidates by relevance using configurable scorer.

        Multi-signal scoring:
        - Semantic: L2 distance (weight varies by task)
        - Temporal: Recent events score higher
        - Causal: Events in same chain score higher
        - Keyword: Exact matches score higher

        When a topic shift is detected, temporal and causal weights are
        suppressed so that stale context from the old topic does not
        dominate the selection.
        """
        # Detect topic shift from recent events.
        # Uses stored embeddings when available (1 embed call for query only).
        # Sort by created_at descending to ensure we compare against the most
        # recent events regardless of the order forced_retrieval provides.
        recent_for_shift = sorted(
            [c for c in candidates if c.get("created_at")],
            key=lambda c: c["created_at"],
            reverse=True,
        )[:3]
        topic_shift = self.scorer.detect_topic_shift(query, recent_for_shift)

        # Use the new configurable scorer with topic shift awareness
        scored_with_signals = self.scorer.score_candidates(
            query, candidates, session_id, task_type, topic_shift=topic_shift,
        )

        # Convert to old format (candidate, score) for compatibility
        scored = [(candidate, score) for candidate, score, _signals in scored_with_signals]

        logger.debug(
            "Scored %d candidates (task=%s, topic_shift=%.2f)",
            len(scored), task_type.value, topic_shift,
        )
        # Store for build_context to propagate to Context dataclass
        self._last_topic_shift = topic_shift
        return scored

    def _select_within_budget(
        self, scored: list[tuple[dict[str, Any], float]], budget: dict[str, dict[str, int]]
    ) -> list[dict[str, Any]]:
        """Select top events within token budget."""
        selected = []
        tokens_used = 0
        history_limit = budget.get("history", {}).get("allocated", 0)

        for event, score in scored:
            event_tokens = len(event["content"]) // 4
            if tokens_used + event_tokens <= history_limit:
                selected.append({"event": event, "score": score, "tokens": event_tokens})
                tokens_used += event_tokens
            else:
                break

        budget.get("history", {})["used"] = tokens_used
        logger.debug(f"Selected {len(selected)} events using {tokens_used} tokens")
        return selected

    def _assemble_context(
        self,
        selected: list[dict[str, Any]],
        budget: dict[str, int],
        task_type: TaskType,
        assembly_time_ms: int,
        retrieved_events: list[dict[str, Any]] | None = None,
        topic_shift_score: float = 0.0,
    ) -> Context:
        """Assemble final context."""
        system_prompt = self._get_system_prompt(task_type)

        # Load skill definitions from registry
        skill_definitions = self._get_skill_definitions(budget["skills"]["allocated"])

        selected_events = [
            {
                "event_id": s["event"]["event_id"],
                "event_type": s["event"]["event_type"],
                "content": s["event"]["content"],
                "score": float(s["score"]),  # Ensure JSON serializable
            }
            for s in selected
        ]

        # Load code context for code-related tasks
        code_context = []
        if task_type in [TaskType.CODE_REVIEW, TaskType.DEBUGGING]:
            code_context = self._get_code_context(selected_events, budget.get("code", {}).get("allocated", 0))

        total_tokens = sum(s["tokens"] for s in selected) + budget["system"]["allocated"]

        relevance_scores = {s["event"]["event_id"]: s["score"] for s in selected}

        return Context(
            system_prompt=system_prompt,
            skill_definitions=skill_definitions,
            selected_events=selected_events,
            code_context=code_context,
            documentation=[],
            total_tokens=total_tokens,
            token_budget=budget,
            assembly_time_ms=assembly_time_ms,
            relevance_scores=relevance_scores,
            task_type=task_type,
            retrieved_events=retrieved_events,
            topic_shift_score=topic_shift_score,
        )

    def _get_system_prompt(self, task_type: TaskType) -> str:
        """Get system prompt based on task type from database.

        Falls back to hardcoded prompts if database lookup fails.
        """
        template_map = {
            TaskType.CODE_REVIEW: "system_code_review",
            TaskType.PLANNING: "system_planning",
            TaskType.DEBUGGING: "system_debugging",
            TaskType.GENERAL: "system_general",
        }

        template_id = template_map.get(task_type, "system_general")

        # Try to get from database first
        try:
            return self.prompts.get_system_prompt(template_id)
        except Exception as e:
            logger.warning(f"Failed to load prompt from DB: {e}, using fallback")
            # Fallback to hardcoded
            fallbacks = {
                TaskType.CODE_REVIEW: "You are an expert code reviewer. Focus on code quality, security, and best practices.",
                TaskType.PLANNING: "You are a technical architect. Help plan and design solutions.",
                TaskType.DEBUGGING: "You are a debugging expert. Help identify and fix issues.",
                TaskType.GENERAL: "You are an intelligent development agent.",
            }
            return fallbacks.get(task_type, fallbacks[TaskType.GENERAL])

    def _get_skill_definitions(self, token_budget: int) -> list[dict[str, Any]]:
        """Get active skill definitions from DB within token budget."""
        from api.models import SkillRegistry as SkillModel

        try:
            with self._db() as db:
                skills = db.query(SkillModel).filter(
                    SkillModel.is_active == 1
                ).all()
        except Exception as e:
            logger.warning("Failed to load skill definitions: %s", e)
            return []

        result = []
        tokens_used = 0
        for s in skills:
            defn: dict[str, Any] = {
                "skill_name": s.skill_name,
                "description": s.description or "",
                "version": s.version,
            }
            if s.skill_definition:
                defn["definition"] = s.skill_definition
            if s.triggers:
                defn["triggers"] = s.triggers
            entry_tokens = len(str(defn)) // 4
            if tokens_used + entry_tokens > token_budget:
                break
            result.append(defn)
            tokens_used += entry_tokens

        return result

    def _get_code_context(
        self, selected_events: list[dict[str, Any]], token_budget: int
    ) -> list[dict[str, Any]]:
        """Extract code context from events.

        Extracts file paths mentioned in event content using regex,
        then returns de-duplicated file references.
        """
        import re

        # Non-capturing group so findall returns full match, not just extension.
        # Supports hyphens, dots, and @ in directory/file names.
        file_pattern = r"[\w.@/-]+\.(?:py|tsx|ts|jsx|js|go|java|rs|cpp|c|h|rb|sh|yaml|yml|json|toml)"

        seen: set[str] = set()
        code_files: list[dict[str, str]] = []

        for event in selected_events[:5]:
            content = event.get("content", "")
            for match in re.finditer(file_pattern, content):
                path = match.group()
                if path in seen or len(code_files) >= 5:
                    continue
                seen.add(path)
                code_files.append({
                    "file": path,
                    "mentioned_in": event["event_id"],
                    "summary": f"File mentioned in conversation: {path}",
                })

        return code_files

    def save_snapshot(
        self,
        context: Context,
        session_id: str,
        event_id: str | None = None,
        llm_request_id: str | None = None,
        llm_response_id: str | None = None,
    ) -> str:
        """Save a business-level context snapshot to database (async).

        Returns context_capture_id immediately. The actual DB write happens
        in a background thread with its own session.
        """
        from uuid_utils import uuid7

        context_capture_id = str(uuid7())

        # Prepare all data synchronously (must happen on caller's thread
        # because Context object may not be safe to share)
        skills_used = [
            {"skill_name": s.get("skill_name") or s.get("name", ""), "version": s.get("version", "latest")}
            for s in context.skill_definitions
        ]

        def _json(obj):
            return json.loads(json.dumps(obj, default=str))

        payload = {
            "context_capture_id": context_capture_id,
            "session_id": session_id,
            "event_id": event_id,
            "system_prompt": context.system_prompt,
            "skill_definitions": _json(context.skill_definitions),
            "selected_events": _json(context.selected_events),
            "retrieved_events": _json(context.retrieved_events),
            "code_context": _json(context.code_context),
            "documentation": _json(context.documentation),
            "total_tokens": context.total_tokens,
            "token_budget": _json(context.token_budget),
            "assembly_time_ms": context.assembly_time_ms,
            "relevance_scores": _json(context.relevance_scores),
            "task_type": context.task_type.value,
            "skills_used": _json(skills_used),
            "llm_request_id": llm_request_id,
            "llm_response_id": llm_response_id,
        }

        _write_pool.submit(self._write_snapshot, self._db_factory, payload)

        logger.info(f"Context snapshot queued: {context_capture_id}")
        return context_capture_id

    @staticmethod
    def _write_snapshot(db_factory, payload: dict) -> None:
        """Write snapshot in background thread with its own DB session."""
        db = db_factory()
        try:
            from api.models import ContextSnapshot as SnapshotModel
            snapshot = SnapshotModel(**payload)
            db.add(snapshot)
            db.commit()
        except Exception:
            db.rollback()
            _logging.getLogger(__name__).exception(
                "Failed to write snapshot %s", payload.get("context_capture_id")
            )
        finally:
            db.close()

    @staticmethod
    def _update_snapshot(db_factory, context_capture_id: str, update_dict: dict) -> None:
        """Update snapshot in background thread.

        Retries briefly if the row doesn't exist yet — the INSERT from
        save_snapshot may still be in-flight on another thread-pool worker.

        time.sleep() is acceptable here: this runs in a background thread
        (not the async event loop), and the total worst-case delay (750 ms)
        is bounded.  If the INSERT truly failed, the warning log after
        retries is the correct outcome — the snapshot row simply won't
        have LLM IDs, which is non-critical metadata.
        """
        import time
        db = db_factory()
        try:
            from api.models import ContextSnapshot as SnapshotModel
            for attempt in range(5):
                rows_updated = db.query(SnapshotModel).filter(
                    SnapshotModel.context_capture_id == context_capture_id
                ).update(update_dict)
                db.commit()
                if rows_updated > 0:
                    return
                # Row not yet visible — INSERT may still be in-flight
                delay = 0.05 * (attempt + 1)
                _logging.getLogger(__name__).debug(
                    "Snapshot %s not found (attempt %d/5), retrying in %.0fms",
                    context_capture_id, attempt + 1, delay * 1000,
                )
                time.sleep(delay)
            _logging.getLogger(__name__).warning(
                "Snapshot %s not found after 5 retries — INSERT may have failed; "
                "LLM IDs will not be recorded for this snapshot",
                context_capture_id,
            )
        except Exception:
            db.rollback()
            _logging.getLogger(__name__).warning(
                "Failed to update snapshot %s", context_capture_id,
                exc_info=True,
            )
        finally:
            db.close()

    @staticmethod
    def flush_writes() -> None:
        """Block until all pending background snapshot writes complete.

        Useful in tests and before load_snapshot when immediate consistency is needed.
        """
        global _write_pool
        _write_pool.shutdown(wait=True)
        _write_pool = ThreadPoolExecutor(max_workers=2, thread_name_prefix="ctx_snapshot")

    @staticmethod
    def update_snapshot_llm_ids(
        db_factory: DbFactory, context_capture_id: str,
        llm_request_id: str | None = None, llm_response_id: str | None = None,
    ) -> None:
        """Update context capture with LLM request/response IDs (async).

        Accepts db_factory directly — no need to instantiate the full
        ContextManager (which creates EmbeddingService, PromptManager, etc.)
        just for a metadata update.

        Offloaded to background thread — fire-and-forget.
        """
        update_dict = {}
        if llm_request_id:
            update_dict["llm_request_id"] = llm_request_id
        if llm_response_id:
            update_dict["llm_response_id"] = llm_response_id
        if not update_dict:
            return

        _write_pool.submit(
            ContextManager._update_snapshot, db_factory, context_capture_id, update_dict
        )

    def load_snapshot(self, context_capture_id: str) -> Context:
        """Load context capture from database.

        Note: if save_snapshot was called recently, call flush_writes() first
        to ensure the background write has completed.
        """
        from api.models import ContextSnapshot as SnapshotModel

        with self._db() as db:
            row = db.query(SnapshotModel).filter(
                SnapshotModel.context_capture_id == context_capture_id
            ).first()

        if not row:
            raise ContextError(f"Context capture not found: {context_capture_id}")

        # Safe after session close: ContextSnapshot has only Column() attrs, no lazy relationships.
        skill_definitions = row.skill_definitions or []
        selected_events = row.selected_events or []
        retrieved_events = row.retrieved_events or []
        code_context = row.code_context or []
        documentation = row.documentation or []

        return Context(
            system_prompt=row.system_prompt,
            skill_definitions=skill_definitions,
            selected_events=selected_events,
            code_context=code_context,
            documentation=documentation,
            total_tokens=row.total_tokens,
            token_budget=row.token_budget or {},
            assembly_time_ms=row.assembly_time_ms or 0,
            relevance_scores=row.relevance_scores or {},
            task_type=TaskType(row.task_type) if row.task_type else TaskType.GENERAL,
            retrieved_events=retrieved_events,
        )
