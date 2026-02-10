"""Prompt template management with versioning and dynamic updates."""

from typing import Optional, Dict
from sdk import Database
from core.logging_config import get_logger

logger = get_logger(__name__)


class PromptManager:
    """Manage versioned prompt templates."""
    
    def __init__(self, db: Database):
        self.db = db
        self._cache = {}  # Simple in-memory cache
    
    def get_prompt(
        self,
        template_id: str,
        version: Optional[str] = None,
        variables: Optional[Dict[str, str]] = None
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
                row = self.db.fetchone("""
                    SELECT content
                    FROM prompt_templates
                    WHERE template_id = %s AND version = %s
                """, (template_id, version))
            else:
                # Get latest active version
                row = self.db.fetchone("""
                    SELECT content
                    FROM prompt_templates
                    WHERE template_id = %s AND is_active = 1
                    ORDER BY effective_at DESC, created_at DESC
                    LIMIT 1
                """, (template_id,))
            
            if not row:
                logger.warning(f"Prompt template not found: {template_id}@{version}")
                return self._get_fallback_prompt(template_id)
            
            content = row['content']
            self._cache[cache_key] = content
        
        # Substitute variables
        if variables:
            for key, value in variables.items():
                content = content.replace(f"{{{key}}}", value)
        
        return content
    
    def register_prompt(
        self,
        template_id: str,
        version: str,
        content: str,
        is_active: bool = True
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
            self.db.execute("""
                UPDATE prompt_templates
                SET is_active = 0
                WHERE template_id = %s
            """, (template_id,))
        
        # Insert new version
        self.db.execute("""
            INSERT INTO prompt_templates
            (template_id, version, content, is_active, effective_at)
            VALUES (%s, %s, %s, %s, NOW())
            ON DUPLICATE KEY UPDATE
                content = VALUES(content),
                is_active = VALUES(is_active),
                effective_at = VALUES(effective_at)
        """, (template_id, version, content, is_active))
        
        # Clear cache
        self._cache.clear()
        
        logger.info(f"Registered prompt: {template_id}@{version} (active={is_active})")
    
    def _get_fallback_prompt(self, template_id: str) -> str:
        """Get hardcoded fallback prompt."""
        fallbacks = {
            'system_code_review': "You are an expert code reviewer. Focus on code quality, security, and best practices.",
            'system_planning': "You are a technical architect. Help plan and design solutions.",
            'system_debugging': "You are a debugging expert. Help identify and fix issues.",
            'system_general': "You are an intelligent development agent."
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
            'system_code_review',
            '1.0',
            """You are an expert code reviewer with deep knowledge of software engineering best practices.

Your responsibilities:
- Review code for correctness, security, and performance
- Identify potential bugs and edge cases
- Suggest improvements following SOLID principles
- Check for proper error handling and testing

Focus areas: {focus}

Be constructive and specific in your feedback."""
        ),
        (
            'system_planning',
            '1.0',
            """You are a technical architect specializing in system design and planning.

Your responsibilities:
- Break down complex problems into manageable tasks
- Design scalable and maintainable solutions
- Consider trade-offs and alternatives
- Document architectural decisions

Project context: {context}

Think step-by-step and explain your reasoning."""
        ),
        (
            'system_debugging',
            '1.0',
            """You are a debugging expert with strong analytical skills.

Your responsibilities:
- Analyze error messages and stack traces
- Identify root causes of issues
- Suggest concrete fixes with code examples
- Explain why the bug occurred

Error context: {error}

Be systematic and thorough in your analysis."""
        ),
        (
            'system_general',
            '1.0',
            """You are an intelligent development agent helping with software development tasks.

Your capabilities:
- Answer technical questions
- Write and review code
- Debug issues
- Explain concepts

Be helpful, accurate, and concise."""
        )
    ]
    
    for template_id, version, content in prompts:
        try:
            manager.register_prompt(template_id, version, content, is_active=True)
        except Exception as e:
            logger.warning(f"Failed to register {template_id}: {e}")


# =============================================================================
# TODO: Phase 2 - Feedback Collection (Future)
# =============================================================================
# 
# Collect user feedback on LLM responses to enable prompt optimization.
# 
# Dependencies:
# - llm_feedback table (already exists in schema)
# - User rating UI/API
# - Feedback analysis tools
#
# Example implementation:
#
# class PromptFeedback:
#     def record_feedback(
#         self,
#         prompt_id: str,
#         prompt_version: str,
#         llm_response: str,
#         user_rating: int,  # 1-5
#         user_comment: Optional[str] = None
#     ):
#         """Record user feedback for a prompt."""
#         self.db.execute("""
#             INSERT INTO llm_feedback
#             (prompt_template_id, prompt_version, rating, comment, created_at)
#             VALUES (%s, %s, %s, %s, NOW())
#         """, (prompt_id, prompt_version, user_rating, user_comment))
#
# =============================================================================


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
