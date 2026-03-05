"""Unified Tool Registry — single source of truth for all tools available to the LLM.

All tool sources (edge tools, local skills, cloud skills, MCP tools) register here.
Each tool is either "pinned" (always sent to LLM) or "dynamic" (selected per-request
via embedding retrieval).

Per-request flow:
    registry.select(user_query, messages) → list[tool_schema]
        1. All pinned tools (always included)
        2. IntentRouter + Prefilter (zero cost reorder/filter)
        3. Embedding retrieval top-K from dynamic pool
        4. Merge + dedup → final tool list for LLM
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Callable

from core.logging_config import get_logger

logger = get_logger(__name__)


class ToolSource(str, Enum):
    """Where a tool comes from."""
    EDGE = "edge"          # CLI-side tools (file_ops, bash, grep...)
    LOCAL = "local"        # User-defined .mo-agent/skills/
    CLOUD = "cloud"        # Server-side builtin/marketplace skills
    MCP = "mcp"            # External MCP server tools


@dataclass(frozen=True)
class ToolEntry:
    """A registered tool with metadata for selection."""
    name: str
    description: str
    schema: dict[str, Any]          # OpenAI function calling schema
    source: ToolSource
    pinned: bool = False            # Always include in LLM context
    category: str = ""              # For tag-based filtering
    execute_fn: Any = None          # Callable for server-side execution (cloud/local)

    @property
    def schema_tokens(self) -> int:
        """Rough token estimate for this tool's schema (~4 chars/token)."""
        import json
        try:
            return len(json.dumps(self.schema)) // 4
        except (TypeError, ValueError):
            return 100  # fallback estimate


# Default pinned tools — these are needed in almost every coding interaction.
# Everything else is selected dynamically per request.
_DEFAULT_PINNED = frozenset({
    "read_file", "write_file", "str_replace", "list_dir",
    "grep", "glob", "bash",
})

# Max dynamic tools to retrieve per request
_MAX_DYNAMIC_TOOLS = 8

# Token budget for all tool schemas combined
_MAX_TOOL_TOKENS = 2500


def _default_tags_for_source(source: ToolSource):
    """Fallback SkillTags when a tool has no category. Ensures edge tools
    get scope='local' so the prefilter can deprioritize them for fetch queries."""
    from core.skills.prefilter import SkillTags
    if source in (ToolSource.CLOUD, ToolSource.MCP):
        return SkillTags(scope="external", data_source="external_api",
                         intent_type=("fetch",), requires_history=False)
    return SkillTags(scope="local", data_source="local_filesystem",
                     intent_type=("fetch",), requires_history=False)


class ToolRegistry:
    """Unified registry for all tools. Handles selection per request.

    Usage:
        registry = ToolRegistry()
        registry.register(tool_entry)
        schemas = registry.select(user_query, messages)
    """

    def __init__(
        self,
        pinned_names: frozenset[str] | None = None,
        max_dynamic: int = _MAX_DYNAMIC_TOOLS,
        max_tokens: int = _MAX_TOOL_TOKENS,
        embed_fn: Callable[[str], list[float]] | None = None,
    ):
        self._tools: dict[str, ToolEntry] = {}
        self._pinned_names = pinned_names or _DEFAULT_PINNED
        self._max_dynamic = max_dynamic
        self._max_tokens = max_tokens
        self._embed_fn = embed_fn
        # Cache embeddings for dynamic tools
        self._embeddings: dict[str, list[float]] = {}
        self._embeddings_dirty = True

    # ── Registration ─────────────────────────────────────────────

    def register(self, entry: ToolEntry) -> None:
        """Register a tool. Overwrites if name already exists."""
        self._tools[entry.name] = entry
        self._embeddings_dirty = True

    def register_skill(
        self,
        skill: Any,
        source: ToolSource,
        pinned: bool | None = None,
        category: str = "",
    ) -> None:
        """Register a Skill instance (has to_openai_schema())."""
        schema = skill.to_openai_schema()
        name = skill.name
        if pinned is None:
            pinned = name in self._pinned_names
        self._tools[name] = ToolEntry(
            name=name,
            description=skill.description or "",
            schema=schema,
            source=source,
            pinned=pinned,
            category=category,
            execute_fn=skill,
        )
        self._embeddings_dirty = True

    def register_schema(
        self,
        schema: dict[str, Any],
        source: ToolSource,
        pinned: bool | None = None,
        category: str = "",
    ) -> None:
        """Register a raw OpenAI tool schema dict."""
        fn = schema.get("function", {})
        name = fn.get("name", "")
        if not name:
            return
        if pinned is None:
            pinned = name in self._pinned_names
        self._tools[name] = ToolEntry(
            name=name,
            description=fn.get("description", ""),
            schema=schema,
            source=source,
            pinned=pinned,
            category=category,
        )
        self._embeddings_dirty = True

    def unregister(self, name: str) -> None:
        """Remove a tool by name."""
        self._tools.pop(name, None)
        self._embeddings.pop(name, None)

    def clear(self, source: ToolSource | None = None) -> None:
        """Remove all tools, or all tools from a specific source."""
        if source is None:
            self._tools.clear()
            self._embeddings.clear()
        else:
            to_remove = [n for n, t in self._tools.items() if t.source == source]
            for n in to_remove:
                self._tools.pop(n, None)
                self._embeddings.pop(n, None)
        self._embeddings_dirty = True

    # ── Query ────────────────────────────────────────────────────

    def get(self, name: str) -> ToolEntry | None:
        return self._tools.get(name)

    def all_tools(self) -> list[ToolEntry]:
        return list(self._tools.values())

    def pinned_tools(self) -> list[ToolEntry]:
        return [t for t in self._tools.values() if t.pinned]

    def dynamic_tools(self) -> list[ToolEntry]:
        return [t for t in self._tools.values() if not t.pinned]

    @property
    def size(self) -> int:
        return len(self._tools)

    # ── Selection (per-request) ──────────────────────────────────

    def select(
        self,
        user_query: str = "",
        messages: list[dict[str, Any]] | None = None,
    ) -> list[dict[str, Any]]:
        """Select tools for one LLM request. Returns list of OpenAI tool schemas.

        1. All pinned tools (always)
        2. Intent + prefilter on dynamic pool (zero cost)
        3. Embedding retrieval top-K from dynamic pool
        4. Token budget enforcement
        """
        pinned = self.pinned_tools()
        dynamic_pool = self.dynamic_tools()

        if not dynamic_pool:
            return [t.schema for t in pinned]

        # Step 1: Intent-based filtering
        dynamic_pool = self._intent_filter(user_query, dynamic_pool)

        # Step 2: Prefilter reorder by conversation state
        if messages:
            dynamic_pool = self._prefilter(messages, dynamic_pool)

        # Step 3: Embedding retrieval or truncate to max_dynamic
        if user_query and self._embed_fn and len(dynamic_pool) > self._max_dynamic:
            dynamic_pool = self._embedding_select(user_query, dynamic_pool)
        else:
            dynamic_pool = dynamic_pool[:self._max_dynamic]

        # Step 4: Merge and enforce token budget
        selected = pinned + dynamic_pool
        return self._enforce_budget(selected)

    def get_all_schemas(self) -> list[dict[str, Any]]:
        """Return all tool schemas (no filtering). For backward compat."""
        return [t.schema for t in self._tools.values()]

    # ── Internal selection stages ────────────────────────────────

    def _intent_filter(
        self, user_query: str, pool: list[ToolEntry],
    ) -> list[ToolEntry]:
        """Filter dynamic pool by intent classification. Zero cost."""
        if not user_query:
            return pool
        try:
            from core.skills.intent_router import classify_intent
            result = classify_intent(user_query)
            if result.intent == "CONVERSATIONAL":
                return []  # No dynamic tools needed for chitchat
            # EXTERNAL_FETCH and DEFAULT: keep all dynamic tools
            # (prefilter will reorder them)
        except Exception as e:
            logger.debug("Intent filter skipped: %s", e)
        return pool

    def _prefilter(
        self, messages: list[dict[str, Any]], pool: list[ToolEntry],
    ) -> list[ToolEntry]:
        """Reorder dynamic pool by conversation state signals. Zero cost."""
        try:
            from core.skills.prefilter import (
                ConversationState,
                SkillTags,
                ToolWrapper,
                pre_filter,
            )
            state = ConversationState.from_messages(messages)
            wrappers = [
                ToolWrapper(
                    name=t.name,
                    tags=(SkillTags.infer_from_category(t.category) if t.category
                          else _default_tags_for_source(t.source)),
                    schema=t.schema,
                )
                for t in pool
            ]
            reordered, applied = pre_filter(wrappers, state)
            if applied:
                # Map back to ToolEntry preserving order
                name_to_entry = {t.name: t for t in pool}
                return [name_to_entry[w.name] for w in reordered if w.name in name_to_entry]
        except Exception as e:
            logger.debug("Prefilter skipped: %s", e)
        return pool

    def _embedding_select(
        self, user_query: str, pool: list[ToolEntry],
    ) -> list[ToolEntry]:
        """Select top-K from pool using embedding similarity."""
        try:
            q_vec = self._embed_fn(user_query)
            self._ensure_embeddings(pool)

            # Compute cosine similarity
            scored: list[tuple[ToolEntry, float]] = []
            for entry in pool:
                vec = self._embeddings.get(entry.name)
                if vec is None:
                    scored.append((entry, 0.0))
                    continue
                sim = _cosine_similarity(q_vec, vec)
                scored.append((entry, sim))

            scored.sort(key=lambda x: x[1], reverse=True)
            return [e for e, _ in scored[:self._max_dynamic]]
        except Exception as e:
            logger.debug("Embedding select failed, using truncation: %s", e)
            return pool[:self._max_dynamic]

    def _ensure_embeddings(self, pool: list[ToolEntry]) -> None:
        """Compute embeddings for tools that don't have one cached."""
        if not self._embed_fn:
            return
        for entry in pool:
            if entry.name not in self._embeddings:
                try:
                    text = f"{entry.name}: {entry.description}"
                    self._embeddings[entry.name] = self._embed_fn(text)
                except Exception as e:
                    logger.debug("Embedding failed for %s: %s", entry.name, e)

    def _enforce_budget(self, selected: list[ToolEntry]) -> list[dict[str, Any]]:
        """Enforce token budget. Pinned tools are never dropped."""
        schemas: list[dict[str, Any]] = []
        total_tokens = 0
        for entry in selected:
            tokens = entry.schema_tokens
            if entry.pinned:
                # Pinned always included
                schemas.append(entry.schema)
                total_tokens += tokens
            elif total_tokens + tokens <= self._max_tokens:
                schemas.append(entry.schema)
                total_tokens += tokens
            else:
                logger.debug(
                    "Token budget exceeded (%d/%d), dropping %s",
                    total_tokens, self._max_tokens, entry.name,
                )
                break
        return schemas


def _cosine_similarity(a: list[float], b: list[float]) -> float:
    """Compute cosine similarity between two vectors."""
    if len(a) != len(b):
        return 0.0
    dot = sum(x * y for x, y in zip(a, b))
    norm_a = sum(x * x for x in a) ** 0.5
    norm_b = sum(x * x for x in b) ** 0.5
    if norm_a == 0 or norm_b == 0:
        return 0.0
    return dot / (norm_a * norm_b)
