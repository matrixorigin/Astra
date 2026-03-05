"""Context-aware pre-filtering for skill selection.

Zero-token, deterministic pre-filtering that narrows skill candidates
before vector retrieval. Uses structured skill tags and conversation
state signals extracted from message history.

Design doc: docs/design/skills-and-tools.md §3.5, §3.6
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Protocol, runtime_checkable

from core.logging_config import get_logger

logger = get_logger(__name__)

# ── Valid tag values (registration-time validation) ──────────────

VALID_SCOPES = frozenset({
    "current_session",
    "historical",
    "cross_session",
    "external",
})

VALID_DATA_SOURCES = frozenset({
    "session_metadata",
    "event_store",
    "memory_store",
    "external_api",
})

VALID_INTENT_TYPES = frozenset({
    "analytical",
    "fetch",
    "mutate",
    "introspect",
})

# ── Default tag inference from category ──────────────────────────

_CATEGORY_TAG_DEFAULTS: dict[str, dict[str, Any]] = {
    "github": {"scope": "external", "data_source": "external_api", "intent_type": ["fetch", "mutate"], "requires_history": False},
    "jira": {"scope": "external", "data_source": "external_api", "intent_type": ["fetch", "mutate"], "requires_history": False},
    "external": {"scope": "external", "data_source": "external_api", "intent_type": ["fetch"], "requires_history": False},
    "code_execution": {"scope": "current_session", "data_source": "session_metadata", "intent_type": ["mutate"], "requires_history": False},
    "system": {"scope": "current_session", "data_source": "session_metadata", "intent_type": ["introspect"], "requires_history": False},
    "multi_agent": {"scope": "current_session", "data_source": "session_metadata", "intent_type": ["mutate"], "requires_history": False},
}


# ── SkillTags ────────────────────────────────────────────────────

@dataclass(frozen=True)
class SkillTags:
    """Structured tags for pre-filtering. Never sent to LLM."""

    scope: str
    data_source: str
    intent_type: tuple[str, ...]
    requires_history: bool

    def to_dict(self) -> dict[str, Any]:
        """Serialize to JSON-storable dict."""
        return {
            "scope": self.scope,
            "data_source": self.data_source,
            "intent_type": list(self.intent_type),
            "requires_history": self.requires_history,
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> SkillTags:
        """Deserialize from JSON dict. Raises ValueError on invalid values."""
        scope = data.get("scope", "")
        data_source = data.get("data_source", "")
        intent_type = data.get("intent_type", [])
        requires_history = bool(data.get("requires_history", False))

        if scope not in VALID_SCOPES:
            raise ValueError(f"Invalid scope: {scope!r}. Must be one of {sorted(VALID_SCOPES)}")
        if data_source not in VALID_DATA_SOURCES:
            raise ValueError(f"Invalid data_source: {data_source!r}. Must be one of {sorted(VALID_DATA_SOURCES)}")
        invalid_intents = set(intent_type) - VALID_INTENT_TYPES
        if invalid_intents:
            raise ValueError(f"Invalid intent_type: {sorted(invalid_intents)}. Must be from {sorted(VALID_INTENT_TYPES)}")

        return cls(
            scope=scope,
            data_source=data_source,
            intent_type=tuple(sorted(intent_type)),
            requires_history=requires_history,
        )

    @classmethod
    def infer_from_category(cls, category: str) -> SkillTags | None:
        """Infer default tags from skill category. Returns None if no mapping."""
        defaults = _CATEGORY_TAG_DEFAULTS.get(category)
        if defaults is None:
            return None
        return cls(
            scope=defaults["scope"],
            data_source=defaults["data_source"],
            intent_type=tuple(sorted(defaults["intent_type"])),
            requires_history=defaults["requires_history"],
        )


def validate_tags(tags_dict: dict[str, Any]) -> SkillTags:
    """Validate and parse a tags dict. Raises ValueError on invalid input."""
    return SkillTags.from_dict(tags_dict)


@runtime_checkable
class HasTags(Protocol):
    """Minimum interface for objects passed to pre_filter()."""

    name: str
    tags: SkillTags | None


# ── ConversationState ────────────────────────────────────────────

# English markers use multi-word phrases to avoid false positives.
# Single words like "get", "last", "status" are too common in general English.
# Chinese markers can be shorter because they are more semantically specific.

_HISTORY_MARKERS = frozenset({
    # Chinese: specific history references
    "前一个", "上一轮", "刚才", "之前", "上次",
    # English: multi-word to avoid matching "last PR" or "before we start"
    "previous context", "previous session", "previous turn",
    "last session", "last turn", "last conversation",
    "earlier context", "earlier session",
    "before that",
})

_ANALYTICAL_MARKERS = frozenset({
    "分析", "评估", "为什么", "怎么回事", "原因",
    "analyze", "evaluate", "why", "assess", "diagnose",
})

_FETCH_MARKERS = frozenset({
    "查看", "列出", "最新", "情况", "查询",
    "show me", "list", "latest", "get the",
})

_MUTATE_MARKERS = frozenset({
    "创建", "修改", "删除", "新建", "更新",
    "create", "update", "delete", "modify", "remove",
})


@dataclass(frozen=True)
class ConversationState:
    """Signals extracted from conversation context. Zero LLM cost."""

    references_history: bool = False
    is_analytical: bool = False
    is_fetch: bool = False
    is_mutate: bool = False
    turn_count: int = 0
    has_tool_results: bool = False
    previous_skill: str | None = None

    @classmethod
    def from_messages(cls, messages: list[dict[str, Any]]) -> ConversationState:
        """Extract signals from message history. O(n) string scan, no LLM.

        Intent signals (history/analytical/fetch/mutate) are extracted from the
        last user message only — earlier messages reflect prior intents, not
        the current request.  Structural signals (turn_count, has_tool_results,
        previous_skill) scan the full history.
        """
        if not messages:
            return cls()

        last_user_msg = ""
        for msg in reversed(messages):
            if msg.get("role") == "user":
                content = msg.get("content", "")
                last_user_msg = content.lower() if isinstance(content, str) else ""
                break

        # Scan for tool results between the last two user messages.
        # This covers multi-tool-call turns where the assistant invokes
        # several tools before the user speaks again.
        has_tool = False
        seen_last_user = False
        for msg in reversed(messages):
            role = msg.get("role")
            if role == "user":
                if seen_last_user:
                    break  # stop at the previous user message
                seen_last_user = True
                continue
            if role == "tool" and seen_last_user:
                has_tool = True
                break

        return cls(
            references_history=any(m in last_user_msg for m in _HISTORY_MARKERS),
            is_analytical=any(m in last_user_msg for m in _ANALYTICAL_MARKERS),
            is_fetch=any(m in last_user_msg for m in _FETCH_MARKERS),
            is_mutate=any(m in last_user_msg for m in _MUTATE_MARKERS),
            turn_count=sum(1 for m in messages if m.get("role") == "user"),
            has_tool_results=has_tool,
            previous_skill=cls._extract_previous_skill(messages),
        )

    @staticmethod
    def _extract_previous_skill(messages: list[dict[str, Any]]) -> str | None:
        """Find the skill used in the most recent assistant turn."""
        for msg in reversed(messages):
            if msg.get("role") == "assistant":
                tool_calls = msg.get("tool_calls")
                if tool_calls and isinstance(tool_calls, list) and len(tool_calls) > 0:
                    fn = tool_calls[0].get("function", {})
                    return fn.get("name")
        return None

    def to_dict(self) -> dict[str, Any]:
        """Serialize for audit logging."""
        return {
            "references_history": self.references_history,
            "is_analytical": self.is_analytical,
            "is_fetch": self.is_fetch,
            "is_mutate": self.is_mutate,
            "turn_count": self.turn_count,
            "has_tool_results": self.has_tool_results,
            "previous_skill": self.previous_skill,
        }


# ── Pre-filter logic ─────────────────────────────────────────────

def pre_filter(
    skills: list[HasTags],
    state: ConversationState | None,
) -> tuple[list[HasTags], bool]:
    """Narrow skill candidates based on conversation state. Zero LLM cost.

    Conservative: returns full list if no rules match.
    Never removes skills — only reorders (preferred first, deprioritized last).

    Args:
        skills: List of objects with ``name`` and ``tags`` attributes.
        state: Conversation state signals. None = no filtering.

    Returns:
        (reordered_skills, pre_filter_applied) tuple.
    """
    if not state or not skills:
        return skills, False

    # Rule 1: History reference + analytical → prefer historical scope
    if state.references_history and state.is_analytical:
        reordered = _prefer(
            skills,
            include_scopes={"historical", "cross_session"},
            deprioritize_scopes={"current_session"},
        )
        if _order_changed(skills, reordered):
            logger.info("Pre-filter: history+analytical → prefer historical scope")
            return reordered, True

    # Rule 2: Fetch intent without history reference → prefer external
    if state.is_fetch and not state.references_history:
        reordered = _prefer(
            skills,
            include_scopes={"external"},
        )
        if _order_changed(skills, reordered):
            logger.info("Pre-filter: fetch → prefer external scope")
            return reordered, True

    # Rule 3: Mutate intent → prefer mutate skills
    if state.is_mutate:
        reordered = _prefer_by_intent(skills, include_intents={"mutate"})
        if _order_changed(skills, reordered):
            logger.info("Pre-filter: mutate → prefer mutate intent")
            return reordered, True

    return skills, False


def _prefer(
    skills: list[HasTags],
    include_scopes: set[str] | None = None,
    deprioritize_scopes: set[str] | None = None,
) -> list[HasTags]:
    """Reorder skills by scope tags. Skills without tags go to normal bucket."""
    preferred: list[Any] = []
    normal: list[Any] = []
    deprioritized: list[Any] = []

    for skill in skills:
        tags: SkillTags | None = getattr(skill, "tags", None)
        if tags is None:
            normal.append(skill)
            continue

        if include_scopes and tags.scope in include_scopes:
            preferred.append(skill)
        elif deprioritize_scopes and tags.scope in deprioritize_scopes:
            deprioritized.append(skill)
        else:
            normal.append(skill)

    return preferred + normal + deprioritized


def _prefer_by_intent(
    skills: list[HasTags],
    include_intents: set[str],
) -> list[HasTags]:
    """Reorder skills by intent_type tags."""
    preferred: list[Any] = []
    normal: list[Any] = []

    for skill in skills:
        tags: SkillTags | None = getattr(skill, "tags", None)
        if tags is None:
            normal.append(skill)
            continue

        if include_intents & set(tags.intent_type):
            preferred.append(skill)
        else:
            normal.append(skill)

    return preferred + normal


def _order_changed(original: list[HasTags], reordered: list[HasTags]) -> bool:
    """Check if reordering actually changed the order."""
    if len(original) != len(reordered):
        return True
    return any(a is not b for a, b in zip(original, reordered, strict=True))
