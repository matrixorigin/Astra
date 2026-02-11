"""Prompt template management with versioning and dynamic updates."""

from typing import Any

from core.logging_config import get_logger
from sdk import Database

logger = get_logger(__name__)


class PromptManager:
    """Manage versioned prompt templates."""

    def __init__(self, db: Database):
        self.db = db
        self._cache: dict[str, Any] = {}  # Simple in-memory cache

    def get_prompt(
        self,
        template_id: str,
        version: str | None = None,
        variables: dict[str, str] | None = None,
    ) -> str:
        """Get prompt template by ID and version.

        Args:
            template_id: Template identifier (e.g., 'system_code_review')
            version: Specific version (e.g., '1.0'), or None for latest active
            variables: Variables to substitute in template

        Returns:
            Rendered prompt content
        """
        cache_key = f"{template_id}:{version or 'latest'}"

        # Check cache
        if cache_key in self._cache:
            content = self._cache[cache_key]
        else:
            # Load from database
            if version:
                row = self.db.fetchone(
                    """
                    SELECT content
                    FROM prompt_templates
                    WHERE template_id = %s AND version = %s
                """,
                    (template_id, version),
                )
            else:
                # Get latest active version
                row = self.db.fetchone(
                    """
                    SELECT content
                    FROM prompt_templates
                    WHERE template_id = %s AND is_active = 1
                    ORDER BY effective_at DESC, created_at DESC
                    LIMIT 1
                """,
                    (template_id,),
                )

            if not row:
                logger.warning(f"Prompt template not found: {template_id}@{version}")
                return self._get_fallback_prompt(template_id)

            content = row["content"]
            self._cache[cache_key] = content

        # Substitute variables
        if variables:
            for key, value in variables.items():
                content = content.replace(f"{{{key}}}", value)

        return content

    def register_prompt(
        self, template_id: str, version: str, content: str, is_active: bool = True
    ) -> None:
        """Register a new prompt template version.

        Args:
            template_id: Template identifier
            version: Version string (e.g., '1.0', '1.1')
            content: Prompt content (can include {variables})
            is_active: Whether this version is active
        """
        # Deactivate old versions if this is active
        if is_active:
            self.db.execute(
                """
                UPDATE prompt_templates
                SET is_active = 0
                WHERE template_id = %s
            """,
                (template_id,),
            )

        # Insert new version
        self.db.execute(
            """
            INSERT INTO prompt_templates
            (template_id, version, content, is_active, effective_at)
            VALUES (%s, %s, %s, %s, NOW())
            ON DUPLICATE KEY UPDATE
                content = VALUES(content),
                is_active = VALUES(is_active),
                effective_at = VALUES(effective_at)
        """,
            (template_id, version, content, is_active),
        )

        # Clear cache
        self._cache.clear()

        logger.info(f"Registered prompt: {template_id}@{version} (active={is_active})")

    def _get_fallback_prompt(self, template_id: str) -> str:
        """Get hardcoded fallback prompt."""
        fallbacks = {
            "system_code_review": "You are an expert code reviewer. Focus on code quality, security, and best practices.",
            "system_planning": "You are a technical architect. Help plan and design solutions.",
            "system_debugging": "You are a debugging expert. Help identify and fix issues.",
            "system_general": "You are an intelligent development agent.",
        }
        return fallbacks.get(template_id, "You are a helpful AI assistant.")

    def clear_cache(self):
        """Clear prompt cache (call after updates)."""
        self._cache.clear()
        logger.info("Prompt cache cleared")


def init_default_prompts(db: Database):
    """Initialize default prompt templates."""
    manager = PromptManager(db)

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
            """You are an intelligent development agent helping with software development tasks.

Your capabilities:
- Answer technical questions
- Write and review code
- Debug issues
- Explain concepts

Be helpful, accurate, and concise.""",
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


class PromptFeedback:
    """Collect and analyze user feedback on LLM responses."""

    def __init__(self, db: Database):
        self.db = db

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
        import json

        from uuid_utils import uuid7

        if not 1 <= user_rating <= 5:
            raise ValueError(f"Rating must be 1-5, got {user_rating}")

        feedback_id = str(uuid7())

        self.db.execute(
            """
            INSERT INTO llm_feedback
            (feedback_id, prompt_template_id, prompt_version, llm_request_id,
             rating, comment, metadata, created_at)
            VALUES (%s, %s, %s, %s, %s, %s, %s, NOW())
        """,
            (
                feedback_id,
                prompt_template_id,
                prompt_version,
                llm_request_id,
                user_rating,
                user_comment,
                json.dumps(metadata or {}),
            ),
        )

        logger.info(
            f"Recorded feedback: {prompt_template_id}@{prompt_version} rating={user_rating}"
        )
        return feedback_id

    def get_feedback_stats(
        self, prompt_template_id: str, prompt_version: str | None = None
    ) -> dict[str, Any]:
        """Get feedback statistics for a prompt."""
        if prompt_version:
            where_clause = "WHERE prompt_template_id = %s AND prompt_version = %s"
            params: tuple[str, ...] = (prompt_template_id, prompt_version)
        else:
            where_clause = "WHERE prompt_template_id = %s"
            params = (prompt_template_id,)

        stats_row = self.db.fetchone(
            f"""
            SELECT
                COUNT(*) as total_count,
                AVG(rating) as avg_rating,
                MIN(rating) as min_rating,
                MAX(rating) as max_rating
            FROM llm_feedback
            {where_clause}
        """,
            params,
        )

        distribution = self.db.fetchall(
            f"""
            SELECT rating, COUNT(*) as count
            FROM llm_feedback
            {where_clause}
            GROUP BY rating
            ORDER BY rating
        """,
            params,
        )

        return {
            "total_count": stats_row.get("total_count", 0) if stats_row else 0,
            "avg_rating": float(stats_row.get("avg_rating", 0))
            if stats_row and stats_row.get("avg_rating")
            else 0.0,
            "min_rating": stats_row.get("min_rating", 0) if stats_row else 0,
            "max_rating": stats_row.get("max_rating", 0) if stats_row else 0,
            "distribution": {row["rating"]: row["count"] for row in distribution},
        }

    def get_low_score_cases(
        self, prompt_template_id: str, threshold: int = 2, limit: int = 100
    ) -> list[dict[str, Any]]:
        """Get low-scoring feedback cases for analysis."""
        rows = self.db.fetchall(
            """
            SELECT
                feedback_id,
                prompt_version,
                llm_request_id,
                rating,
                comment,
                metadata,
                created_at
            FROM llm_feedback
            WHERE prompt_template_id = %s AND rating <= %s
            ORDER BY created_at DESC
            LIMIT %s
        """,
            (prompt_template_id, threshold, limit),
        )

        return [dict(row) for row in rows]

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


# =============================================================================
# TODO: Phase 3 - Automatic Optimization (Future - Complex Dependencies)
# =============================================================================
#
# Automatically optimize prompts based on feedback data.
#
# ⚠️ WARNING: This is a complex feature with many dependencies:
# - Large amount of feedback data (1000+ samples per prompt)
# - LLM for analysis (GPT-4 or similar)
# - A/B testing infrastructure
# - Statistical significance testing
# - Human review process
#
# DO NOT implement until:
# 1. Phase 2 (Feedback Collection) is complete
# 2. Sufficient feedback data is collected (6+ months)
# 3. Clear ROI is demonstrated
#
# Placeholder design:
#
# class PromptOptimizer:
#     def analyze_low_scores(self, template_id: str) -> Dict[str, Any]:
#         """Analyze low-scoring cases to identify issues."""
#         # Query low-score cases
#         # Use LLM to analyze patterns
#         # Return analysis report
#         pass
#
#     def suggest_improvements(self, template_id: str) -> str:
#         """Generate improved prompt based on analysis."""
#         # Get current prompt
#         # Get analysis report
#         # Use LLM to generate improved version
#         # Return new prompt (requires human review!)
#         pass
#
# =============================================================================
