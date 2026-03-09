"""Unified prompt assembler — single entry point for both /chat and /chat/turn.

Design doc: docs/design/prompt-lifecycle.md

Assembles the system prompt as a materialized view over distributed state:
  §1 Identity (agent profile from DB)
  §2 Self-Model (capabilities, boundaries, learned insights)
  §3 Project Context (rules + profile from edge)
  §4 Memory (tiered: L0 profile + L1 semantic/episodic)
  §5 Working Memory (scratchpad)
  §6 History (budget-capped conversation events)
  §7 Constraints (behavioral rules)
"""

from __future__ import annotations

import json
import logging
import os
import time
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from typing import Any, ClassVar

from sqlalchemy import func as sa_func
from sqlalchemy import text
from sqlalchemy.exc import IntegrityError, SQLAlchemyError

from api.models.skill import SkillSelectionEvent
from core.db_consumer import DbConsumer, DbFactory

# Context window management (feature-flagged)
try:
    from core.context.prompt_integration import integrate_compression_into_prompt
    from core.context.zone_budgets import compute_zone_budgets
    _COMPRESSION_AVAILABLE = True
except ImportError:
    _COMPRESSION_AVAILABLE = False

logger = logging.getLogger(__name__)

# Prompt injection patterns to detect in edge-contributed content.
# Each pattern is matched as a substring within a line (case-insensitive).
# Patterns like "system:" use line-start anchoring via _sanitize_edge_content
# to avoid false positives (e.g. "system: Ubuntu 22.04" in project rules).
#
# We use phrase-level patterns rather than single words to reduce false positives.
# E.g. "override" alone would block "Override default linter settings", but
# "override all" or "override instructions" are more likely injection attempts.
_INJECTION_PATTERNS_ANYWHERE = [
    "ignore previous instructions",
    "ignore all previous",
    "disregard all",
    "disregard previous",
    "forget everything",
    "new instructions:",
    "you are now a",       # "you are now a pirate" but not "you are now running on"
    "you are now in",      # "you are now in DAN mode"
    "override all",
    "override instructions",
    "bypass safety",
    "bypass restrictions",
    "jailbreak mode",
    "</system>",
    "<|im_start|>",
    "<|im_end|>",
    "[INST]",
    "[/INST]",
]
# These only match at line start (after stripping whitespace)
_INJECTION_PATTERNS_LINE_START = [
    "system:",
]

# Cold start baselines by agent type
_BASELINE_INSIGHTS: dict[str, str] = {
    "specialist": "I focus deeply on my domain but may need to delegate cross-domain questions.",
    "reviewer": "I read and analyze code but don't modify files directly.",
    "orchestrator": "I break down tasks and delegate to specialists rather than solving directly.",
}
_DEFAULT_INSIGHT = "I'm still learning about my strengths and weaknesses. I'll improve as we work together."

# Budget constants (tokens)
_FIXED_SELF_MODEL = 600
_MAX_HISTORY_RATIO = 0.50

# Edge content limits (public — tests verify truncation behavior).
# These are structural invariants, NOT deployment config:
#   - MAX_PROJECT_RULES_CHARS bounds the injection-defense scan surface
#   - MAX_PROFILE_FIELD_CHARS prevents edge fields from dominating the prompt
#   - _MAX_HISTORY_EVENTS caps DB query cost and prompt history size
# Changing these affects token budgets, security boundaries, and prompt structure.
# They require code review + test updates, not a config reload or env var override.
MAX_PROJECT_RULES_CHARS = 4000
MAX_PROFILE_FIELD_CHARS = 200
SNAPSHOT_SECTION_CHARS = 2000
_MAX_HISTORY_EVENTS = 20


# Default token budget for system prompt assembly and refresh.
# Shared by assemble() and refresh_memory() to ensure consistent budget enforcement.
_DEFAULT_MAX_TOKENS = 8000

# Canonical section ordering — shared by assemble() and refresh_memory().
# Cache-friendly: stable sections first (identity, self_model, project_context)
# so LLM providers can cache the prefix across turns.
_SECTION_ORDER = [
    "identity", "self_model", "project_context", "memory",
    "working_memory", "history", "constraints",
]


def _estimate_tokens(text: str) -> int:
    """Rough token estimate. ASCII ≈ 4 chars/token, CJK ≈ 1 char/token.

    Why not tiktoken? tiktoken adds ~100MB of tokenizer data as a dependency,
    and exact counts aren't needed here — this is for budget enforcement, not
    billing. The only requirement is: never underestimate (which would cause
    budget overruns). This formula is conservative (overestimates), so the
    worst case is slightly more aggressive compression, which is safe.

    CJK characters typically tokenize to 1-2 tokens each in GPT/Claude models.
    Using divisor 1.0 (1 token per CJK char) is conservative — it slightly
    overestimates, which is the safe direction for budget enforcement.
    A divisor >1.0 would underestimate CJK tokens and risk budget overruns.
    """
    ascii_chars = sum(1 for c in text if ord(c) < 128)
    non_ascii = len(text) - ascii_chars
    return ascii_chars // 4 + non_ascii


def _sanitize_edge_content(content: str, max_chars: int) -> str:
    """Sanitize edge-contributed content: truncate + strip injection patterns."""
    content = content[:max_chars]
    lines = content.splitlines()
    clean = []
    for line in lines:
        lower = line.lower()
        stripped_lower = lower.lstrip()
        if any(pat in lower for pat in _INJECTION_PATTERNS_ANYWHERE):
            logger.warning("Stripped suspected injection line: %s", line[:80])
            continue
        if any(stripped_lower.startswith(pat) for pat in _INJECTION_PATTERNS_LINE_START):
            logger.warning("Stripped suspected injection line: %s", line[:80])
            continue
        clean.append(line)
    return "\n".join(clean)


@dataclass
class EdgeContext:
    """Context contributed by the edge on first turn."""
    project_rules: str | None = None
    edge_tools: list[dict[str, Any]] | None = None
    edge_profile: dict[str, Any] = field(default_factory=dict)


@dataclass
class AssembledPrompt:
    """Result of prompt assembly."""
    system_message: str
    tools_schema: list[dict[str, Any]]
    snapshot_id: str | None = None
    token_breakdown: dict[str, int] = field(default_factory=dict)
    cache_prefix_tokens: int = 0
    sections: dict[str, str] = field(default_factory=dict)
    memory_stats: dict[str, Any] | None = None  # Populated when explain=True
    assembly_duration_ms: float = 0.0  # Total prompt assembly wall-clock time
    memory_duration_ms: float = 0.0  # Time spent in _build_memory
    routing_intent: str | None = None  # Intent from routing (preference/command/feedback/question)
    routing_confidence: float = 0.0  # Routing confidence score


class PromptAssembler(DbConsumer):
    """Assemble the full system prompt from distributed state.

    Single entry point for both /chat (cloud-only) and /chat/turn (edge-cloud).
    """

    def __init__(self, db_factory: DbFactory):
        super().__init__(db_factory)

    def assemble(
        self,
        agent_id: str | None,
        user_query: str,
        session_id: str,
        user_id: str,
        edge_context: EdgeContext | None = None,
        max_tokens: int = _DEFAULT_MAX_TOKENS,
        username: str | None = None,
        explain: bool = False,
        verbose: bool = False,
        routing_decision: Any | None = None,
    ) -> AssembledPrompt:
        """
        Assemble system prompt with zone-based budget tracking.

        Phase 1 Integration: Computes zone budgets based on model context size
        and tracks which zones overflow. This enables data-driven optimization.

        Args:
            explain: If True, collect memory retrieval stats in result.memory_stats.
            verbose: If True (requires explain), include content previews in stats.
            routing_decision: RoutingDecision from IntentRouter — controls which sections to build.
        """
        sections: dict[str, str] = {}
        breakdown: dict[str, int] = {}
        memory_stats: dict[str, Any] | None = None
        _t0 = time.monotonic()
        _mem_duration_ms = 0.0

        # Extract routing plan (if provided)
        _plan = routing_decision.plan if routing_decision else None
        _tier1 = routing_decision.tier1_result if routing_decision else None

        # Phase 1: Compute zone budgets based on model context size
        # This provides the foundation for measuring compression effectiveness
        zone_budgets = None
        if _COMPRESSION_AVAILABLE:
            try:
                # Get model context size from edge_context or estimate
                # NOTE: max_tokens is the BUDGET (e.g., 4000), not the MODEL SIZE (e.g., 128K)
                # We need the actual model context size to compute zone budgets correctly
                if edge_context and hasattr(edge_context, 'model_context_size'):
                    model_context_size = edge_context.model_context_size
                    logger.debug(f"Using model context size from edge_context: {model_context_size}")
                else:
                    # Estimate: budget is typically 25% of model size
                    # For GPT-4: 128K context, typical budget 32K
                    # For GPT-3.5: 16K context, typical budget 4K
                    model_context_size = max_tokens * 4
                    logger.debug(f"Estimating model context size: {model_context_size} (4x budget of {max_tokens})")

                zone_budgets = compute_zone_budgets(model_context_size)
                logger.info(f"Zone budgets computed for {model_context_size} context: "
                           f"fixed={zone_budgets.fixed}, managed={zone_budgets.managed}, elastic={zone_budgets.elastic}")
            except Exception as e:
                # Log error with full traceback for debugging
                logger.error(f"Failed to compute zone budgets: {e}", exc_info=True)
                zone_budgets = None

        # §1 Identity
        identity = self._build_identity(agent_id)
        if username:
            identity += f"\n\nCurrent user: {username}"
        sections["identity"] = identity
        breakdown["identity"] = _estimate_tokens(identity)

        # §2 Self-Model
        agent_type = self._get_agent_type(agent_id)
        self_model = self._build_self_model(agent_id, agent_type, edge_context, user_id=user_id)
        sections["self_model"] = self_model
        breakdown["self_model"] = _estimate_tokens(self_model)

        # §3 Project Context
        project_ctx = self._build_project_context(edge_context)
        if project_ctx:
            sections["project_context"] = project_ctx
            breakdown["project_context"] = _estimate_tokens(project_ctx)

        # §4 Memory (continuity + observations + few-shot)
        # Routing: skip if plan.load_memory is False, L0-only if "profile"
        _skip_memory = _plan and _plan.load_memory is False
        _profile_only = _plan and _plan.load_memory == "profile"
        _mem_t0 = time.monotonic()
        if _skip_memory:
            memory, memory_stats = None, None
        elif _tier1 and _tier1.compressed_memory:
            # Use Tier 1 pre-compressed memory
            memory = _tier1.compressed_memory
            memory_stats = {"source": "tier1_compressed"} if explain else None
        elif _profile_only:
            # L0 profile only — skip L1 semantic retrieval
            memory, memory_stats = self._build_memory_profile_only(user_id, explain=explain)
        else:
            memory, memory_stats = self._build_memory(user_id, session_id, user_query, explain=explain, verbose=verbose)
        _mem_duration_ms = (time.monotonic() - _mem_t0) * 1000
        if memory:
            sections["memory"] = memory
            breakdown["memory"] = _estimate_tokens(memory)

        # §5 Working Memory (scratchpad)
        working = self._build_working_memory(session_id)
        if working:
            sections["working_memory"] = working
            breakdown["working_memory"] = _estimate_tokens(working)

        # §6 History
        # Routing: skip if plan.load_history is False, limit if int
        _skip_history = _plan and _plan.load_history is False
        if _skip_history:
            history = None
        elif _plan and isinstance(_plan.load_history, int):
            history = self._build_history(session_id, max_tokens, max_turns=_plan.load_history)
        else:
            history = self._build_history(session_id, max_tokens)
        if history:
            sections["history"] = history
            breakdown["history"] = _estimate_tokens(history)

        # §7 Constraints - load only relevant rules based on task type
        edge_tools = (edge_context.edge_tools or []) if edge_context else []
        constraints = self._build_constraints(user_query, edge_tools)
        sections["constraints"] = constraints
        breakdown["constraints"] = _estimate_tokens(constraints)

        # Compress if over budget
        # (total computed after zone rebalance which may shrink history)

        # Phase 1: Check zone budget overflows and log
        if zone_budgets and _COMPRESSION_AVAILABLE:
            self._check_zone_overflows(breakdown, zone_budgets, session_id)
            # Proportional rebalance: if history exceeds its zone budget by >50%,
            # compress it even when total is under max_tokens.
            elastic_usage = sum(breakdown.get(s, 0) for s in ("history",))
            if elastic_usage > zone_budgets.elastic * 1.5:
                target = zone_budgets.elastic
                if "history" in sections:
                    keep_chars = target * 4
                    truncated = sections["history"][:keep_chars]
                    last_nl = truncated.rfind("\n")
                    if last_nl > 0:
                        truncated = truncated[:last_nl]
                    sections["history"] = truncated + "\n[truncated]"
                    breakdown["history"] = _estimate_tokens(sections["history"])
                    logger.info("History zone rebalanced: %d → %d tokens", elastic_usage, breakdown["history"])

        total = sum(breakdown.values())
        if total > max_tokens:
            sections, breakdown = self._compress(sections, breakdown, max_tokens)

        # Hard cap: if history alone exceeds 70% of budget, force-summarize to 25%
        _HISTORY_HARD_CAP_RATIO = 0.70
        _HISTORY_TARGET_RATIO = 0.25
        history_tokens = breakdown.get("history", 0)
        if history_tokens > max_tokens * _HISTORY_HARD_CAP_RATIO:
            target = int(max_tokens * _HISTORY_TARGET_RATIO)
            keep_chars = target * 4
            truncated = sections["history"][:keep_chars]
            last_nl = truncated.rfind("\n")
            if last_nl > 0:
                truncated = truncated[:last_nl]
            sections["history"] = truncated + "\n[compressed — older history auto-summarized]"
            breakdown["history"] = _estimate_tokens(sections["history"])
            logger.warning(
                "History hard-cap triggered: %d → %d tokens (%.0f%% of %d budget)",
                history_tokens, breakdown["history"],
                history_tokens / max_tokens * 100, max_tokens,
            )

        # Assemble in cache-friendly order (see _SECTION_ORDER)
        parts = [sections[k] for k in _SECTION_ORDER if k in sections]
        system_message = "\n\n".join(parts)

        # Cache prefix = stable sections (identity + self_model + project_context)
        stable_keys = ["identity", "self_model", "project_context"]
        cache_prefix = sum(breakdown.get(k, 0) for k in stable_keys)

        # Tools schema (edge tools passed through for now; unified catalog in Phase 4)
        tools_schema = (edge_context.edge_tools or []) if edge_context else []

        # Routing: skip tools if plan says so, or prune via Tier 1
        if _plan and not _plan.load_tools:
            tools_schema = []
        elif _tier1 and _tier1.pruned_tools is not None:
            pruned_set = set(_tier1.pruned_tools)
            tools_schema = [t for t in tools_schema if t.get("function", {}).get("name", "") in pruned_set]

        # Build snapshot_breakdown = breakdown + tool_schemas + user_query
        # so ctx_snapshots.token_budget has a complete picture of the prompt.
        # These are NOT added to token_breakdown (returned to caller) because
        # they don't participate in compression and would skew budget checks.
        snapshot_breakdown = dict(breakdown)
        if tools_schema:
            snapshot_breakdown["tool_schemas"] = _estimate_tokens(json.dumps(tools_schema))
        if user_query:
            snapshot_breakdown["user_query"] = _estimate_tokens(user_query)

        # Persist snapshot
        snapshot_id = self._save_snapshot(session_id, sections, snapshot_breakdown)

        _routing_intent = routing_decision.routing_result.intent if routing_decision else None
        _routing_conf = routing_decision.routing_result.confidence if routing_decision else 0.0

        return AssembledPrompt(
            system_message=system_message,
            tools_schema=tools_schema,
            snapshot_id=snapshot_id,
            token_breakdown=breakdown,
            cache_prefix_tokens=cache_prefix,
            sections=sections,
            memory_stats=memory_stats,
            assembly_duration_ms=(time.monotonic() - _t0) * 1000,
            memory_duration_ms=_mem_duration_ms,
            routing_intent=_routing_intent,
            routing_confidence=_routing_conf,
        )

    # ------------------------------------------------------------------
    # Incremental refresh (turn 2+)
    # ------------------------------------------------------------------

    def refresh_memory(
        self,
        session_id: str,
        user_id: str,
        user_query: str,
        current_sections: dict[str, str],
        max_tokens: int = _DEFAULT_MAX_TOKENS,
        explain: bool = False,
        verbose: bool = False,
    ) -> AssembledPrompt:
        """Refresh §4 (memory) and §5 (working memory) for turn 2+.

        Re-runs _build_memory() and _build_working_memory() with the latest
        query, keeps all other sections unchanged, applies budget compression,
        rebuilds the system message, and saves a new snapshot.

        tools_schema is not returned (empty list) because tool definitions
        don't change during incremental refresh — the caller already has them
        cached from the initial assemble() call.

        Args:
            explain: If True, collect memory retrieval stats in result.memory_stats.
            verbose: If True (requires explain), include content previews in stats.
        """
        sections = dict(current_sections)
        breakdown: dict[str, int] = {}

        # Refresh memory
        memory, memory_stats = self._build_memory(user_id, session_id, user_query, explain=explain, verbose=verbose)
        if memory:
            sections["memory"] = memory
        else:
            sections.pop("memory", None)

        # Refresh working memory (scratchpad may have changed)
        working = self._build_working_memory(session_id)
        if working:
            sections["working_memory"] = working
        else:
            sections.pop("working_memory", None)

        # Recompute breakdown
        for k, v in sections.items():
            breakdown[k] = _estimate_tokens(v)

        # Compress if over budget (memory may have grown with new observations)
        total = sum(breakdown.values())
        if total > max_tokens:
            sections, breakdown = self._compress(sections, breakdown, max_tokens)

        # Reassemble
        parts = [sections[k] for k in _SECTION_ORDER if k in sections]
        system_message = "\n\n".join(parts)

        # snapshot_breakdown includes user_query so ctx_snapshots has a more
        # complete picture.  tool_schemas is unavailable here (caller caches
        # them from the initial assemble call).
        snapshot_breakdown = dict(breakdown)
        if user_query:
            snapshot_breakdown["user_query"] = _estimate_tokens(user_query)

        snapshot_id = self._save_snapshot(session_id, sections, snapshot_breakdown)

        return AssembledPrompt(
            system_message=system_message,
            tools_schema=[],
            snapshot_id=snapshot_id,
            token_breakdown=breakdown,
            sections=sections,
            memory_stats=memory_stats,
        )

    # ------------------------------------------------------------------
    # Section builders
    # ------------------------------------------------------------------

    # Core rules always included (~200 tokens)
    _CORE_RULES = (
        "## Core Rules\n"
        "1. Think step-by-step, then act.\n"
        "2. If uncertain → say so explicitly. NEVER fabricate data.\n"
        "3. Do ONLY what the user asked. When done → STOP and report.\n"
        "4. User preference statement (e.g. 'tests need -n auto', 'I use vim', '记住…', 'remember…') "
        "→ MUST call memory_program tool to persist it. memory_program is always available — do NOT use find_skills to look for it. Do NOT just acknowledge verbally.\n"
        "5. If the answer is already in the current conversation history → answer directly. Do NOT call any tool to look it up."
    )

    # Reasoning protocol — injected after core rules
    _REASONING_PROTOCOL = (
        "\n\n## Reasoning Protocol\n"
        "Use this structure for non-trivial tasks:\n"
        "<think>\n"
        "[Goal] What does the user want? Resolve references: "
        "'this/that/之前/above' → find the concrete entity in conversation history. "
        "If ambiguous, ask before acting.\n"
        "[Plan] What steps are needed?\n"
        "[Tool] Which tool fits best? One call per intent — avoid duplicate calls.\n"
        "</think>\n"
        "Then act. After tool results:\n"
        "<reflect>\n"
        "[Result] Did it work? [Next] Continue or report?\n"
        "</reflect>\n"
        "Critical tasks (PR create/merge, CI trigger, file delete): "
        "verify intent with user before executing. "
        "If result looks wrong, re-check before reporting."
    )

    # Task-specific rule blocks
    _RULE_BLOCKS: ClassVar[dict[str, str]] = {
        "file_editing": (
            "\nFile editing rules:\n"
            "- To edit existing files, ALWAYS use str_replace — never rewrite entire file\n"
            "- Use write_file ONLY for new files\n"
            "- Include enough context in old_str to match exactly one location"
        ),
        "tool_selection": (
            "\nTool selection rules:\n"
            "- Prefer specialized cloud skills over generic tools (bash, grep)\n"
            "- If a skill needs unknown params, ask user — don't search with bash/grep"
        ),
        "reflection": (
            "\nReflection rules:\n"
            "- When tool fails, review actions before retrying\n"
            "- For WHY questions (slow, errors), use reflect with appropriate focus"
        ),
        "introspection": (
            "\nIntrospection rules:\n"
            "- For questions about YOUR capabilities, answer from Self-Model\n"
            "- 'the process' without context means YOUR recent actions, not code"
        ),
    }

    # Keywords that trigger specific rule blocks
    _RULE_TRIGGERS: ClassVar[dict[str, list[str]]] = {
        "file_editing": ["edit", "modify", "change", "update", "write", "create", "file"],
        "tool_selection": ["tool", "skill", "how to", "which"],
        "reflection": ["why", "error", "fail", "slow", "wrong"],
        "introspection": ["you", "your", "can you", "what can", "capabilities"],
    }

    def _build_constraints(
        self,
        query: str | None,
        tools_schema: list[dict[str, Any]] | None,
    ) -> str:
        """Build constraints section with only relevant rules.
        
        Core rules always included. Task-specific rules added based on:
        - Query keywords
        - Available tools (file editing rules only if file tools present)
        """
        parts = [self._CORE_RULES, self._REASONING_PROTOCOL]
        
        if not query:
            # No query context — include all rules
            parts.extend(self._RULE_BLOCKS.values())
            return "".join(parts)
        
        q_lower = query.lower()
        included: set[str] = set()
        
        # Check query keywords
        for block_name, triggers in self._RULE_TRIGGERS.items():
            if any(t in q_lower for t in triggers):
                included.add(block_name)
        
        # Check available tools
        if tools_schema:
            tool_names = {t.get("function", {}).get("name", "") for t in tools_schema}
            if tool_names & {"write_file", "str_replace", "read_file"}:
                included.add("file_editing")
        
        # Always include tool_selection if multiple tools available
        if tools_schema and len(tools_schema) > 1:
            included.add("tool_selection")
        
        # Add relevant blocks
        for block_name in included:
            if block_name in self._RULE_BLOCKS:
                parts.append(self._RULE_BLOCKS[block_name])
        
        return "".join(parts)

    def _build_identity(self, agent_id: str | None) -> str:
        """§1: Load agent system prompt from DB, fallback to default.

        Uses explicit exception list: JSONDecodeError for malformed config,
        SQLAlchemyError for DB issues, KeyError/TypeError for unexpected
        schema.  Any other exception propagates — programming errors should
        not be silently swallowed.
        """
        with self._db() as db:
            if agent_id:
                try:
                    from api.models import Agent
                    row = db.query(Agent.agent_config).filter(Agent.agent_id == agent_id).first()
                    if row and row[0]:
                        raw = row[0]
                        config = raw if isinstance(raw, dict) else json.loads(raw) if isinstance(raw, str) and raw.strip() else None
                        if config and config.get("system_prompt"):
                            return config["system_prompt"]
                except (json.JSONDecodeError, KeyError, TypeError) as e:
                    logger.warning("Failed to load agent %s (%s): %s", agent_id, type(e).__name__, e)
                except SQLAlchemyError as e:
                    logger.warning("DB error loading agent %s (%s): %s", agent_id, type(e).__name__, e)
            return "You are a development assistant. Use tools to solve tasks exactly as asked."

    def _get_agent_type(self, agent_id: str | None) -> str:
        with self._db() as db:
            if not agent_id:
                return "default"
            try:
                from api.models import Agent
                row = db.query(Agent.agent_type).filter(Agent.agent_id == agent_id).first()
                return row[0] if row and row[0] else "default"
            except SQLAlchemyError:
                return "default"

    def _build_self_model(
        self,
        agent_id: str | None,
        agent_type: str,
        edge_context: EdgeContext | None,
        user_id: str | None = None,
    ) -> str:
        """§2: Agent self-awareness — capabilities, boundaries, learned insights.

        Optimized for large skill catalogs: uses category summary instead of
        listing individual skills. Full skill list available via get_agent_info.
        """
        with self._db() as db:
            parts = ["## Self-Model"]

            # Capabilities — compact tool list
            if edge_context and edge_context.edge_tools:
                tool_names = [t.get("function", {}).get("name", "unknown") for t in edge_context.edge_tools]
                parts.append(f"Tools: {', '.join(tool_names)}")
            else:
                parts.append("Tools: file ops, shell, git, search")

            # User-installed skills — only show names
            installed_names: set[str] = set()
            if user_id:
                try:
                    from api.models import SkillInstallation
                    installed = (
                        db.query(SkillInstallation.skill_name)
                        .filter(SkillInstallation.user_id == user_id, SkillInstallation.status == "installed")
                        .limit(10)
                        .all()
                    )
                    if installed:
                        installed_names = {r[0] for r in installed}
                        parts.append(f"Installed: {', '.join(installed_names)}")
                except SQLAlchemyError:
                    pass

            # Cloud skills — single-line count + use find_skills
            skill_count = self._count_cloud_skills(db, installed_names)
            if skill_count:
                parts.append(f"+{skill_count} cloud skills (use `find_skills` or `get_agent_info` to discover)")

            # Memory hint — always present so the LLM knows it can recall user
            # context via get_agent_info, even on first interaction (~15 tokens).
            parts.append("Memory: if User Profile is shown above in context, use it directly. Otherwise use `get_agent_info(dimension='memory')` to recall what you know about the user")

            # Delegation
            if agent_id:
                try:
                    from api.models import Agent
                    row = db.query(Agent.agent_config).filter(Agent.agent_id == agent_id).first()
                    if row and row[0]:
                        config = row[0] if isinstance(row[0], dict) else json.loads(row[0])
                        delegates = config.get("delegate_to") or config.get("allowed_delegates")
                        if delegates:
                            parts.append(f"Delegates: {', '.join(delegates)}")
                except (SQLAlchemyError, json.JSONDecodeError, KeyError, TypeError):
                    pass

            # Learned insights — only if data-driven (skip cold-start filler)
            insight = self._get_learned_insight(agent_id, agent_type)
            if insight != _DEFAULT_INSIGHT:
                parts.append(insight)

            return "\n".join(parts)

    _LEARNED_INSIGHT_THRESHOLD = 50
    _INSIGHT_WINDOW_DAYS = 30

    def _get_learned_insight(self, agent_id: str | None, agent_type: str) -> str:
        """Load procedural memory insights, or cold start baseline.

        Uses (agent_id, created_at) composite index — range scan on last 30 days.
        Called once per session (turn 1 only, then cached).
        Returns data-driven insight after ≥50 interactions with execution data,
        otherwise falls back to baseline by agent type.
        """
        if not agent_id:
            return _BASELINE_INSIGHTS.get(agent_type, _DEFAULT_INSIGHT)

        try:
            # Naive UTC to match DB column (func.now() stores naive datetimes).
            cutoff = datetime.now(timezone.utc).replace(tzinfo=None) - timedelta(
                days=self._INSIGHT_WINDOW_DAYS
            )
            with self._db() as db:
                total, successes = (
                    db.query(
                        sa_func.count(SkillSelectionEvent.event_id),
                        sa_func.sum(SkillSelectionEvent.execution_success),
                    )
                    .filter(
                        SkillSelectionEvent.agent_id == agent_id,
                        SkillSelectionEvent.execution_success.isnot(None),
                        SkillSelectionEvent.created_at >= cutoff,
                    )
                    .one()
                )
                if total and total >= self._LEARNED_INSIGHT_THRESHOLD:
                    rate = (successes or 0) / total * 100
                    return (
                        f"Based on recent history: {rate:.0f}% skill selection "
                        f"accuracy over {total} interactions."
                    )
        except SQLAlchemyError:
            logger.debug("Learned insight query failed", exc_info=True)
        return _BASELINE_INSIGHTS.get(agent_type, _DEFAULT_INSIGHT)

    def _build_project_context(self, edge_context: EdgeContext | None) -> str | None:
        """§3: Project rules + edge profile."""
        if not edge_context:
            return None
        parts = []
        if edge_context.project_rules:
            sanitized = _sanitize_edge_content(edge_context.project_rules, MAX_PROJECT_RULES_CHARS)
            parts.append(f"# Project Rules\n{sanitized}")
        if edge_context.edge_profile:
            profile = edge_context.edge_profile
            info = []
            for key in ("cwd", "git_branch", "project_type"):
                val = profile.get(key)
                if val:
                    # Truncate + sanitize each field (edge-contributed, could contain injection)
                    sanitized_val = _sanitize_edge_content(str(val), MAX_PROFILE_FIELD_CHARS)
                    info.append(f"{key}: {sanitized_val}")
            if profile.get("languages"):
                langs = profile["languages"]
                if isinstance(langs, list):
                    info.append(f"languages: {', '.join(str(lang)[:20] for lang in langs[:10])}")
            if info:
                parts.append("# Project Profile\n" + "\n".join(info))
        return "\n\n".join(parts) if parts else None

    def _count_cloud_skills(self, db, exclude_names: set[str] | None = None) -> int | None:
        """Count distinct active cloud skills (O(1) query, no category breakdown)."""
        try:
            from api.models import SkillRegistry
            from sqlalchemy import func as sa_func
            query = db.query(sa_func.count(SkillRegistry.skill_name.distinct())).filter(
                SkillRegistry.is_active == 1,
            )
            if exclude_names:
                query = query.filter(~SkillRegistry.skill_name.in_(exclude_names))
            count = query.scalar()
            return count if count and count > 0 else None
        except SQLAlchemyError:
            return None

    def _build_skill_categories(self, db, exclude_names: set[str] | None = None) -> str | None:
        """Build skill category summary for Self-Model section.

        Design: O(categories) not O(skills) — scales to 1000+ skills without prompt bloat.
        
        Returns a compact summary like:
          "- 42 cloud skills in 5 categories:
             - github (15): list_prs, ci_status, get_pr
             - aws (12): ec2_status, s3_list, lambda_invoke
           - Use find_skills('task') to discover relevant skills"
        
        Args:
            db: Database session
            exclude_names: Set of skill names to exclude (e.g., user's installed skills)
        
        Returns:
            Formatted category summary string, or None if no skills exist
        """
        try:
            from api.models import SkillRegistry

            # Step 1: Count distinct skill names per category
            # (not skill_id, which would count multiple versions as separate skills)
            # Exclude user's installed skills from the count
            
            # Build base query
            query = db.query(
                SkillRegistry.category,
                SkillRegistry.skill_name,
            ).filter(SkillRegistry.is_active == 1)
            
            # Exclude installed skills from cloud skills section
            if exclude_names:
                query = query.filter(~SkillRegistry.skill_name.in_(exclude_names))
            
            # Get all distinct (category, skill_name) pairs
            all_skills = query.distinct().all()
            
            if not all_skills:
                logger.debug("No active skills found")
                return None

            # Step 2: Group by category and count
            category_counts: dict[str, int] = {}
            for cat, _ in all_skills:
                category_counts[cat or "general"] = category_counts.get(cat or "general", 0) + 1
            
            logger.debug("Category counts: %s", category_counts)
            
            # Sort by count descending, limit to top 10
            sorted_cats = sorted(category_counts.items(), key=lambda x: x[1], reverse=True)[:10]
            
            if not sorted_cats:
                logger.debug("No categories after sorting")
                return None

            # Step 3: Calculate total
            total = sum(cnt for _, cnt in sorted_cats)
            
            # Step 4: Get top 3 skill examples per category (ordered by priority)
            # Note: MatrixOne requires ORDER BY columns to be in SELECT list when using DISTINCT
            # Also exclude installed skills from examples
            category_examples: dict[str, list[str]] = {}
            for cat, _ in sorted_cats:
                query = (
                    db.query(SkillRegistry.skill_name, SkillRegistry.priority)
                    .filter(
                        SkillRegistry.is_active == 1,
                        SkillRegistry.category == cat,
                    )
                )
                # Exclude installed skills from examples too
                if exclude_names:
                    query = query.filter(~SkillRegistry.skill_name.in_(exclude_names))
                
                examples = (
                    query
                    .order_by(SkillRegistry.priority.desc())  # Top priority skills first
                    .distinct()  # Avoid duplicates from multiple versions
                    .limit(3)
                    .all()
                )
                # Extract just the skill names (priority was needed for ORDER BY)
                category_examples[cat] = [e[0] for e in examples]

            # Step 5: Format output
            lines = [f"- {total} cloud skills in {len(sorted_cats)} categories:"]
            for cat, cnt in sorted_cats:
                example_str = ", ".join(category_examples.get(cat, []))
                lines.append(f"  - {cat} ({cnt}): {example_str}")

            lines.append("- Use find_skills('task') or get_agent_info for full catalog")
            return "\n".join(lines)

        except SQLAlchemyError as e:
            logger.error("Skill categories query failed: %s", e, exc_info=True)
            return None
        except Exception as e:
            logger.error("Unexpected error in _build_skill_categories: %s", e, exc_info=True)
            return None

    def _build_memory(
        self, user_id: str, session_id: str, query: str,
        explain: bool = False, verbose: bool = False,
    ) -> tuple[str | None, dict[str, Any] | None]:
        """§4: Tiered memory (L0 profile + L1 query-relevant) + legacy fallbacks.

        Primary: TieredMemoryLoader (new memory system)
        Fallback: continuity + observations + few-shot (legacy)

        When memory exceeds 300 tokens, compresses via cheapest LLM model
        to reduce downstream token cost on the main model.

        Returns:
            (section_text, stats) — stats is None when explain=False, otherwise a dict
            with flat keys: l0, retrieval, few_shot. verbose adds content previews.
        """
        parts = []
        stats: dict[str, Any] = {} if explain else {}

        # Primary: tiered memory system (L0 + L1)
        try:
            from core.context.tiered_loader import TieredMemoryLoader
            from core.memory import create_memory_service
            svc = create_memory_service(self._db_factory)
            loader = TieredMemoryLoader(svc)
            tiered_section, tiered_stats = loader.build_section(
                user_id, session_id, query, explain=explain,
            )
            if tiered_section:
                parts.append(tiered_section)
            if explain and tiered_stats:
                from dataclasses import asdict
                # Flatten: L0 stats at top level, retrieval nested properly
                stats["l0"] = {
                    "loaded": tiered_stats.l0_loaded,
                    "tokens": tiered_stats.l0_tokens,
                    "ms": round(tiered_stats.l0_ms, 1),
                }
                stats["l1"] = {
                    "loaded": tiered_stats.l1_loaded,
                    "count": tiered_stats.l1_count,
                    "tokens": tiered_stats.l1_tokens,
                    "ms": round(tiered_stats.l1_ms, 1),
                    "error": tiered_stats.l1_error,
                }
                if tiered_stats.retrieval:
                    stats["retrieval"] = asdict(tiered_stats.retrieval)
                stats["total_ms"] = round(tiered_stats.total_ms, 1)
                # Verbose: add content previews
                if verbose:
                    l0_text = loader.load_l0(user_id)
                    stats["l0"]["preview"] = l0_text[:200] if l0_text else None
                    if tiered_stats.retrieval and tiered_stats.retrieval.final_count > 0:
                        # Re-retrieve to get content (already cached in loader)
                        memories, _ = loader.load_l1(
                            user_id, session_id, query, explain=False,
                        )
                        if memories:
                            stats["l1"]["previews"] = [
                                line.strip() for line in memories.split("\n")
                                if line.strip() and line.strip() != "Relevant Memories:"
                            ][:5]
        except Exception as e:
            logger.debug("TieredMemoryLoader skipped: %s", e)
            if explain:
                stats["error"] = str(e)

        # Few-shot examples
        try:
            from core.context.few_shot import FewShotRetriever
            fsr = FewShotRetriever(self._db_factory)
            examples = fsr.retrieve(query)
            few_shot = fsr.format_for_prompt(examples)
            if few_shot:
                parts.append(few_shot)
            if explain:
                stats["few_shot"] = {"count": len(examples)}
        except Exception as e:
            logger.debug("Few-shot skipped: %s", e)
            if explain:
                stats["few_shot"] = {"error": str(e)}

        # Return None for stats when explain=False, otherwise the populated dict
        section = "\n\n".join(parts) if parts else None

        # Compress memory via cheapest LLM when section is large enough
        # to save tokens on the expensive main model call.
        _COMPRESS_THRESHOLD = 300  # tokens; below this, overhead > savings
        if section and _estimate_tokens(section) > _COMPRESS_THRESHOLD:
            section = self._compress_memory_with_llm(section)

        return section, stats if explain else None


    _COMPRESS_PROMPT = (
        "Compress this context. Keep all facts, preferences, and key details. "
        "Remove filler and redundancy. Output ONLY the compressed text."
    )

    def _compress_memory_with_llm(self, text: str) -> str:
        """Compress memory using cheapest available model. Falls back to original on error."""
        try:
            from core.llm.client import LLMClient
            llm = LLMClient(self._db_factory)
            resp = llm.chat(
                messages=[
                    {"role": "system", "content": self._COMPRESS_PROMPT},
                    {"role": "user", "content": text},
                ],
                user_id="system",
                model="cheapest",
                temperature=0.0,
                task_hint="compression",
            )
            compressed = resp.content.strip()
            if compressed and len(compressed) < len(text):
                logger.info("Memory compressed: %d → %d chars", len(text), len(compressed))
                return compressed
        except Exception as e:
            logger.debug("Memory compression skipped: %s", e)
        return text
    def _build_memory_profile_only(
        self, user_id: str, explain: bool = False,
    ) -> tuple[str | None, dict[str, Any] | None]:
        """L0 profile memory only — used by preference intent routing."""
        stats: dict[str, Any] | None = {"source": "profile_only"} if explain else None
        try:
            from core.context.tiered_loader import TieredMemoryLoader
            from core.memory import create_memory_service
            svc = create_memory_service(self._db_factory)
            loader = TieredMemoryLoader(svc)
            l0_text = loader.load_l0(user_id)
            if l0_text:
                return l0_text, stats
        except Exception as e:
            logger.debug("Profile-only memory skipped: %s", e)
        return None, stats

    def _build_working_memory(self, session_id: str) -> str | None:
        """§5: Scratchpad notes."""
        try:
            from core.context.scratchpad import AgentScratchpad
            sp = AgentScratchpad(self._db_factory)
            notes = sp.get_active_notes(session_id)
            if notes:
                lines = [f"[{n['note_type']}] {n['content']}" for n in notes]
                return "Working memory (your active notes):\n" + "\n---\n".join(lines)
        except Exception as e:
            logger.debug("Scratchpad skipped: %s", e)
        return None

    def _build_history(self, session_id: str, max_tokens: int, max_turns: int | None = None) -> str | None:
        """§6: Budget-capped conversation history with tiered compression.

        Compression is enabled by default for token efficiency:
        - Tier 1: Last 2 turns kept in full fidelity
        - Tier 2: Older turns compressed (80 char summaries, unreferenced tool results omitted)
        - Tier 3: Synopsis for very long histories (>6 turns)

        Set ENABLE_HISTORY_COMPRESSION=false to disable.

        Args:
            max_turns: If set, limit to last N user_query events (for feedback intent).
        """
        # Default: compression enabled for token efficiency
        enable_compression = os.getenv("ENABLE_HISTORY_COMPRESSION", "true").lower() != "false"

        # Routing may request fewer turns (e.g. feedback intent → last 2)
        event_limit = min(max_turns * 2, _MAX_HISTORY_EVENTS) if max_turns else _MAX_HISTORY_EVENTS

        with self._db() as db:
            budget_chars = int(max_tokens * _MAX_HISTORY_RATIO) * 4
            try:
                rows = db.execute(
                    text(f"""
                        SELECT event_id, event_type, content, metadata FROM agent_events
                        WHERE session_id = :sid AND event_type IN ('user_query', 'llm_response', 'tool_result')
                        ORDER BY created_at DESC LIMIT {event_limit}
                    """),
                    {"sid": session_id},
                ).fetchall()
                if not rows:
                    return None

                # Use compression if enabled and available
                if enable_compression and _COMPRESSION_AVAILABLE:
                    return self._build_history_compressed(rows, budget_chars)
                else:
                    return self._build_history_simple(rows, budget_chars)

            except SQLAlchemyError as e:
                logger.debug("History skipped: %s", e)
                return None

    def _build_history_simple(self, rows, budget_chars: int) -> str | None:
        """Original simple history formatting."""
        lines = []
        used = 0
        for row in reversed(rows):
            event_type = row[1]
            if event_type not in ('user_query', 'llm_response'):
                continue
            label = "User" if event_type == "user_query" else "Agent"
            content = row[2] or ""
            if len(content) > 300:
                content = content[:300] + "..."
            line = f"{label}: {content}"
            if used + len(line) > budget_chars and lines:
                break
            lines.append(line)
            used += len(line)
        return "Recent conversation:\n" + "\n".join(lines) if lines else None

    def _build_history_compressed(self, rows, budget_chars: int) -> str | None:
        """
        Reference-aware compressed history formatting.

        Converts DB rows to history format and applies compression.
        Handles malformed data gracefully with fallback to simple format.
        """
        try:
            # Convert rows to history format
            history = []
            current_turn = {}

            for row in reversed(rows):
                # Validate row structure
                if len(row) < 4:
                    logger.warning(f"Invalid row structure: expected 4 columns, got {len(row)}")
                    continue

                event_id, event_type, content, metadata = row

                if event_type == "user_query":
                    # Start new turn
                    if current_turn:
                        history.append(current_turn)
                    current_turn = {"user_query": content or ""}

                elif event_type == "llm_response":
                    # Add response to current turn
                    current_turn["llm_response"] = content or ""

                elif event_type == "tool_result":
                    # Add tool result to current turn
                    if "tool_results" not in current_turn:
                        current_turn["tool_results"] = []

                    # Parse metadata with error handling
                    meta = {}
                    if metadata:
                        try:
                            meta = json.loads(metadata)
                            if not isinstance(meta, dict):
                                logger.warning(f"Metadata is not a dict: {type(meta)}")
                                meta = {}
                        except json.JSONDecodeError as e:
                            logger.warning(f"Failed to parse metadata JSON: {e}")
                            meta = {}

                    current_turn["tool_results"].append({
                        "event_id": event_id,
                        "tool_name": meta.get("tool_name", "unknown"),
                        "content": content or "",
                        "args": meta.get("args", {})
                    })

            # Add last turn
            if current_turn:
                history.append(current_turn)

            # If no valid history, return None
            if not history:
                return None

            # Use compression integration
            return integrate_compression_into_prompt(
                history=history,
                current_turn_response="",  # No current response in history building
                current_turn_tool_calls=[],
                elastic_budget=budget_chars // 4,  # Convert chars to tokens
                enable_compression=True
            )

        except Exception as e:
            # On any error, fall back to simple format
            logger.error(f"Compression failed, falling back to simple format: {e}")
            return self._build_history_simple(rows, budget_chars)

    # ------------------------------------------------------------------
    # Compression
    # ------------------------------------------------------------------

    def _check_zone_overflows(
        self,
        breakdown: dict[str, int],
        zone_budgets,
        session_id: str
    ) -> None:
        """
        Phase 1: Check which zones exceed their budgets and log for observability.

        This enables data-driven optimization by identifying bottlenecks.
        Maps prompt sections to zone budgets:
        - Fixed zone: identity, self_model, project_context, constraints
        - Managed zone: memory, working_memory
        - Elastic zone: history
        """
        # Map sections to zones
        fixed_sections = ["identity", "self_model", "project_context", "constraints"]
        managed_sections = ["memory", "working_memory"]
        elastic_sections = ["history"]

        # Calculate actual usage per zone
        fixed_usage = sum(breakdown.get(s, 0) for s in fixed_sections)
        managed_usage = sum(breakdown.get(s, 0) for s in managed_sections)
        elastic_usage = sum(breakdown.get(s, 0) for s in elastic_sections)

        # Check overflows and log
        overflows = []

        if fixed_usage > zone_budgets.fixed:
            overflow_pct = ((fixed_usage - zone_budgets.fixed) / zone_budgets.fixed) * 100
            overflows.append(f"fixed: {fixed_usage}/{zone_budgets.fixed} (+{overflow_pct:.1f}%)")

        if managed_usage > zone_budgets.managed:
            overflow_pct = ((managed_usage - zone_budgets.managed) / zone_budgets.managed) * 100
            overflows.append(f"managed: {managed_usage}/{zone_budgets.managed} (+{overflow_pct:.1f}%)")

        if elastic_usage > zone_budgets.elastic:
            overflow_pct = ((elastic_usage - zone_budgets.elastic) / zone_budgets.elastic) * 100
            overflows.append(f"elastic: {elastic_usage}/{zone_budgets.elastic} (+{overflow_pct:.1f}%)")

        if overflows:
            logger.warning(
                f"Zone budget overflows in session {session_id}: {', '.join(overflows)}. "
                f"Total: {fixed_usage + managed_usage + elastic_usage}/{zone_budgets.model_context_size}"
            )
        else:
            logger.debug(
                f"All zones within budget for session {session_id}. "
                f"Fixed: {fixed_usage}/{zone_budgets.fixed}, "
                f"Managed: {managed_usage}/{zone_budgets.managed}, "
                f"Elastic: {elastic_usage}/{zone_budgets.elastic}"
            )

    def _compress(
        self,
        sections: dict[str, str],
        breakdown: dict[str, int],
        max_tokens: int,
    ) -> tuple[dict[str, str], dict[str, int]]:
        """Compress sections to fit budget. Priority: drop least important first."""
        # Compression order (first dropped = least important)
        compress_order = [
            "memory",          # reduce few-shot first (within memory)
            "history",         # truncate old turns
            "working_memory",  # keep only active plan
            "self_model",      # drop "What I've Learned" (available via get_agent_info)
        ]
        # Never compress: identity, constraints

        for key in compress_order:
            total = sum(breakdown.values())
            if total <= max_tokens:
                break
            if key in sections:
                overshoot = total - max_tokens
                current = breakdown[key]
                if overshoot >= current:
                    # Drop entirely
                    del sections[key]
                    del breakdown[key]
                else:
                    # Truncate at last newline boundary to avoid breaking mid-word/mid-UTF8
                    keep_chars = (current - overshoot) * 4
                    truncated = sections[key][:keep_chars]
                    last_nl = truncated.rfind("\n")
                    if last_nl > 0:
                        truncated = truncated[:last_nl]
                    sections[key] = truncated + "\n[truncated]"
                    breakdown[key] = _estimate_tokens(sections[key])

        return sections, breakdown

    # ------------------------------------------------------------------
    # Snapshot
    # ------------------------------------------------------------------

    # Fixed sections: stable within a session, stored by hash for deduplication.
    # These rarely change between turns, so storing once and referencing by hash
    # reduces storage by ~80% for long sessions.
    _FIXED_SECTIONS: ClassVar[set[str]] = {"identity", "self_model", "project_context", "constraints"}

    # Variable sections: change every turn, stored inline in snapshot.
    # These must be stored per-snapshot since they differ each turn.
    _VARIABLE_SECTIONS: ClassVar[set[str]] = {"memory", "working_memory", "history"}

    def _save_snapshot(self, session_id: str, sections: dict[str, str], breakdown: dict[str, int]) -> str | None:
        """Persist context snapshot with content-addressed deduplication.

        Fixed sections (identity, self_model, constraints) are stored once in
        ctx_prompt_fragments and referenced by hash. Variable sections (memory,
        history) are stored inline in the snapshot.

        This reduces storage by ~80% for long sessions where fixed content repeats.
        """
        import hashlib

        with self._db() as db:
            try:
                from uuid_utils import uuid7

                capture_id = str(uuid7())

                # 1. Store fixed sections by hash (deduplicated)
                fixed_hashes: dict[str, str] = {}
                for key in self._FIXED_SECTIONS:
                    if key not in sections:
                        continue
                    content = sections[key][:SNAPSHOT_SECTION_CHARS]
                    hash_val = hashlib.sha256(content.encode()).hexdigest()
                    fixed_hashes[key] = hash_val

                    # Upsert fragment (INSERT IGNORE for race-condition safety)
                    db.execute(
                        text("""
                            INSERT IGNORE INTO ctx_prompt_fragments
                            (fragment_hash, content, token_count, fragment_type)
                            VALUES (:hash, :content, :tokens, :ftype)
                        """),
                        {"hash": hash_val, "content": content,
                         "tokens": breakdown.get(key, 0), "ftype": key}
                    )

                # 2. Store variable sections inline
                variable_sections = {
                    k: v[:SNAPSHOT_SECTION_CHARS]
                    for k, v in sections.items()
                    if k in self._VARIABLE_SECTIONS
                }

                # 3. Insert snapshot with hash references
                db.execute(
                    text("""
                        INSERT INTO ctx_snapshots
                            (context_capture_id, session_id, event_id, system_prompt, token_budget, total_tokens, created_at)
                        VALUES (:cid, :sess, :eid, :prompt, :budget, :total, NOW())
                    """),
                    {
                        "cid": capture_id,
                        "sess": session_id,
                        "eid": capture_id,
                        "prompt": json.dumps({
                            "fixed_hashes": fixed_hashes,
                            "variable_sections": variable_sections,
                            "token_breakdown": breakdown,
                        }),
                        "budget": json.dumps(breakdown),
                        "total": sum(breakdown.values()),
                    },
                )
                db.commit()
                return capture_id
            except IntegrityError:
                db.rollback()
                logger.debug("Snapshot save skipped (duplicate key)")
                return None
            except (SQLAlchemyError, ImportError) as e:
                db.rollback()
                logger.warning("Snapshot save failed (%s): %s", type(e).__name__, e)
                return None


# ------------------------------------------------------------------
# Helpers
# ------------------------------------------------------------------

def _categorize_tools(tool_names: list[str]) -> list[str]:
    """Group tool names into human-readable categories for the Self-Model section.

    Uses exact name matching to avoid false positives (e.g. "read_file_metadata"
    won't match "read_file"). Unknown tools pass through as-is so the LLM still
    knows about them. Meta-tools (get_agent_info) are excluded because they are
    self-referential and don't represent user-facing capabilities.

    The mapping is hardcoded here rather than in tool definitions because:
    1. Categories are a prompt-assembly concern, not a tool concern
    2. Tools shouldn't need to know how they're presented to the LLM
    3. New tools are rare; unknown tools pass through with their raw name
    """
    # Exact name → category mapping
    _TOOL_CATEGORIES: dict[str, str] = {
        "read_file": "file operations", "write_file": "file operations",
        "list_dir": "file operations", "str_replace": "file operations",
        "bash": "shell commands", "shell": "shell commands",
        "git_status": "git operations", "git_diff": "git operations",
        "git_log": "git operations", "git_commit": "git operations",
        "grep": "code search", "glob": "code search",
        "search": "code search", "find": "code search",
    }
    _META_TOOLS = {"get_agent_info"}
    categories: set[str] = set()
    for name in tool_names:
        if name in _META_TOOLS:
            continue
        cat = _TOOL_CATEGORIES.get(name)
        if cat:
            categories.add(cat)
        else:
            categories.add(name)
    return sorted(categories)
