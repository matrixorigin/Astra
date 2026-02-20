"""Context management for LLM agent.

Implements intelligent context selection and assembly based on:
- Relevance scoring (semantic, temporal, causal)
- Token budget allocation
- Task-aware optimization
"""

import time
from dataclasses import dataclass
from enum import Enum
from typing import Any

from core.exceptions import ContextError
from core.logging_config import get_logger
from sqlalchemy.orm import Session
from api.database import get_db_session

logger = get_logger(__name__)


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

    def to_prompt(self) -> str:
        """Convert context to LLM prompt."""
        parts = [self.system_prompt]

        if self.skill_definitions:
            parts.append("\n## Available Skills\n")
            for skill in self.skill_definitions:
                parts.append(f"- {skill['name']}: {skill['description']}")

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


class ContextManager:
    """Orchestrate context selection and assembly."""

    def __init__(
        self, db: Session, embedding_provider: str = "mock"
    ):
        """Initialize context manager.

        Args:
            db: Session connection
            embedding_provider: Embedding provider (openai, mock)
        """
        self.db = db

        # Initialize embedding service
        from core.context.embeddings import EmbeddingService

        self.embeddings = EmbeddingService(db, provider=embedding_provider)

        # Initialize prompt manager
        from core.context.prompts import PromptManager

        self.prompts = PromptManager(db)

        # Initialize relevance scorer
        from core.context.scorer import RelevanceScorer

        self.scorer = RelevanceScorer(db, self.embeddings)

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
                retrieved_events=candidates  # Store raw retrieval for replay
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
        """
        fixed_tokens = 500 + 1000 + 500  # system + skills + reserve
        available = max(0, total_tokens - fixed_tokens)

        ratios = _BUDGET_RATIOS[task_type]
        budget: dict[str, dict[str, int]] = {
            "system":  {"allocated": 500,  "used": 0},
            "skills":  {"allocated": 1000, "used": 0},
            "reserve": {"allocated": 500,  "used": 0},
        }
        for section, ratio in ratios.items():
            budget[section] = {"allocated": int(available * ratio), "used": 0}
        return budget


    def _retrieve_candidates(self, session_id: str, query: str) -> list[dict[str, Any]]:
        """Retrieve candidate events for context (fallback method)."""
        # Get recent events from current session
        from api.models import Event
        events = self.db.query(Event).filter(
            Event.session_id == session_id
        ).order_by(Event.created_at.desc()).limit(100).all()

        return [
            {
                "event_id": e.event_id,
                "event_type": e.event_type,
                "content": e.content,
                "created_at": e.created_at,  # Keep as datetime for scorer
                "parent_event_id": e.parent_event_id,
                "causal_chain_id": e.causal_chain_id,
                "metadata": e.metadata,
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
        retriever = HybridRetriever(self.db)
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
        """Retrieve relevant knowledge entries using keyword search.
        
        Note: This MVP implementation uses keyword matching.
        Vector similarity search will be implemented in future release.
        
        Args:
            user_id: User whose knowledge to search
            query: Search query
            limit: Max results
            min_confidence: Minimum confidence threshold
            
        Returns:
            List of knowledge entries with relevance scores
        """
        from api.models import KnowledgeEntry
        
        # Keyword-based retrieval (MVP)
        entries = self.db.query(KnowledgeEntry).filter(
            KnowledgeEntry.user_id == user_id,
            KnowledgeEntry.confidence >= min_confidence
        ).order_by(KnowledgeEntry.confidence.desc()).limit(limit * 2).all()  # Get more for filtering
        
        results = []
        query_lower = query.lower()
        
        for entry in entries:
            # Keyword matching with scoring
            relevance = 0.3  # Base relevance
            
            # Exact match in value
            if query_lower in entry.value.lower():
                relevance = 0.9
            # Match in key name
            elif query_lower in entry.key_name.lower():
                relevance = 0.7
            # Partial word match
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
        
        # Sort by combined score: relevance * confidence
        results.sort(key=lambda x: x["relevance"] * x["confidence"], reverse=True)
        
        logger.debug(f"Retrieved {len(results)} knowledge entries for query: {query[:50]}")
        return results[:limit]

    def _score_candidates(
        self, query: str, candidates: list[dict[str, Any]], session_id: str, task_type: TaskType
    ) -> list[tuple[dict[str, Any], float]]:
        """Score candidates by relevance using configurable scorer.

        Multi-signal scoring:
        - Semantic: L2 distance (weight varies by task)
        - Temporal: Recent events score higher
        - Causal: Events in same chain score higher
        - Keyword: Exact matches score higher
        """
        # Use the new configurable scorer
        scored_with_signals = self.scorer.score_candidates(query, candidates, session_id, task_type)

        # Convert to old format (candidate, score) for compatibility
        scored = [(candidate, score) for candidate, score, _signals in scored_with_signals]

        logger.debug(f"Scored {len(scored)} candidates using task-aware weights")
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
            return self.prompts.get_prompt(template_id)
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
        """Get available skill definitions within budget."""
        # Return empty list for now (skills_registry schema mismatch)
        return []

    def _get_code_context(
        self, selected_events: list[dict[str, Any]], token_budget: int
    ) -> list[dict[str, Any]]:
        """Extract code context from events and repos.

        Strategy:
        1. Extract file paths mentioned in events
        2. Get repo info from session
        3. Return file summaries
        """
        code_files: list[dict[str, str]] = []

        # Extract file paths from event content
        import re

        file_pattern = r"[\w/]+\.(py|js|go|java|rs|cpp|c|h)"

        for event in selected_events[:5]:  # Check recent 5 events
            content = event.get("content", "")
            matches = re.findall(file_pattern, content)

            for match in matches:
                if len(code_files) >= 5:  # Limit to 5 files
                    break

                code_files.append(
                    {
                        "file": match,
                        "mentioned_in": event["event_id"],
                        "summary": f"File mentioned in conversation: {match}",
                    }
                )

        return code_files

    def save_snapshot(
        self,
        context: Context,
        session_id: str,
        event_id: str | None = None,
        llm_request_id: str | None = None,
        llm_response_id: str | None = None,
    ) -> str:
        """Save a business-level context snapshot to database.

        This captures what the LLM saw at decision time (system prompt, selected
        events, skill definitions, code context, documentation). It is NOT a
        MatrixOne database-level snapshot — those are used for time-travel queries
        and zero-cost branching at the storage layer.

        Args:
            context: Context object
            session_id: Session identifier
            event_id: Associated event ID
            llm_request_id: LLM request identifier
            llm_response_id: LLM response identifier

        Returns:
            context_capture_id (business-level context capture ID)
        """
        import json

        from uuid_utils import uuid7

        context_capture_id = str(uuid7())

        # Extract skills used (name and version)
        skills_used = [
            {"name": s["name"], "version": s.get("version", "latest")}
            for s in context.skill_definitions
        ]

        # Ensure all data is JSON serializable by round-tripping
        def ensure_json_serializable(obj):
            """Ensure object is JSON serializable."""
            return json.loads(json.dumps(obj, default=str))

        from api.models import ContextSnapshot as SnapshotModel
        from datetime import datetime, timezone
        
        snapshot = SnapshotModel(
            context_capture_id=context_capture_id,
            session_id=session_id,
            event_id=event_id,
            system_prompt=context.system_prompt,
            skill_definitions=ensure_json_serializable(context.skill_definitions),
            selected_events=ensure_json_serializable(context.selected_events),
            retrieved_events=ensure_json_serializable(context.retrieved_events),
            code_context=ensure_json_serializable(context.code_context),
            documentation=ensure_json_serializable(context.documentation),
            total_tokens=context.total_tokens,
            token_budget=ensure_json_serializable(context.token_budget),
            assembly_time_ms=context.assembly_time_ms,
            relevance_scores=ensure_json_serializable(context.relevance_scores),
            task_type=context.task_type.value,
            skills_used=ensure_json_serializable(skills_used),
            llm_request_id=llm_request_id,
            llm_response_id=llm_response_id,
        )
        self.db.add(snapshot)
        self.db.commit()

        logger.info(f"Context snapshot saved: {context_capture_id} (retrieved: {len(context.retrieved_events or [])} events)")
        return context_capture_id

    def update_snapshot_llm_ids(
        self, context_capture_id: str, llm_request_id: str | None = None, llm_response_id: str | None = None
    ) -> None:
        """Update context capture with LLM request/response IDs.

        Args:
            context_capture_id: Context capture identifier
            llm_request_id: LLM request identifier
            llm_response_id: LLM response identifier
        """
        from api.models import ContextSnapshot as SnapshotModel
        
        update_dict = {}
        if llm_request_id:
            update_dict["llm_request_id"] = llm_request_id
        if llm_response_id:
            update_dict["llm_response_id"] = llm_response_id

        if not update_dict:
            return

        self.db.query(SnapshotModel).filter(
            SnapshotModel.context_capture_id == context_capture_id
        ).update(update_dict)
        self.db.commit()

    def load_snapshot(self, context_capture_id: str) -> Context:
        """Load context capture from database."""
        from api.models import ContextSnapshot as SnapshotModel

        row = self.db.query(SnapshotModel).filter(
            SnapshotModel.context_capture_id == context_capture_id
        ).first()

        if not row:
            raise ContextError(f"Context capture not found: {context_capture_id}")

        # JSON fields are already parsed by SQLAlchemy
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
