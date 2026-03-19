"""Prompt management and optimization.

Handles system prompts, user prompts, and template management.
Supports versioning and feedback collection for prompt optimization.
"""

import time
from typing import Any

from sqlalchemy import text
from sqlalchemy.engine import Connection, Engine
from sqlalchemy.orm import sessionmaker

from core.db_consumer import DbConsumer, DbFactory
from core.logging_config import get_logger

logger = get_logger(__name__)


class PromptManager(DbConsumer):
    """Manage prompt templates and versions."""

    def __init__(self, db_factory: DbFactory, gate_trigger=None):
        super().__init__(db_factory)
        self.gate_trigger = gate_trigger
        self._cache: dict[str, str] = {}

    def get_system_prompt(
        self, template_id: str = "system_general", version: str | None = None
    ) -> str:
        """Get system prompt by ID and version.

        Args:
            template_id: Template identifier
            version: Specific version (default: latest active)

        Returns:
            Prompt content
        """
        # Check cache first (only for latest version)
        if not version and template_id in self._cache:
            return self._cache[template_id]

        try:
            if version:
                query = text("""
                    SELECT content FROM ctx_prompt_templates
                    WHERE template_id = :template_id AND version = :version
                """)
                params = {"template_id": template_id, "version": version}
            else:
                query = text("""
                    SELECT content FROM ctx_prompt_templates
                    WHERE template_id = :template_id AND is_active = 1
                    ORDER BY created_at DESC LIMIT 1
                """)
                params = {"template_id": template_id}

            with self._db() as db:
                result = db.execute(query, params).first()

            if result:
                content = result.content
                if not version:
                    self._cache[template_id] = content
                return content

            # Fallback to hardcoded defaults if DB empty
            logger.warning(f"Prompt template {template_id} not found in DB, using fallback")
            return self._get_fallback_prompt(template_id)

        except Exception as e:
            logger.error(f"Failed to fetch prompt {template_id}: {e}")
            return self._get_fallback_prompt(template_id)

    def register_prompt(
        self, template_id: str, version: str, content: str, is_active: bool = True
    ) -> None:
        """Register a new prompt version.

        Args:
            template_id: Template ID
            version: Version string
            content: Prompt content
            is_active: Whether to make this the active version
        """
        try:
            with self._db() as db:
                # If active, deactivate others
                if is_active:
                    db.execute(
                        text(
                            "UPDATE ctx_prompt_templates SET is_active = 0 WHERE template_id = :template_id"
                        ),
                        {"template_id": template_id},
                    )

                db.execute(
                    text("""
                    INSERT INTO ctx_prompt_templates (template_id, version, content, is_active, created_at, updated_at)
                    VALUES (:template_id, :version, :content, :is_active, NOW(), NOW())
                    ON DUPLICATE KEY UPDATE
                        version = VALUES(version),
                        content = VALUES(content),
                        is_active = VALUES(is_active),
                        updated_at = NOW()
                    """),
                    {
                        "template_id": template_id,
                        "content": content,
                        "version": version,
                        "is_active": 1 if is_active else 0,
                    },
                )
                db.commit()

            # Update cache
            if is_active:
                self._cache[template_id] = content

            # Auto-trigger regression gate (async, non-blocking)
            if self.gate_trigger and is_active:
                self.gate_trigger.on_prompt_change(
                    template_id=template_id,
                    version=version,
                    content=content,
                )

            logger.info(f"Registered prompt {template_id} version {version}")

        except Exception as e:
            logger.error(f"Failed to register prompt: {e}")
            raise

    def _get_fallback_prompt(self, template_id: str) -> str:
        """Get hardcoded fallback prompt."""
        fallbacks = {
            "system_code_review": "You are an expert code reviewer. Focus on correctness, security, performance, and maintainability.",
            "system_planning": "You are a technical architect. Clarify requirements, identify trade-offs, propose solutions.",
            "system_debugging": "You are a debugging expert. Reproduce, isolate, root-cause, fix, verify.",
            "system_general": "You are an intelligent development agent.",
        }
        return fallbacks.get(template_id, "You are a helpful AI assistant.")

    def clear_cache(self):
        """Clear prompt cache (call after updates)."""
        self._cache.clear()
        logger.info("Prompt cache cleared")

    def rollback_prompt(self, template_id: str) -> str | None:
        """Rollback to the previous active version of a prompt.

        Deactivates the current version and reactivates the most recent
        prior version.

        Returns:
            The version string that was reactivated, or None if no prior version exists.
        """
        try:
            with self._db() as db:
                # Find current active version
                current = db.execute(
                    text("""
                        SELECT version, created_at FROM ctx_prompt_templates
                        WHERE template_id = :tid AND is_active = 1
                        ORDER BY created_at DESC LIMIT 1
                    """),
                    {"tid": template_id},
                ).first()

                if not current:
                    logger.warning("No active version to rollback for %s", template_id)
                    return None

                # Find previous version
                previous = db.execute(
                    text("""
                        SELECT version FROM ctx_prompt_templates
                        WHERE template_id = :tid AND is_active = 0
                        ORDER BY created_at DESC LIMIT 1
                    """),
                    {"tid": template_id},
                ).first()

                if not previous:
                    logger.warning("No prior version to rollback to for %s", template_id)
                    return None

                # Deactivate current, activate previous
                db.execute(
                    text(
                        "UPDATE ctx_prompt_templates SET is_active = 0 WHERE template_id = :tid AND version = :ver"
                    ),
                    {"tid": template_id, "ver": current.version},
                )
                db.execute(
                    text(
                        "UPDATE ctx_prompt_templates SET is_active = 1 WHERE template_id = :tid AND version = :ver"
                    ),
                    {"tid": template_id, "ver": previous.version},
                )
                db.commit()

            # Invalidate cache
            self._cache.pop(template_id, None)

            logger.info(
                "Rolled back %s from %s to %s", template_id, current.version, previous.version
            )
            return previous.version

        except Exception as e:
            logger.error("Failed to rollback prompt %s: %s", template_id, e)
            raise


def init_default_prompts(db_factory: DbFactory):
    """Initialize default prompt templates."""
    manager = PromptManager(db_factory)

    prompts = [
        (
            "system_code_review",
            "1.0",
            """You are an expert code reviewer with deep knowledge of software engineering best practices.

Your responsibilities:
- Review code for correctness, security, and performance
- Identify potential bugs and edge cases
- Suggest improvements following SOLID principles
- Check for proper error handling and testing

Focus areas: {focus}

Be constructive and specific in your feedback.""",
        ),
        (
            "system_planning",
            "1.0",
            """You are a technical architect specializing in system design and planning.

Your responsibilities:
- Break down complex problems into manageable tasks
- Design scalable and maintainable solutions
- Consider trade-offs and alternatives
- Document architectural decisions

Project context: {context}

Think step-by-step and explain your reasoning.""",
        ),
        (
            "system_debugging",
            "1.0",
            """You are a debugging expert with strong analytical skills.

Your responsibilities:
- Analyze error messages and stack traces
- Identify root causes of issues
- Suggest concrete fixes with code examples
- Explain why the bug occurred

Error context: {error}

Be systematic and thorough in your analysis.""",
        ),
        (
            "system_general",
            "1.0",
            """You are an intelligent development agent.

Your capabilities:
- Writing and refactoring code
- Analyzing logs and errors
- Planning tasks and architectures
- Executing tools and skills

Always:
- Think before you act
- Verify your changes
- Explain your decisions""",
        ),
    ]

    for template_id, version, content in prompts:
        try:
            manager.register_prompt(template_id, version, content, is_active=True)
        except Exception as e:
            logger.warning(f"Failed to register {template_id}: {e}")


# =============================================================================
# Phase 2: Feedback Collection
# =============================================================================


class PromptFeedback(DbConsumer):
    """Collect and analyze user feedback on LLM responses."""

    def __init__(self, db_factory: DbFactory):
        super().__init__(db_factory)

    def record_feedback(
        self,
        prompt_template_id: str,
        prompt_version: str,
        llm_request_id: str,
        user_rating: int,
        user_comment: str | None = None,
        metadata: dict[str, str] | None = None,
    ) -> str:
        """Record user feedback for a prompt.

        Args:
            prompt_template_id: Template ID (e.g., 'system_code_review')
            prompt_version: Version used (e.g., '1.0')
            llm_request_id: LLM request ID for tracing
            user_rating: Rating 1-5 (1=poor, 5=excellent)
            user_comment: Optional text feedback
            metadata: Optional additional data

        Returns:
            feedback_id
        """
        from uuid_utils import uuid7

        if not 1 <= user_rating <= 5:
            raise ValueError(f"Rating must be 1-5, got {user_rating}")

        feedback_id = str(uuid7())

        with self._db() as db:
            from api.models import LLMFeedback

            db.add(
                LLMFeedback(
                    feedback_id=feedback_id,
                    prompt_template_id=prompt_template_id,
                    prompt_version=prompt_version,
                    llm_request_id=llm_request_id,
                    rating=user_rating,
                    comment=user_comment,
                    feedback_metadata=metadata or {},
                )
            )
            db.flush()
            db.commit()
            db.expire_all()
            bind = db.get_bind()
            if isinstance(bind, (Engine, Connection)):
                for attempt in range(6):
                    fresh_db = sessionmaker(bind=bind, expire_on_commit=False)()
                    try:
                        row = (
                            fresh_db.query(LLMFeedback.feedback_id)
                            .filter(LLMFeedback.feedback_id == feedback_id)
                            .first()
                        )
                    finally:
                        fresh_db.close()
                    if row is not None:
                        break
                    if attempt < 5:
                        time.sleep(0.03 * (attempt + 1))

        logger.info(
            f"Recorded feedback: {prompt_template_id}@{prompt_version} rating={user_rating}"
        )
        return feedback_id

    def get_feedback_stats(
        self, prompt_template_id: str, prompt_version: str | None = None
    ) -> dict[str, Any]:
        """Get feedback statistics for a prompt."""
        if prompt_version:
            where_clause = "WHERE prompt_template_id = :template_id AND prompt_version = :version"
            params = {"template_id": prompt_template_id, "version": prompt_version}
        else:
            where_clause = "WHERE prompt_template_id = :template_id"
            params = {"template_id": prompt_template_id}

        with self._db() as db:
            result = db.execute(
                text(f"""
                SELECT
                    COUNT(*) as total_count,
                    AVG(rating) as avg_rating,
                    MIN(rating) as min_rating,
                    MAX(rating) as max_rating
                FROM eval_llm_feedback
                {where_clause}
                """),
                params,
            )
            stats_row = result.first()

            result = db.execute(
                text(f"""
                SELECT rating, COUNT(*) as count
                FROM eval_llm_feedback
                {where_clause}
                GROUP BY rating
                ORDER BY rating
                """),
                params,
            )
            distribution = result.fetchall()

        return {
            "total_count": stats_row.total_count if stats_row else 0,
            "avg_rating": float(stats_row.avg_rating)
            if stats_row and stats_row.avg_rating
            else 0.0,
            "min_rating": stats_row.min_rating if stats_row else 0,
            "max_rating": stats_row.max_rating if stats_row else 0,
            "distribution": {row.rating: row.count for row in distribution},
        }

    def get_low_score_cases(
        self, prompt_template_id: str, threshold: int = 2, limit: int = 100
    ) -> list[dict[str, Any]]:
        """Get low-scoring feedback cases for analysis."""
        with self._db() as db:
            result = db.execute(
                text("""
                SELECT
                    feedback_id,
                    prompt_version,
                    llm_request_id,
                    rating,
                    comment,
                    metadata,
                    created_at
                FROM eval_llm_feedback
                WHERE prompt_template_id = :template_id AND rating <= :threshold
                ORDER BY created_at DESC
                LIMIT :limit
                """),
                {"template_id": prompt_template_id, "threshold": threshold, "limit": limit},
            )
            rows = result.fetchall()

        return [dict(row._mapping) for row in rows]

    def compare_versions(
        self, prompt_template_id: str, version_a: str, version_b: str
    ) -> dict[str, Any]:
        """Compare feedback between two prompt versions."""
        stats_a = self.get_feedback_stats(prompt_template_id, version_a)
        stats_b = self.get_feedback_stats(prompt_template_id, version_b)

        return {
            "version_a": version_a,
            "version_b": version_b,
            "stats_a": stats_a,
            "stats_b": stats_b,
            "improvement": stats_b["avg_rating"] - stats_a["avg_rating"]
            if stats_a["avg_rating"] and stats_b["avg_rating"]
            else None,
        }
