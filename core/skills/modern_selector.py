"""Internal modern skill selector with native LLM function calling.

⚠️ DO NOT USE DIRECTLY - Use SkillPipeline instead.

This module is an internal implementation detail used by SkillPipeline.
External code should use SkillPipeline from core.skills.pipeline.
"""

import json
from typing import Any

from core.logging_config import get_logger
from core.skills.selector import SkillMetadata, SkillSelector

logger = get_logger(__name__)

_DEFAULT_CONTEXT_BUDGET = 2000  # tokens reserved for tool schemas


def _estimate_tokens(obj: Any) -> int:
    """Estimate token count from serialized JSON size (~4 chars per token)."""
    return len(json.dumps(obj, default=str)) // 4


class ModernSkillSelector:
    """Skill selector using native LLM function calling (OpenAI/Gemini/DeepSeek)."""

    def __init__(self, db_factory, llm_client=None, *, embed_fn=None):
        if not callable(db_factory):
            raise TypeError(f"db_factory must be callable, got {type(db_factory).__name__}")
        self._db_factory = db_factory
        self.llm = llm_client
        self.rule_selector = SkillSelector(db_factory)

        # Cache registry for schema lookups (avoid re-instantiation per skill)
        from core.skills.registry import SkillRegistry
        self._registry = SkillRegistry(db_factory)

        # Semantic index — primary retrieval path when available
        from core.skills.skill_index import SkillIndex
        self._index = SkillIndex(embed_fn=embed_fn, db_factory=db_factory)
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

        Stage 1 (Index Tier): Retrieve candidates via rule-based matching on
                          lightweight metadata. Zero prompt tokens.
        Stage 2 (Schema Tier): Build full schema for each candidate, measure real
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

        # --- Stage 2: budget-aware Schema Tier expansion ---
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
