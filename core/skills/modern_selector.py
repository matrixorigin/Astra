"""Internal modern skill selector with native LLM function calling.

⚠️ DO NOT USE DIRECTLY - Use SkillPipeline instead.

This module is an internal implementation detail used by SkillPipeline.
External code should use SkillPipeline from core.skills.pipeline.
"""

import json
from typing import Any

from sqlalchemy.orm import Session

from core.logging_config import get_logger
from core.skills.selector import SkillMetadata, SkillSelector

logger = get_logger(__name__)


class ModernSkillSelector:
    """Skill selector using native LLM function calling (OpenAI/Gemini/DeepSeek)."""

    def __init__(self, session: Session, llm_client=None):
        if not isinstance(session, Session):
            raise TypeError("session must be a SQLAlchemy Session")
        
        self.session = session
        self.llm = llm_client
        self.rule_selector = SkillSelector(session)

    def get_tools_schema(
        self,
        query: str,
        max_candidates: int = 5,
    ) -> list[dict[str, Any]]:
        """Return OpenAI tool schemas for candidate skills (no LLM call)."""
        candidates = self.rule_selector.select_skills(query, max_skills=max_candidates)
        if not candidates:
            return []
        return [self._skill_to_tool_schema(skill) for skill in candidates]

    def select_and_execute(
        self, query: str, context: dict[str, Any] | None = None, max_candidates: int = 5
    ) -> list[dict[str, Any]]:
        """Select skills and extract parameters using native function calling.

        This is the "灵魂升华" - LLM directly outputs function calls with parameters.

        Args:
            query: User query
            context: Optional context
            max_candidates: Max skills to consider (for retrieval)

        Returns:
            List of tool calls with parameters
        """
        # Step 1: Retrieval (粗筛) - Use rule-based selector
        candidates = self.rule_selector.select_skills(query, max_skills=max_candidates)

        if not candidates:
            logger.info("No candidate skills found")
            return []

        logger.info(f"Retrieved {len(candidates)} candidate skills")

        # Step 2: Convert to OpenAI tool schema (精调)
        tools_schema = [self._skill_to_tool_schema(skill) for skill in candidates]

        # Step 3: Native function calling (一步到位)
        messages = [
            {
                "role": "system",
                "content": "You are a development assistant. Use the available tools to help the user.",
            },
            {"role": "user", "content": query},
        ]

        try:
            response = self.llm.chat_with_tools(
                messages=messages,
                tools=tools_schema,
                tool_choice="auto",  # Let LLM decide
            )

            # Extract tool calls
            tool_calls: list[dict[str, Any]] = response.get("tool_calls", [])

            logger.info(
                f"LLM selected {len(tool_calls)} tools: {[t['function']['name'] for t in tool_calls]}"
            )

            return tool_calls

        except Exception as e:
            logger.error(f"Function calling failed: {e}")
            # Fallback to rule-based
            return self._fallback_to_rules(candidates, query)

    def _skill_to_tool_schema(self, skill: SkillMetadata) -> dict[str, Any]:
        """Convert skill metadata to OpenAI tool schema.

        Auto-generates schema from skill's input model (Pydantic).
        """
        # Try to load skill class and extract schema
        try:
            # Get skill from registry
            from core.skills.registry import SkillRegistry

            registry = SkillRegistry(self.session)
            skill_def = registry.get_skill(skill.name)

            if skill_def and hasattr(skill_def, "requirements"):
                # Use Pydantic model's schema
                input_model = skill_def.requirements
                if hasattr(input_model, "model_json_schema"):
                    # Pydantic v2
                    parameters = input_model.model_json_schema()
                elif hasattr(input_model, "schema"):
                    # Pydantic v1
                    parameters = input_model.schema()
                else:
                    parameters = self._get_default_schema(skill.name)
            else:
                parameters = self._get_default_schema(skill.name)

        except Exception as e:
            logger.warning(f"Failed to auto-generate schema for {skill.name}: {e}")
            parameters = self._get_default_schema(skill.name)

        return {
            "type": "function",
            "function": {
                "name": skill.name,
                "description": skill.description,
                "parameters": parameters,
            },
        }

    def _get_default_schema(self, skill_name: str) -> dict[str, Any]:
        """Get default schema for known skills (fallback)."""
        schemas = {
            "summarize_pr": {
                "type": "object",
                "properties": {
                    "repo_id": {"type": "string", "description": "Repository ID"},
                    "pr_number": {"type": "integer", "description": "Pull request number"},
                },
                "required": ["repo_id", "pr_number"],
            },
            "code_review": {
                "type": "object",
                "properties": {
                    "repo_id": {"type": "string", "description": "Repository ID"},
                    "pr_number": {"type": "integer", "description": "Pull request number"},
                    "focus": {
                        "type": "string",
                        "enum": ["all", "security", "performance", "style"],
                        "description": "Review focus area",
                    },
                },
                "required": ["repo_id", "pr_number"],
            },
            "list_prs": {
                "type": "object",
                "properties": {
                    "repo_id": {"type": "string", "description": "Repository ID"},
                    "state": {
                        "type": "string",
                        "enum": ["open", "closed", "all"],
                        "description": "PR state filter",
                    },
                    "limit": {"type": "integer", "description": "Max number of PRs"},
                },
                "required": ["repo_id"],
            },
            "ci_status": {
                "type": "object",
                "properties": {
                    "repo_id": {"type": "string", "description": "Repository ID"},
                    "limit": {"type": "integer", "description": "Max number of runs"},
                },
                "required": ["repo_id"],
            },
            "search_code": {
                "type": "object",
                "properties": {
                    "repo_id": {"type": "string", "description": "Repository ID"},
                    "query": {"type": "string", "description": "Search query"},
                    "file_pattern": {"type": "string", "description": "File pattern filter"},
                },
                "required": ["repo_id", "query"],
            },
            "analyze_bug": {
                "type": "object",
                "properties": {
                    "repo_id": {"type": "string", "description": "Repository ID"},
                    "issue_number": {"type": "integer", "description": "Issue number"},
                },
                "required": ["repo_id", "issue_number"],
            },
        }

        return schemas.get(skill_name, {"type": "object", "properties": {}, "required": []})

    def _fallback_to_rules(
        self, candidates: list[SkillMetadata], query: str
    ) -> list[dict[str, Any]]:
        """Fallback to rule-based selection if function calling fails."""
        logger.warning("Falling back to rule-based selection")

        # Return top skill with empty parameters
        if candidates:
            return [{"function": {"name": candidates[0].name, "arguments": "{}"}}]

        return []


class ModelRouter:
    """Route queries to different LLM models based on skill category.

    This implements "Mixture of Agents" pattern.
    """

    def __init__(self, session: Session | None = None):
        self._session = session
        self.model_mapping = {
            # Code-related: Use DeepSeek Coder or Claude
            "code": {"model": "deepseek-coder", "style": "concise", "temperature": 0.2},
            # GitHub/Issue management: Use GPT-4
            "github": {"model": "gpt-4", "style": "structured", "temperature": 0.3},
            # Documentation: Use Claude (best at writing)
            "docs": {"model": "claude-3-sonnet", "style": "detailed", "temperature": 0.5},
            # Default
            "default": {"model": "gpt-4", "style": "balanced", "temperature": 0.3},
        }

    def route(self, skill: SkillMetadata, query: str) -> dict[str, Any]:
        """Route to appropriate model based on skill category.

        Args:
            skill: Selected skill
            query: User query

        Returns:
            Model configuration
        """
        category = skill.category
        config = self.model_mapping.get(category, self.model_mapping["default"])

        logger.info(f"Routing {skill.name} to {config['model']} (category={category})")

        # Adjust based on priority
        if skill.priority >= 9:
            # High priority: Use best model, lower temperature
            config = config.copy()
            config["model"] = "gpt-4"
            config["temperature"] = 0.1
            logger.info(f"High priority skill, upgraded to {config['model']}")

        # Adjust based on cost
        if skill.cost_estimate == "high":
            # High cost: Add budget warning
            config = config.copy()
            config["budget_warning"] = True
            logger.warning(f"High cost skill: {skill.name}")

        return config

    def get_system_prompt(self, category: str, style: str) -> str:
        """Get category-specific system prompt."""
        prompts = {
            "code": "You are an expert code analyst. Be concise and technical. Focus on code quality, bugs, and best practices.",
            "github": "You are a GitHub workflow expert. Provide structured, actionable insights about PRs, issues, and CI/CD.",
            "docs": "You are a technical writer. Provide clear, detailed documentation with examples.",
            "default": "You are a helpful development assistant.",
        }

        return prompts.get(category, prompts["default"])


class AdaptiveSkillOrchestrator:
    """Orchestrator with model routing and adaptive execution.

    This is the "千人千面" implementation.
    """

    def __init__(self, session: Session | None = None, llm_client=None):
        self._session = session
        self.selector = ModernSkillSelector(session, llm_client)
        self.router = ModelRouter(session)
        self.llm = llm_client

    async def execute_query(
        self, query: str, session_id: str, user_preferences: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        """Execute query with adaptive model routing.

        Args:
            query: User query
            session_id: Session ID
            user_preferences: Optional user preferences

        Returns:
            Execution result
        """
        # Step 1: Select skills with native function calling
        tool_calls = self.selector.select_and_execute(query)

        if not tool_calls:
            return {
                "response": "I couldn't find a suitable tool for this query.",
                "skills_used": [],
                "model_used": "none",
            }

        # Step 2: Execute each tool with appropriate model
        results = []
        for tool_call in tool_calls:
            skill_name = tool_call["function"]["name"]
            arguments = json.loads(tool_call["function"]["arguments"])

            # Get skill metadata
            skill = self.selector.rule_selector.get_skill_by_name(skill_name)
            if not skill:
                continue

            # Route to appropriate model
            model_config = self.router.route(skill, query)

            # Execute skill
            try:
                result = await self._execute_skill(skill_name, arguments, model_config)
                results.append(
                    {"skill": skill_name, "result": result, "model": model_config["model"]}
                )
            except Exception as e:
                logger.error(f"Skill execution failed: {skill_name} - {e}")
                results.append(
                    {"skill": skill_name, "error": str(e), "model": model_config["model"]}
                )

        # Step 3: Synthesize final response
        final_response = self._synthesize_response(query, results)

        return {
            "response": final_response,
            "skills_used": [r["skill"] for r in results],
            "models_used": list({r["model"] for r in results}),
            "skill_results": results,
        }

    async def _execute_skill(
        self, skill_name: str, arguments: dict[str, Any], model_config: dict[str, Any]
    ) -> Any:
        """Execute a skill with given arguments."""
        # This would call the actual skill implementation
        # For now, return mock result
        return {"status": "success", "data": f"Executed {skill_name} with {arguments}"}

    def _synthesize_response(self, query: str, results: list[dict[str, Any]]) -> str:
        """Synthesize final response from skill results."""
        if not results:
            return "No results available."

        # Simple synthesis for now
        response_parts = []
        for result in results:
            if "error" in result:
                response_parts.append(f"❌ {result['skill']}: {result['error']}")
            else:
                response_parts.append(f"✅ {result['skill']}: {result['result']}")

        return "\n".join(response_parts)
