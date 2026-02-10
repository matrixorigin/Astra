"""Context management for LLM agent.

Implements intelligent context selection and assembly based on:
- Relevance scoring (semantic, temporal, causal)
- Token budget allocation
- Task-aware optimization
"""

from typing import List, Dict, Any, Optional
from dataclasses import dataclass
from enum import Enum
import time

from sdk import Database
from core.logging_config import get_logger
from core.exceptions import ContextError

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
    metadata: Dict[str, Any]


@dataclass
class Context:
    """Assembled context ready for LLM."""
    system_prompt: str
    skill_definitions: List[Dict[str, Any]]
    selected_events: List[Dict[str, Any]]
    code_context: List[Dict[str, Any]]
    documentation: List[Dict[str, Any]]
    
    total_tokens: int
    token_budget: Dict[str, int]
    assembly_time_ms: int
    relevance_scores: Dict[str, float]
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
                parts.append(doc['content'])
        
        return "\n".join(parts)


class ContextManager:
    """Orchestrate context selection and assembly."""
    
    def __init__(self, db: Database, enable_snapshots: bool = False):
        """Initialize context manager.
        
        Args:
            db: Database connection
            enable_snapshots: Whether to save context snapshots (default: False)
        """
        self.db = db
        self.enable_snapshots = enable_snapshots
        logger.info(f"ContextManager initialized (snapshots={'enabled' if enable_snapshots else 'disabled'})")
    
    def build_context(
        self,
        session_id: str,
        query: str,
        max_tokens: int = 8000,
        task_type: TaskType = TaskType.GENERAL
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
            # 1. Allocate token budget
            budget = self._allocate_budget(max_tokens, task_type)
            logger.debug(f"Token budget allocated: {budget}")
            
            # 2. Retrieve candidates
            candidates = self._retrieve_candidates(session_id, query)
            logger.debug(f"Retrieved {len(candidates)} candidate events")
            
            # 3. Score and rank
            scored = self._score_candidates(query, candidates, session_id)
            logger.debug(f"Scored {len(scored)} candidates")
            
            # 4. Select within budget
            selected = self._select_within_budget(scored, budget)
            
            # 5. Assemble context
            context = self._assemble_context(
                selected, budget, task_type,
                assembly_time_ms=int((time.time() - start_time) * 1000)
            )
            
            logger.info(
                f"Context built: {context.total_tokens} tokens, "
                f"{len(context.selected_events)} events, "
                f"{context.assembly_time_ms}ms"
            )
            
            return context
            
        except Exception as e:
            logger.error(f"Failed to build context: {e}")
            raise ContextError(f"Context assembly failed: {e}")
    
    def _allocate_budget(
        self,
        total_tokens: int,
        task_type: TaskType
    ) -> Dict[str, int]:
        """Allocate token budget based on task type."""
        allocations = {
            TaskType.CODE_REVIEW: {
                "system": 500,
                "skills": 1000,
                "history": int(total_tokens * 0.2),
                "code": int(total_tokens * 0.6),
                "docs": int(total_tokens * 0.1),
                "reserve": 500
            },
            TaskType.PLANNING: {
                "system": 500,
                "skills": 1000,
                "history": int(total_tokens * 0.6),
                "code": int(total_tokens * 0.2),
                "docs": int(total_tokens * 0.1),
                "reserve": 500
            },
            TaskType.DEBUGGING: {
                "system": 500,
                "skills": 1000,
                "history": int(total_tokens * 0.2),
                "code": int(total_tokens * 0.4),
                "docs": int(total_tokens * 0.1),
                "reserve": 500
            },
            TaskType.GENERAL: {
                "system": 500,
                "skills": 1000,
                "history": int(total_tokens * 0.5),
                "code": int(total_tokens * 0.2),
                "docs": int(total_tokens * 0.1),
                "reserve": 500
            }
        }
        
        return allocations[task_type]
    
    def _retrieve_candidates(
        self,
        session_id: str,
        query: str
    ) -> List[Dict[str, Any]]:
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
            (session_id,)
        )
        
        return [dict(e) for e in events]
    
    def _score_candidates(
        self,
        query: str,
        candidates: List[Dict[str, Any]],
        session_id: str
    ) -> List[tuple[Dict[str, Any], float]]:
        """Score candidates by relevance.
        
        Multi-signal scoring:
        - Temporal: Recent events score higher
        - Causal: Events in same chain score higher
        - Keyword: Exact matches score higher
        """
        scored = []
        
        for candidate in candidates:
            # Simple scoring for MVP
            score = 0.0
            
            # Temporal score (exponential decay)
            age_hours = (time.time() - candidate['created_at'].timestamp()) / 3600
            temporal_score = 0.5 ** (age_hours / 24.0)  # Half-life of 24 hours
            
            # Keyword score (simple contains check)
            query_lower = query.lower()
            content_lower = candidate['content'].lower()
            keyword_score = 1.0 if query_lower in content_lower else 0.0
            
            # Weighted sum
            score = 0.6 * temporal_score + 0.4 * keyword_score
            
            scored.append((candidate, score))
        
        # Sort by score descending
        scored.sort(key=lambda x: x[1], reverse=True)
        
        return scored
    
    def _select_within_budget(
        self,
        scored: List[tuple[Dict[str, Any], float]],
        budget: Dict[str, int]
    ) -> List[Dict[str, Any]]:
        """Select top events within token budget."""
        selected = []
        tokens_used = 0
        history_budget = budget['history']
        
        for event, score in scored:
            # Estimate tokens (rough: 1 token ≈ 4 chars)
            event_tokens = len(event['content']) // 4
            
            if tokens_used + event_tokens <= history_budget:
                selected.append({
                    'event': event,
                    'score': score,
                    'tokens': event_tokens
                })
                tokens_used += event_tokens
            else:
                break
        
        logger.debug(f"Selected {len(selected)} events using {tokens_used} tokens")
        return selected
    
    def _assemble_context(
        self,
        selected: List[Dict[str, Any]],
        budget: Dict[str, int],
        task_type: TaskType,
        assembly_time_ms: int
    ) -> Context:
        """Assemble final context."""
        system_prompt = "You are an intelligent development agent."
        
        selected_events = [
            {
                'event_id': s['event']['event_id'],
                'event_type': s['event']['event_type'],
                'content': s['event']['content'],
                'score': s['score']
            }
            for s in selected
        ]
        
        total_tokens = sum(s['tokens'] for s in selected) + budget['system']
        
        relevance_scores = {
            s['event']['event_id']: s['score']
            for s in selected
        }
        
        return Context(
            system_prompt=system_prompt,
            skill_definitions=[],
            selected_events=selected_events,
            code_context=[],
            documentation=[],
            total_tokens=total_tokens,
            token_budget=budget,
            assembly_time_ms=assembly_time_ms,
            relevance_scores=relevance_scores,
            task_type=task_type
        )
    
    def save_snapshot(
        self,
        context: Context,
        session_id: str,
        event_id: Optional[str] = None
    ) -> Optional[str]:
        """Save context snapshot to database.
        
        Only saves if snapshots are enabled.
        
        Returns:
            snapshot_id or None if snapshots disabled
        """
        if not self.enable_snapshots:
            logger.debug("Context snapshots disabled, skipping save")
            return None
        
        from uuid_utils import uuid7
        import json
        
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
                snapshot_id, session_id, event_id,
                context.system_prompt,
                json.dumps(context.skill_definitions),
                json.dumps(context.selected_events),
                json.dumps(context.code_context),
                json.dumps(context.documentation),
                context.total_tokens,
                json.dumps(context.token_budget),
                context.assembly_time_ms,
                json.dumps(context.relevance_scores),
                context.task_type.value
            )
        )
        
        logger.info(f"Context snapshot saved: {snapshot_id}")
        return snapshot_id
    
    def load_snapshot(self, snapshot_id: str) -> Context:
        """Load context snapshot from database."""
        import json
        
        row = self.db.fetchone(
            "SELECT * FROM context_snapshots WHERE snapshot_id = %s",
            (snapshot_id,)
        )
        
        if not row:
            raise ContextError(f"Snapshot not found: {snapshot_id}")
        
        # Parse JSON fields
        skill_definitions = json.loads(row['skill_definitions']) if isinstance(row['skill_definitions'], str) else row['skill_definitions']
        selected_events = json.loads(row['selected_events']) if isinstance(row['selected_events'], str) else row['selected_events']
        code_context = json.loads(row['code_context']) if isinstance(row['code_context'], str) else row['code_context']
        documentation = json.loads(row['documentation']) if isinstance(row['documentation'], str) else row['documentation']
        token_budget = json.loads(row['token_budget']) if isinstance(row['token_budget'], str) else row['token_budget']
        relevance_scores = json.loads(row['relevance_scores']) if isinstance(row['relevance_scores'], str) else row['relevance_scores']
        
        return Context(
            system_prompt=row['system_prompt'],
            skill_definitions=skill_definitions or [],
            selected_events=selected_events or [],
            code_context=code_context or [],
            documentation=documentation or [],
            total_tokens=row['total_tokens'],
            token_budget=token_budget,
            assembly_time_ms=row['assembly_time_ms'],
            relevance_scores=relevance_scores,
            task_type=TaskType(row['task_type'])
        )
