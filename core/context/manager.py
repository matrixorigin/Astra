"""Context management for LLM agent.

Implements intelligent context selection and assembly based on:
- Relevance scoring (semantic, temporal, causal)
- Token budget allocation
- Task-aware optimization
"""

import hashlib
import os
import time
from dataclasses import dataclass
from enum import Enum
from typing import Any

from core.exceptions import ContextError
from core.logging_config import get_logger
from core.cache import get_cache
from sdk import Database

logger = get_logger(__name__)


class TaskType(str, Enum):
    """Task types for context optimization."""

    CODE_REVIEW = "code_review"
    PLANNING = "planning"
    DEBUGGING = "debugging"
    GENERAL = "general"


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
    token_budget: dict[str, int]
    assembly_time_ms: int
    relevance_scores: dict[str, float]
    task_type: TaskType

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
        self, db: Database, embedding_provider: str = "mock"
    ):
        """Initialize context manager.

        Args:
            db: Database connection
            embedding_provider: Embedding provider (openai, mock)
        """
        self.db = db
        self.cache = get_cache()
        self.cache_ttl = int(os.getenv("CONTEXT_CACHE_TTL", "300"))
        self.context_version = os.getenv("CONTEXT_VERSION", "v1")

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

    def build_context(
        self,
        session_id: str,
        query: str,
        max_tokens: int = 8000,
        task_type: TaskType = TaskType.GENERAL,
        refresh: bool = False,
    ) -> Context:
        """Build optimal context for current query.

        Args:
            session_id: Current session
            query: User query
            max_tokens: Maximum tokens allowed
            task_type: Type of task for optimization

        Returns:
            Assembled context
        """
        start_time = time.time()

        try:
            last_event_id = self._get_last_event_id(session_id)
            cache_key = self._get_cache_key(
                session_id=session_id,
                query=query,
                task_type=task_type,
                max_tokens=max_tokens,
                last_event_id=last_event_id,
            )
            if not refresh:
                cached = self._get_cached_context(cache_key)
                if cached is not None:
                    logger.debug(f"Context cache hit: {cache_key}")
                    return cached

            # 1. Allocate token budget
            budget = self._allocate_budget(max_tokens, task_type)
            logger.debug(f"Token budget allocated: {budget}")

            # 2. Retrieve candidates
            candidates = self._retrieve_candidates(session_id, query)
            logger.debug(f"Retrieved {len(candidates)} candidate events")

            # 3. Score and rank
            scored = self._score_candidates(query, candidates, session_id, task_type)
            logger.debug(f"Scored {len(scored)} candidates")

            # 4. Select within budget
            selected = self._select_within_budget(scored, budget)

            # 5. Assemble context
            context = self._assemble_context(
                selected, budget, task_type, assembly_time_ms=int((time.time() - start_time) * 1000)
            )

            logger.info(
                f"Context built: {context.total_tokens} tokens, "
                f"{len(context.selected_events)} events, "
                f"{context.assembly_time_ms}ms"
            )

            self._set_cached_context(cache_key, context)
            return context

        except Exception as e:
            logger.error(f"Failed to build context: {e}")
            raise ContextError(f"Context assembly failed: {e}") from e

    def _allocate_budget(self, total_tokens: int, task_type: TaskType) -> dict[str, int]:
        """Allocate token budget based on task type."""
        allocations = {
            TaskType.CODE_REVIEW: {
                "system": 500,
                "skills": 1000,
                "history": int(total_tokens * 0.2),
                "code": int(total_tokens * 0.6),
                "docs": int(total_tokens * 0.1),
                "reserve": 500,
            },
            TaskType.PLANNING: {
                "system": 500,
                "skills": 1000,
                "history": int(total_tokens * 0.6),
                "code": int(total_tokens * 0.2),
                "docs": int(total_tokens * 0.1),
                "reserve": 500,
            },
            TaskType.DEBUGGING: {
                "system": 500,
                "skills": 1000,
                "history": int(total_tokens * 0.2),
                "code": int(total_tokens * 0.4),
                "docs": int(total_tokens * 0.1),
                "reserve": 500,
            },
            TaskType.GENERAL: {
                "system": 500,
                "skills": 1000,
                "history": int(total_tokens * 0.5),
                "code": int(total_tokens * 0.2),
                "docs": int(total_tokens * 0.1),
                "reserve": 500,
            },
        }

        return allocations[task_type]

    def _get_last_event_id(self, session_id: str) -> str | None:
        row = self.db.fetchone(
            """
            SELECT event_id
            FROM conversation_events
            WHERE session_id = %s
            ORDER BY created_at DESC
            LIMIT 1
            """,
            (session_id,),
        )
        return row["event_id"] if row else None

    def _hash_query(self, query: str) -> str:
        return hashlib.sha256(query.encode("utf-8")).hexdigest()

    def _get_cache_key(
        self,
        session_id: str,
        query: str,
        task_type: TaskType,
        max_tokens: int,
        last_event_id: str | None,
    ) -> str:
        query_hash = self._hash_query(query)
        event_part = last_event_id or "none"
        return (
            f"context:{self.context_version}:{session_id}:{task_type.value}:{max_tokens}:"
            f"{event_part}:{query_hash}"
        )

    def _get_cached_context(self, cache_key: str) -> Context | None:
        cached = self.cache.get(cache_key)
        if not cached:
            return None
        try:
            return Context(
                system_prompt=cached["system_prompt"],
                skill_definitions=cached["skill_definitions"],
                selected_events=cached["selected_events"],
                code_context=cached["code_context"],
                documentation=cached["documentation"],
                total_tokens=cached["total_tokens"],
                token_budget=cached["token_budget"],
                assembly_time_ms=cached["assembly_time_ms"],
                relevance_scores=cached["relevance_scores"],
                task_type=TaskType(cached["task_type"]),
            )
        except Exception:
            return None

    def _set_cached_context(self, cache_key: str, context: Context) -> None:
        payload = {
            "system_prompt": context.system_prompt,
            "skill_definitions": context.skill_definitions,
            "selected_events": context.selected_events,
            "code_context": context.code_context,
            "documentation": context.documentation,
            "total_tokens": context.total_tokens,
            "token_budget": context.token_budget,
            "assembly_time_ms": context.assembly_time_ms,
            "relevance_scores": context.relevance_scores,
            "task_type": context.task_type.value,
        }
        self.cache.set(cache_key, payload, ttl=self.cache_ttl)

    def invalidate_session_cache(self, session_id: str) -> None:
        pattern = f"context:{self.context_version}:{session_id}:*"
        self.cache.clear_pattern(pattern)

    def _retrieve_candidates(self, session_id: str, query: str) -> list[dict[str, Any]]:
        """Retrieve candidate events for context."""
        # Get recent events from current session
        events = self.db.fetchall(
            """
            SELECT event_id, event_type, content, created_at,
                   parent_event_id, causal_chain_id, metadata
            FROM conversation_events
            WHERE session_id = %s
            ORDER BY created_at DESC
            LIMIT 100
            """,
            (session_id,),
        )

        return [dict(e) for e in events]

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
        self, scored: list[tuple[dict[str, Any], float]], budget: dict[str, int]
    ) -> list[dict[str, Any]]:
        """Select top events within token budget."""
        selected = []
        tokens_used = 0
        history_budget = budget["history"]

        for event, score in scored:
            # Estimate tokens (rough: 1 token ≈ 4 chars)
            event_tokens = len(event["content"]) // 4

            if tokens_used + event_tokens <= history_budget:
                selected.append({"event": event, "score": score, "tokens": event_tokens})
                tokens_used += event_tokens
            else:
                break

        logger.debug(f"Selected {len(selected)} events using {tokens_used} tokens")
        return selected

    def _assemble_context(
        self,
        selected: list[dict[str, Any]],
        budget: dict[str, int],
        task_type: TaskType,
        assembly_time_ms: int,
    ) -> Context:
        """Assemble final context."""
        system_prompt = self._get_system_prompt(task_type)

        # Load skill definitions from registry
        skill_definitions = self._get_skill_definitions(budget["skills"])

        selected_events = [
            {
                "event_id": s["event"]["event_id"],
                "event_type": s["event"]["event_type"],
                "content": s["event"]["content"],
                "score": s["score"],
            }
            for s in selected
        ]

        # Load code context for code-related tasks
        code_context = []
        if task_type in [TaskType.CODE_REVIEW, TaskType.DEBUGGING]:
            code_context = self._get_code_context(selected_events, budget["code"])

        total_tokens = sum(s["tokens"] for s in selected) + budget["system"]

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
        skills = self.db.fetchall("""
            SELECT skill_name, version, description, category, subcategory
            FROM skills_registry
            WHERE is_active = 1
            ORDER BY priority DESC
            LIMIT 10
        """)

        return [
            {
                "name": s["skill_name"],
                "version": s["version"],
                "description": s["description"],
                "category": s.get("category", "general"),
            }
            for s in skills
        ]

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
        self, context: Context, session_id: str, event_id: str | None = None
    ) -> str:
        """Save context snapshot to database (always enabled).

        Returns:
            snapshot_id
        """
        import json

        from uuid_utils import uuid7

        snapshot_id = str(uuid7())

        self.db.execute(
            """
            INSERT INTO context_snapshots (
                snapshot_id, session_id, event_id,
                system_prompt, skill_definitions, selected_events,
                code_context, documentation,
                total_tokens, token_budget, assembly_time_ms,
                relevance_scores, task_type
            ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
            """,
            (
                snapshot_id,
                session_id,
                event_id,
                context.system_prompt,
                json.dumps(context.skill_definitions),
                json.dumps(context.selected_events),
                json.dumps(context.code_context),
                json.dumps(context.documentation),
                context.total_tokens,
                json.dumps(context.token_budget),
                context.assembly_time_ms,
                json.dumps(context.relevance_scores),
                context.task_type.value,
            ),
        )

        logger.info(f"Context snapshot saved: {snapshot_id}")
        return snapshot_id

    def load_snapshot(self, snapshot_id: str) -> Context:
        """Load context snapshot from database."""
        import json

        row = self.db.fetchone(
            "SELECT * FROM context_snapshots WHERE snapshot_id = %s", (snapshot_id,)
        )

        if not row:
            raise ContextError(f"Snapshot not found: {snapshot_id}")

        # Parse JSON fields
        skill_definitions = (
            json.loads(row["skill_definitions"])
            if isinstance(row["skill_definitions"], str)
            else row["skill_definitions"]
        )
        selected_events = (
            json.loads(row["selected_events"])
            if isinstance(row["selected_events"], str)
            else row["selected_events"]
        )
        code_context = (
            json.loads(row["code_context"])
            if isinstance(row["code_context"], str)
            else row["code_context"]
        )
        documentation = (
            json.loads(row["documentation"])
            if isinstance(row["documentation"], str)
            else row["documentation"]
        )
        token_budget = (
            json.loads(row["token_budget"])
            if isinstance(row["token_budget"], str)
            else row["token_budget"]
        )
        relevance_scores = (
            json.loads(row["relevance_scores"])
            if isinstance(row["relevance_scores"], str)
            else row["relevance_scores"]
        )

        return Context(
            system_prompt=row["system_prompt"],
            skill_definitions=skill_definitions or [],
            selected_events=selected_events or [],
            code_context=code_context or [],
            documentation=documentation or [],
            total_tokens=row["total_tokens"],
            token_budget=token_budget,
            assembly_time_ms=row["assembly_time_ms"],
            relevance_scores=relevance_scores,
            task_type=TaskType(row["task_type"]),
        )
