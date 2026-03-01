"""Session evaluation skill — enables agent self-assessment.

Analyzes agent performance metrics (tokens, LLM calls, skill usage) for a session.
"""

from __future__ import annotations

import json
from typing import TYPE_CHECKING

from pydantic import Field
from sqlalchemy import text

from core.logging_config import get_logger
from core.skills.base import (
    AccessScope,
    RepoType,
    RuntimeRequirement,
    SideEffectCategory,
    SideEffectProfile,
    Skill,
    SkillInput,
    SkillOutput,
    SkillRequirement,
)

if TYPE_CHECKING:
    from sqlalchemy.orm import Session

logger = get_logger(__name__)


class EvaluateSessionInput(SkillInput):
    """Input for session evaluation."""

    # Named 'target_session_id' to avoid collision with SkillInput.session_id
    # (which is a framework-injected field filtered from OpenAI schema)
    target_session_id: str = Field(..., description="Session ID to evaluate")
    include_details: bool = Field(default=False, description="Include detailed event breakdown")


class EvaluateSessionOutput(SkillOutput):
    """Output from session evaluation."""

    session_id: str | None = None
    total_events: int = 0
    user_queries: int = 0
    llm_calls: int = 0
    tokens: dict = Field(default_factory=dict)
    skills: dict = Field(default_factory=dict)
    assessment: dict = Field(default_factory=dict)
    event_breakdown: list[dict] | None = None


class EvaluateSessionSkill(Skill[EvaluateSessionInput, EvaluateSessionOutput]):
    """Evaluate agent performance in a session."""

    name = "evaluate_session"
    version = "1.0.0"
    description = "Evaluate agent performance metrics for a session"
    requirements = SkillRequirement(
        runtime=[RuntimeRequirement.DATABASE],
        repo_types=[RepoType.CODE],
        min_access=AccessScope.READ,
        llm_required=False,
        timeout_seconds=30,
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ,
        external_apis=[],
    )

    def __init__(self, db: Session | None = None) -> None:
        self._db = db

    async def execute(self, input_data: EvaluateSessionInput) -> EvaluateSessionOutput:
        """Evaluate session performance."""
        from api.database import get_db_context

        # Use injected db if available, otherwise create new session
        if self._db is not None:
            return self._execute_with_db(self._db, input_data)

        with get_db_context() as db:
            return self._execute_with_db(db, input_data)

    def _execute_with_db(
        self, db: Session, input_data: EvaluateSessionInput
    ) -> EvaluateSessionOutput:
        """Execute evaluation with given database session."""
        result = db.execute(
            text("""
                SELECT event_type, token_usage, llm_model_used, skill_name, content
                FROM agent_events
                WHERE session_id = :session_id
                ORDER BY created_at
            """),
            {"session_id": input_data.target_session_id},
        )
        rows = result.mappings().all()

        if not rows:
            return EvaluateSessionOutput(
                success=False,
                error=f"No events found for session {input_data.target_session_id}",
            )

        metrics = self._calculate_metrics(rows, input_data.target_session_id)

        if input_data.include_details:
            metrics["event_breakdown"] = self._get_event_breakdown(rows)

        metrics["assessment"] = self._generate_assessment(metrics)

        return EvaluateSessionOutput(success=True, **metrics)

    def _calculate_metrics(self, rows: list, session_id: str) -> dict:
        """Calculate performance metrics from event rows."""
        total_prompt = 0
        total_completion = 0
        llm_calls = 0
        user_queries = 0
        skills_used: list[str] = []

        for row in rows:
            if row["event_type"] == "user_query":
                user_queries += 1

            if row["token_usage"]:
                usage = self._parse_token_usage(row["token_usage"])
                if usage:
                    total_prompt += usage.get("prompt", usage.get("prompt_tokens", 0))
                    total_completion += usage.get("completion", usage.get("completion_tokens", 0))
                    llm_calls += 1

            if row["skill_name"]:
                skills_used.append(row["skill_name"])

        total_tokens = total_prompt + total_completion

        return {
            "session_id": session_id,
            "total_events": len(rows),
            "user_queries": user_queries,
            "llm_calls": llm_calls,
            "tokens": {
                "prompt": total_prompt,
                "completion": total_completion,
                "total": total_tokens,
                "avg_per_call": total_tokens // llm_calls if llm_calls > 0 else 0,
            },
            "skills": {
                "unique": len(set(skills_used)),
                "total_calls": len(skills_used),
                "breakdown": {s: skills_used.count(s) for s in set(skills_used)},
            },
        }

    def _parse_token_usage(self, token_usage: str | dict) -> dict | None:
        """Parse token_usage field, handling both JSON string and dict."""
        if isinstance(token_usage, dict):
            return token_usage
        try:
            return json.loads(token_usage)
        except (json.JSONDecodeError, TypeError) as e:
            logger.debug(f"Failed to parse token_usage: {e}")
            return None

    def _get_event_breakdown(self, rows: list) -> list[dict]:
        """Get detailed event breakdown."""
        breakdown = []
        for i, row in enumerate(rows, 1):
            entry: dict = {
                "index": i,
                "type": row["event_type"],
                "model": row["llm_model_used"],
                "skill": row["skill_name"],
            }

            if row["token_usage"]:
                usage = self._parse_token_usage(row["token_usage"])
                if usage:
                    entry["tokens"] = usage.get("total", 0)

            breakdown.append(entry)

        return breakdown

    def _generate_assessment(self, metrics: dict) -> dict:
        """Generate qualitative assessment based on metrics."""
        tokens = metrics["tokens"]
        queries = metrics["user_queries"]
        llm_calls = metrics["llm_calls"]

        # Tokens per query assessment
        tokens_per_query = tokens["total"] // queries if queries > 0 else 0
        if tokens_per_query < 10000:
            token_efficiency = "excellent"
        elif tokens_per_query < 20000:
            token_efficiency = "good"
        elif tokens_per_query < 40000:
            token_efficiency = "moderate"
        else:
            token_efficiency = "needs_improvement"

        # LLM calls per query assessment
        calls_per_query = llm_calls / queries if queries > 0 else 0.0
        if calls_per_query <= 2:
            call_efficiency = "excellent"
        elif calls_per_query <= 4:
            call_efficiency = "good"
        elif calls_per_query <= 6:
            call_efficiency = "moderate"
        else:
            call_efficiency = "needs_improvement"

        # Overall: good only if both token and call efficiency are good or better
        overall = (
            "good"
            if token_efficiency in ("excellent", "good")
            and call_efficiency in ("excellent", "good")
            else "needs_improvement"
        )

        return {
            "token_efficiency": token_efficiency,
            "tokens_per_query": tokens_per_query,
            "call_efficiency": call_efficiency,
            "calls_per_query": round(calls_per_query, 1),
            "overall": overall,
        }
