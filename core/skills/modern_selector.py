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

_DEFAULT_CONTEXT_BUDGET = 2000  # tokens reserved for tool schemas


def _estimate_tokens(obj: Any) -> int:
    """Estimate token count from serialized JSON size (~4 chars per token)."""
    return len(json.dumps(obj, default=str)) // 4


class ModernSkillSelector:
    """Skill selector using native LLM function calling (OpenAI/Gemini/DeepSeek)."""

    def __init__(self, session: Session, llm_client=None, *, embed_fn=None):
        if not isinstance(session, Session):
            raise TypeError("session must be a SQLAlchemy Session")
        
        self.session = session
        self.llm = llm_client
        self.rule_selector = SkillSelector(session)

        # Cache registry for schema lookups (avoid re-instantiation per skill)
        from core.skills.registry import SkillRegistry
        self._registry = SkillRegistry(session)

        # Semantic index — primary retrieval path when available
        from core.skills.skill_index import SkillIndex
        self._index = SkillIndex(embed_fn=embed_fn)
        if embed_fn:
            self._index.build(list(self.rule_selector.skills.values()))

    def get_tools_schema(
        self,
        query: str,
        max_candidates: int = 5,
        *,
        context_budget: int = _DEFAULT_CONTEXT_BUDGET,
    ) -> tuple[list[dict[str, Any]], str]:
        """Return OpenAI tool schemas using progressive disclosure.

        Stage 1 (Tier 1): Retrieve candidates via rule-based matching on
                          lightweight metadata. Zero prompt tokens.
        Stage 2 (Tier 3): Build full schema for each candidate, measure real
                          token cost, include only if within budget.
                          Skills that don't fit are excluded entirely —
                          no empty stubs (they waste tokens and confuse LLMs).
        
        Returns:
            (tools, retrieval_method) where retrieval_method is "semantic" or "keyword"
        """
        # --- Stage 1: retrieve candidates ---
        # Prefer semantic index; fall back to keyword matching
        hit_names = self._index.query(query, top_k=max_candidates * 2)
        if hit_names:
            candidates = [
                self.rule_selector.skills[n]
                for n in hit_names
                if n in self.rule_selector.skills
            ]
            retrieval_method = "semantic"
            logger.debug("Semantic retrieval: %d candidates", len(candidates))
        else:
            candidates = self.rule_selector.select_skills(query, max_skills=max_candidates * 2)
            retrieval_method = "keyword"
            logger.debug("Keyword fallback: %d candidates", len(candidates))
        if not candidates:
            return [], retrieval_method

        # --- Stage 2: budget-aware Tier 3 expansion ---
        budget_remaining = context_budget
        tools: list[dict[str, Any]] = []

        for skill in candidates[:max_candidates]:
            schema = self._skill_to_tool_schema(skill)
            cost = _estimate_tokens(schema)
            if budget_remaining < cost:
                logger.debug(
                    "Skipping %s (%d tokens, %d remaining)", skill.name, cost, budget_remaining,
                )
                continue
            tools.append(schema)
            budget_remaining -= cost

        logger.info(
            "Progressive disclosure: %d/%d candidates loaded, budget used %d/%d tokens",
            len(tools), min(len(candidates), max_candidates),
            context_budget - budget_remaining, context_budget,
        )
        return tools, retrieval_method

    def _skill_to_tool_schema_by_name(self, name: str) -> dict[str, Any] | None:
        """Look up a skill by exact name and return its tool schema, or None."""
        skill = self.rule_selector.skills.get(name)
        if skill is None:
            return None
        return self._skill_to_tool_schema(skill)

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
        # Step 1 & 2: Use get_tools_schema for consistent retrieval path
        tools_schema, _ = self.get_tools_schema(query, max_candidates=max_candidates)

        if not tools_schema:
            logger.info("No candidate skills found")
            return []

        logger.info(f"Retrieved {len(tools_schema)} candidate skills")

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
            logger.error(
                "Function calling failed, falling back to top-ranked candidate: %s", e,
            )
            return self._fallback_selection(tools_schema)

    def _fallback_selection(self, tools_schema: list[dict[str, Any]]) -> list[dict[str, Any]]:
        """Return top-ranked candidate when LLM function calling fails.

        Preserves the semantic/keyword ranking from retrieval stage.
        Marks arguments as pending — the caller must prompt for parameter extraction.
        """
        if not tools_schema:
            return []
        top = tools_schema[0]["function"]
        logger.warning(
            "Fallback selection: %s (from %d candidates)", top["name"], len(tools_schema),
        )
        return [{"function": {"name": top["name"], "arguments": None}, "fallback": True}]

    def _skill_to_tool_schema(self, skill: SkillMetadata) -> dict[str, Any]:
        """Convert skill metadata to OpenAI tool schema.

        Auto-generates schema from skill's input model (Pydantic).
        Framework fields (user_id, session_id, repo_id) are excluded —
        they are injected by the executor at runtime.
        """
        try:
            skill_def = self._registry.get(skill.name)

            if skill_def:
                # Get input model from __init_subclass__ auto-populated _input_cls
                input_cls = getattr(skill_def, "_input_cls", None)
                if input_cls is None:
                    # Fallback: inspect validate_input return type annotation
                    import typing
                    hints = typing.get_type_hints(skill_def.validate_input)
                    input_cls = hints.get("return")
                if input_cls and hasattr(input_cls, "model_json_schema"):
                    parameters = input_cls.model_json_schema()
                elif input_cls and hasattr(input_cls, "schema"):
                    parameters = input_cls.schema()
                else:
                    parameters = self._get_default_schema(skill.name)
                # Strip framework fields from schema
                parameters = self._strip_framework_fields(parameters)
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

    @staticmethod
    def _strip_framework_fields(schema: dict[str, Any]) -> dict[str, Any]:
        """Remove framework-injected fields and inline $defs for OpenAI compatibility."""
        # Inline $ref/$defs — OpenAI function calling doesn't support JSON Schema references
        schema = ModernSkillSelector._inline_refs(schema)
        from core.skills.base import SkillInput
        fw = SkillInput._FRAMEWORK_FIELDS
        props = schema.get("properties", {})
        for f in fw:
            props.pop(f, None)
        req = schema.get("required", [])
        if req:
            schema["required"] = [r for r in req if r not in fw]
        return schema

    @staticmethod
    def _inline_refs(schema: dict[str, Any]) -> dict[str, Any]:
        """Recursively resolve $ref pointers against $defs, producing a self-contained schema."""
        defs = schema.pop("$defs", None) or schema.pop("definitions", None)
        if not defs:
            return schema

        def _resolve(node: Any) -> Any:
            if isinstance(node, dict):
                if "$ref" in node:
                    ref_name = node["$ref"].rsplit("/", 1)[-1]
                    return _resolve(defs.get(ref_name, node))
                return {k: _resolve(v) for k, v in node.items()}
            if isinstance(node, list):
                return [_resolve(item) for item in node]
            return node

        return _resolve(schema)

    def _get_default_schema(self, skill_name: str) -> dict[str, Any]:
        """Default schema for skills without a Pydantic model."""
        return {"type": "object", "properties": {}, "required": []}


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
            raw_args = tool_call["function"]["arguments"]
            if raw_args is None:
                # Fallback selection — no arguments extracted, skip execution
                logger.warning("Skipping %s: fallback selection, no arguments", skill_name)
                continue
            arguments = json.loads(raw_args)

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
