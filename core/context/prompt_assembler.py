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
from dataclasses import dataclass, field
from typing import Any

from sqlalchemy import text
from sqlalchemy.exc import IntegrityError, SQLAlchemyError
from sqlalchemy.orm import Session
from core.db_consumer import DbConsumer, DbFactory

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
    ) -> AssembledPrompt:
        sections: dict[str, str] = {}
        breakdown: dict[str, int] = {}

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
        memory = self._build_memory(user_id, session_id, user_query)
        if memory:
            sections["memory"] = memory
            breakdown["memory"] = _estimate_tokens(memory)

        # §5 Working Memory (scratchpad)
        working = self._build_working_memory(session_id)
        if working:
            sections["working_memory"] = working
            breakdown["working_memory"] = _estimate_tokens(working)

        # §6 History
        history = self._build_history(session_id, max_tokens)
        if history:
            sections["history"] = history
            breakdown["history"] = _estimate_tokens(history)

        # §7 Constraints
        constraints = (
            "Rules:\n"
            "- Think step-by-step before acting\n"
            "- Verify changes before presenting\n"
            "- If uncertain, say so rather than guess\n"
            "- For questions about YOUR capabilities, answer from Self-Model — don't explore files\n"
            "\nData integrity rules:\n"
            "- NEVER fabricate data. If a skill returns success=False, report the error honestly\n"
            "- NEVER invent numbers, dates, or facts not present in skill output\n"
            "- If data is unavailable, say so explicitly\n"
            "\nTool selection rules:\n"
            "- Before using generic tools (bash, grep) for tasks that cloud skills handle (GitHub PRs, CI status), "
            "check if a specialized cloud skill is available — they are faster and more reliable\n"
            "- If you are unsure which tool to use, call decision_trace to see all available cloud skills and their parameters\n"
            "\nReflection rules:\n"
            "- When a tool result is unexpected or a skill fails, review your recent actions before retrying\n"
            "- If a reflect tool is available, use it to inspect decision history\n"
            "- After reviewing, either retry with a corrected approach or report the problem honestly\n"
            "\nFile editing rules:\n"
            "- To edit existing files, ALWAYS use str_replace — never rewrite the entire file with write_file\n"
            "- Use write_file ONLY to create new files that don't exist yet\n"
            "- For multiple changes to one file, call str_replace once per change\n"
            "- Include enough context in old_str to match exactly one location"
        )
        sections["constraints"] = constraints
        breakdown["constraints"] = _estimate_tokens(constraints)

        # Compress if over budget
        total = sum(breakdown.values())
        if total > max_tokens:
            sections, breakdown = self._compress(sections, breakdown, max_tokens)

        # Assemble in cache-friendly order (see _SECTION_ORDER)
        parts = [sections[k] for k in _SECTION_ORDER if k in sections]
        system_message = "\n\n".join(parts)

        # Cache prefix = stable sections (identity + self_model + project_context)
        stable_keys = ["identity", "self_model", "project_context"]
        cache_prefix = sum(breakdown.get(k, 0) for k in stable_keys)

        # Tools schema (edge tools passed through for now; unified catalog in Phase 4)
        tools_schema = (edge_context.edge_tools or []) if edge_context else []

        # Persist snapshot
        snapshot_id = self._save_snapshot(session_id, sections, breakdown)

        return AssembledPrompt(
            system_message=system_message,
            tools_schema=tools_schema,
            snapshot_id=snapshot_id,
            token_breakdown=breakdown,
            cache_prefix_tokens=cache_prefix,
            sections=sections,
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
    ) -> AssembledPrompt:
        """Refresh §4 (memory) and §5 (working memory) for turn 2+.

        Re-runs _build_memory() and _build_working_memory() with the latest
        query, keeps all other sections unchanged, applies budget compression,
        rebuilds the system message, and saves a new snapshot.

        tools_schema is not returned (empty list) because tool definitions
        don't change during incremental refresh — the caller already has them
        cached from the initial assemble() call.
        """
        sections = dict(current_sections)
        breakdown: dict[str, int] = {}

        # Refresh memory
        memory = self._build_memory(user_id, session_id, user_query)
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

        snapshot_id = self._save_snapshot(session_id, sections, breakdown)

        return AssembledPrompt(
            system_message=system_message,
            tools_schema=[],
            snapshot_id=snapshot_id,
            token_breakdown=breakdown,
            sections=sections,
        )

    # ------------------------------------------------------------------
    # Section builders
    # ------------------------------------------------------------------

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
            return "You are a development assistant. Use the available tools to help the user."

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
        """§2: Agent self-awareness — capabilities, boundaries, learned insights."""
        with self._db() as db:
            parts = ["## Self-Model"]
            parts.append("When users ask about YOUR skills, capabilities, or what you can do, answer from this section — do not explore the filesystem.")

            # Capabilities — list actual tool names so LLM knows exactly what it has
            parts.append("\n### My Skills & Tools")
            if edge_context and edge_context.edge_tools:
                tool_names = [t.get("function", {}).get("name", "unknown") for t in edge_context.edge_tools]
                parts.append(f"- Available tools: {', '.join(tool_names)}")
                # Also show categories for context
                categories = _categorize_tools(tool_names)
                if categories:
                    parts.append(f"- Categories: {', '.join(categories)}")
            else:
                parts.append("- Local tools: file operations, shell commands, git, search")

            # User-installed skills — personalized to the current user.
            # These are skills the user explicitly installed via `/skill install`,
            # distinct from globally active cloud skills below.
            # Capped at 10 to stay within the self-model token budget;
            # full list available via get_agent_info tool at runtime.
            installed_names: set[str] = set()
            if user_id:
                try:
                    from api.models import SkillInstallation, SkillRegistry
                    installed = (
                        db.query(SkillInstallation.skill_name, SkillInstallation.skill_version)
                        .filter(SkillInstallation.user_id == user_id, SkillInstallation.status == "installed")
                        .limit(10)
                        .all()
                    )
                    if installed:
                        installed_names = {r[0] for r in installed}
                        descs = (
                            db.query(SkillRegistry.skill_name, SkillRegistry.description)
                            .filter(
                                SkillRegistry.skill_name.in_(list(installed_names)),
                                SkillRegistry.is_active == 1,
                            )
                            .all()
                        )
                        # Deduplicate: keep first description per skill_name
                        desc_map: dict[str, str] = {}
                        for name, d in descs:
                            if d and name not in desc_map:
                                desc_map[name] = d
                        lines = []
                        for name, version in installed:
                            desc = desc_map.get(name)
                            lines.append(f"  - {name} (v{version})" + (f": {desc}" if desc else ""))
                        parts.append("- My installed skills:\n" + "\n".join(lines))
                except SQLAlchemyError:
                    pass

            # Cloud skills — globally active skills available to all users.
            # Excludes already-installed skills to avoid redundancy and save
            # token budget (Self-Model has a 600-token hard cap).
            # Capped at 10; full catalog available via get_agent_info tool.
            try:
                from api.models import SkillRegistry
                query = db.query(SkillRegistry.skill_name, SkillRegistry.description).filter(
                    SkillRegistry.is_active == 1,
                ).order_by(SkillRegistry.skill_name)
                rows = query.limit(30).all()
                if rows:
                    # Deduplicate multi-version rows and exclude installed skills.
                    seen: set[str] = set()
                    lines = []
                    for name, desc in rows:
                        if name in seen or name in installed_names:
                            continue
                        seen.add(name)
                        lines.append(f"  - {name}" + (f": {desc}" if desc else ""))
                        if len(lines) >= 10:
                            break
                    if lines:
                        parts.append("- Available cloud skills:\n" + "\n".join(lines))
            except SQLAlchemyError:
                pass

            # Delegation
            if agent_id:
                try:
                    from api.models import Agent
                    row = db.query(Agent.agent_config).filter(Agent.agent_id == agent_id).first()
                    if row and row[0]:
                        config = row[0] if isinstance(row[0], dict) else json.loads(row[0])
                        delegates = config.get("delegate_to") or config.get("allowed_delegates")
                        if delegates:
                            parts.append(f"- Can delegate to: {', '.join(delegates)}")
                except (SQLAlchemyError, json.JSONDecodeError, KeyError, TypeError):
                    pass

            # Boundaries
            parts.append("\n### Boundaries")
            parts.append("- You need user permission for: shell commands, file writes")
            parts.append("- If uncertain, say so rather than guess")

            # Learned insights (or cold start baseline)
            insight = self._get_learned_insight(agent_id, agent_type)
            parts.append(f"\n### What I've Learned\n{insight}")

            # Introspection hint
            parts.append("\nFor detailed runtime state, use the `get_agent_info` tool.")

            result = "\n".join(parts)
            # Hard cap: drop learned insights if self-model exceeds token budget
            if _estimate_tokens(result) > _FIXED_SELF_MODEL:
                # Compress: keep header + capabilities + boundaries, drop learned insights
                keep = []
                drop_after = "### What I've Learned"
                for p in parts:
                    if p.strip().startswith(drop_after):
                        break
                    keep.append(p)
                result = "\n".join(keep)
                result += "\nFor full details, use `get_agent_info`."
            return result

    def _get_learned_insight(self, agent_id: str | None, agent_type: str) -> str:
        """Load procedural memory insights, or cold start baseline.

        Gracefully handles missing skill_selection_events table (returns baseline).
        The table is created by migration and may not exist in all environments.
        """
        with self._db() as db:
            if agent_id:
                try:
                    # JSON_EXTRACT is supported by MatrixOne (MySQL-compatible syntax).
                    # Parameterized :aid prevents SQL injection; JSON path is a constant.
                    row = db.execute(
                        text("""
                            SELECT COUNT(*) as cnt FROM skill_selection_events
                            WHERE JSON_EXTRACT(metadata, '$.agent_id') = :aid
                        """),
                        {"aid": agent_id},
                    ).fetchone()
                    if row and row[0] and row[0] >= 50:
                        return self._query_procedural_insights(agent_id)
                except SQLAlchemyError:
                    # Table may not exist yet (pre-migration) — fall through to baseline
                    pass
            return _BASELINE_INSIGHTS.get(agent_type, _DEFAULT_INSIGHT)

    def _query_procedural_insights(self, agent_id: str) -> str:
        """Query actual performance data from skill selection history."""
        with self._db() as db:
            try:
                row = db.execute(
                    text("""
                        SELECT
                            COUNT(*) as total,
                            SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as successes
                        FROM skill_selection_events
                        WHERE JSON_EXTRACT(metadata, '$.agent_id') = :aid
                          AND created_at > NOW() - INTERVAL 30 DAY
                    """),
                    {"aid": agent_id},
                ).fetchone()
                if row and row[0] and row[0] > 0:
                    rate = (row[1] or 0) / row[0] * 100
                    return f"Based on recent history: {rate:.0f}% skill selection accuracy over {row[0]} interactions."
            except SQLAlchemyError:
                pass
            return _DEFAULT_INSIGHT

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

    def _build_memory(self, user_id: str, session_id: str, query: str) -> str | None:
        """§4: Tiered memory (L0 profile + L1 query-relevant) + legacy fallbacks.

        Primary: TieredMemoryLoader (new memory system)
        Fallback: continuity + observations + few-shot (legacy)
        """
        parts = []

        # Primary: tiered memory system (L0 + L1)
        try:
            from core.memory.tiered_loader import TieredMemoryLoader
            loader = TieredMemoryLoader(self._db_factory)
            tiered_section, _ = loader.build_section(user_id, session_id, query)
            if tiered_section:
                parts.append(tiered_section)
        except Exception as e:
            logger.debug("TieredMemoryLoader skipped: %s", e)

        # Few-shot examples
        try:
            from core.context.few_shot import FewShotRetriever
            fsr = FewShotRetriever(self._db_factory)
            examples = fsr.retrieve(query)
            few_shot = fsr.format_for_prompt(examples)
            if few_shot:
                parts.append(few_shot)
        except Exception as e:
            logger.debug("Few-shot skipped: %s", e)

        return "\n\n".join(parts) if parts else None

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

    def _build_history(self, session_id: str, max_tokens: int) -> str | None:
        """§6: Budget-capped conversation history."""
        with self._db() as db:
            budget_chars = int(max_tokens * _MAX_HISTORY_RATIO) * 4
            try:
                rows = db.execute(
                    # Why f-string instead of parameterized LIMIT?
                    # MySQL-compatible DBs (including MatrixOne) may quote parameterized
                    # LIMIT values as strings: `LIMIT '20'` → syntax error.
                    # SQLAlchemy's bindparam() has the same issue.
                    # _MAX_HISTORY_EVENTS is a module-level int constant (not user input),
                    # so f-string interpolation is safe from SQL injection.
                    text(f"""
                        SELECT event_type, content FROM agent_events
                        WHERE session_id = :sid AND event_type IN ('user_query', 'llm_response')
                        ORDER BY created_at DESC LIMIT {_MAX_HISTORY_EVENTS}
                    """),
                    {"sid": session_id},
                ).fetchall()
                if not rows:
                    return None
                lines = []
                used = 0
                for row in reversed(rows):
                    label = "User" if row[0] == "user_query" else "Agent"
                    content = row[1] or ""
                    if len(content) > 300:
                        content = content[:300] + "..."
                    line = f"{label}: {content}"
                    if used + len(line) > budget_chars and lines:
                        break
                    lines.append(line)
                    used += len(line)
                return "Recent conversation:\n" + "\n".join(lines) if lines else None
            except SQLAlchemyError as e:
                logger.debug("History skipped: %s", e)
                return None

    # ------------------------------------------------------------------
    # Compression
    # ------------------------------------------------------------------

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

    def _save_snapshot(self, session_id: str, sections: dict[str, str], breakdown: dict[str, int]) -> str | None:
        """Persist context snapshot for audit.

        Uses the production ctx_snapshots schema (context_capture_id PK).
        Stores assembled sections in system_prompt and token breakdown in token_budget.

        Commits immediately because callers (chat_turn, recovery) use autocommit=False
        sessions with no outer transaction — the snapshot must be durable before the
        SSE stream starts, since stream errors would lose uncommitted data.
        """
        with self._db() as db:
            try:
                from uuid_utils import uuid7
                capture_id = str(uuid7())
                db.execute(
                    text("""
                        INSERT INTO ctx_snapshots
                            (context_capture_id, session_id, event_id, system_prompt, token_budget, created_at)
                        VALUES (:cid, :sess, :eid, :prompt, :budget, NOW())
                    """),
                    {
                        "cid": capture_id,
                        "sess": session_id,
                        "eid": capture_id,  # placeholder — real event_id set by caller
                        "prompt": json.dumps({
                            "sections": {k: v[:SNAPSHOT_SECTION_CHARS] for k, v in sections.items()},
                            "token_breakdown": breakdown,
                        }),
                        "budget": json.dumps(breakdown),
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
