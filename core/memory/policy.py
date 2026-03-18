"""Memory policy — first-class routing for memory context and tool usage."""

from __future__ import annotations

import re
from dataclasses import dataclass
from enum import Enum


class MemoryContextMode(str, Enum):
    """How memory should be loaded into prompt context."""

    NONE = "none"
    PROFILE_ONLY = "profile_only"
    RETRIEVE = "retrieve"
    SEARCH = "search"


@dataclass(frozen=True)
class MemoryContextPlan:
    """Plan for loading memory as external context."""

    mode: MemoryContextMode
    query: str = ""
    top_k: int = 5
    include_profile: bool = True
    memory_types: tuple[str, ...] = ("semantic", "procedural", "episodic")
    source: str = "default"
    reason: str = ""

    def as_dict(self) -> dict[str, object]:
        return {
            "mode": self.mode.value,
            "query": self.query,
            "top_k": self.top_k,
            "include_profile": self.include_profile,
            "memory_types": list(self.memory_types),
            "source": self.source,
            "reason": self.reason,
        }


@dataclass(frozen=True)
class MemoryToolHint:
    """Recommended memory operation for this turn."""

    tool_name: str | None = None
    confidence: float = 0.0
    reason: str = ""
    memory_type: str | None = None

    def as_dict(self) -> dict[str, object]:
        return {
            "tool_name": self.tool_name,
            "confidence": self.confidence,
            "reason": self.reason,
            "memory_type": self.memory_type,
        }


@dataclass(frozen=True)
class MemoryPolicyDecision:
    """Combined decision for memory-as-capability and memory-as-context."""

    context_plan: MemoryContextPlan
    tool_hint: MemoryToolHint

    def as_dict(self) -> dict[str, object]:
        return {
            "context_plan": self.context_plan.as_dict(),
            "tool_hint": self.tool_hint.as_dict(),
        }


MEMORY_TOOL_NAMES = frozenset(
    {
        "memory_retrieve",
        "memory_search",
        "memory_profile",
        "memory_store",
        "memory_correct",
        "memory_purge",
    }
)

_MEMORY_TOOL_COMPATIBILITY: dict[str, frozenset[str]] = {
    "memory_profile": frozenset({"memory_profile"}),
    "memory_retrieve": frozenset({"memory_retrieve", "memory_search"}),
    "memory_search": frozenset({"memory_search", "memory_retrieve"}),
    "memory_store": frozenset({"memory_store"}),
    "memory_correct": frozenset({"memory_correct", "memory_search", "memory_retrieve"}),
    "memory_purge": frozenset({"memory_purge", "memory_search", "memory_retrieve"}),
}


@dataclass(frozen=True)
class MemoryExecutionGuardDecision:
    """Execution-time compatibility check between intent-derived hint and tool call."""

    allow: bool
    preferred_tool: str | None = None
    actual_tool: str | None = None
    outcome: str = "allow"
    reason: str = ""
    confidence: float = 0.0
    memory_type: str | None = None

    def as_dict(self) -> dict[str, object]:
        return {
            "allow": self.allow,
            "preferred_tool": self.preferred_tool,
            "actual_tool": self.actual_tool,
            "outcome": self.outcome,
            "reason": self.reason,
            "confidence": self.confidence,
            "memory_type": self.memory_type,
        }


_PROFILE_PATTERNS = [
    r"\bwhat do you know about me\b",
    r"\bwhat do you remember about me\b",
    r"\bmy preferences\b",
    r"\bmy profile\b",
    r"你了解我什么",
    r"你记得我什么",
    r"我的偏好",
    r"关于我的记忆",
]

_BROWSE_PATTERNS = [
    r"\bwhat do you know about\b",
    r"\bshow memories about\b",
    r"\bsearch memories\b",
    r"\bbrowse memories\b",
    r"\bfind memories about\b",
    r"关于.+你知道什么",
    r"搜索记忆",
    r"查一下记忆",
]

_RECALL_PATTERNS = [
    r"\bwhat did i say about\b",
    r"\bdid we discuss\b",
    r"\bdo you remember\b",
    r"\bprevious decision\b",
    r"\brecall\b",
    r"之前说过",
    r"记得.+吗",
    r"上次.*说",
]

_STORE_PATTERNS = [
    r"\bremember that\b",
    r"\bremember i\b",
    r"\bremember my\b",
    r"\bnote that\b",
    r"记住",
    r"记一下",
    r"以后都",
]

_CORRECT_PATTERNS = [
    r"\bactually\b",
    r"\bcorrection\b",
    r"\bupdate that\b",
    r"\bthat is wrong\b",
    r"更正",
    r"改成",
    r"不是.+是",
    r"更新记忆",
]

_PURGE_PATTERNS = [
    r"\bforget that\b",
    r"\bforget what\b",
    r"\bdelete that memory\b",
    r"\berase memory\b",
    r"忘掉",
    r"忘记这个",
    r"删除记忆",
]

_PROFILE_MEMORY_PATTERNS = [
    r"\bprefer\b",
    r"\bmy preference\b",
    r"\bi use\b",
    r"\bi usually\b",
    r"\bdefault to\b",
    r"我喜欢",
    r"我用",
    r"默认",
]

_PROCEDURAL_MEMORY_PATTERNS = [
    r"\bhow to\b",
    r"\brun with\b",
    r"\bworkflow\b",
    r"\bcommand is\b",
    r"步骤",
    r"流程",
    r"命令是",
]

_QUERY_PREFIXES = [
    r"^\s*what do you know about\s+",
    r"^\s*what do you remember about\s+",
    r"^\s*show memories about\s+",
    r"^\s*search memories for\s+",
    r"^\s*find memories about\s+",
    r"^\s*what did i say about\s+",
    r"^\s*do you remember\s+",
]


def _matches_any(query: str, patterns: list[str]) -> bool:
    return any(re.search(pattern, query, flags=re.IGNORECASE) for pattern in patterns)


def _semantic_query(query: str) -> str:
    cleaned = query.strip()
    for pattern in _QUERY_PREFIXES:
        cleaned = re.sub(pattern, "", cleaned, flags=re.IGNORECASE)
    cleaned = re.sub(r"[?？!！]+$", "", cleaned).strip()
    return cleaned or query.strip()


def is_memory_tool(tool_name: str | None) -> bool:
    return bool(tool_name) and tool_name in MEMORY_TOOL_NAMES


def evaluate_memory_tool_call(
    actual_tool: str | None,
    tool_hint: MemoryToolHint | None,
    available_tools: set[str] | None = None,
    min_confidence: float = 0.9,
) -> MemoryExecutionGuardDecision:
    """Check whether a tool call is compatible with the current memory intent."""

    preferred_tool = tool_hint.tool_name if tool_hint else None
    actual_tool = actual_tool or ""

    if not preferred_tool:
        return MemoryExecutionGuardDecision(
            allow=True,
            actual_tool=actual_tool,
            outcome="no_hint",
        )

    if tool_hint.confidence < min_confidence:
        return MemoryExecutionGuardDecision(
            allow=True,
            preferred_tool=preferred_tool,
            actual_tool=actual_tool,
            outcome="weak_hint",
            confidence=tool_hint.confidence,
            memory_type=tool_hint.memory_type,
        )

    if available_tools is not None and preferred_tool not in available_tools:
        return MemoryExecutionGuardDecision(
            allow=True,
            preferred_tool=preferred_tool,
            actual_tool=actual_tool,
            outcome="preferred_unavailable",
            reason="Preferred memory tool is not available in the current tool set",
            confidence=tool_hint.confidence,
            memory_type=tool_hint.memory_type,
        )

    compatible = _MEMORY_TOOL_COMPATIBILITY.get(preferred_tool, frozenset({preferred_tool}))
    if actual_tool == preferred_tool:
        return MemoryExecutionGuardDecision(
            allow=True,
            preferred_tool=preferred_tool,
            actual_tool=actual_tool,
            outcome="preferred_match",
            reason=tool_hint.reason,
            confidence=tool_hint.confidence,
            memory_type=tool_hint.memory_type,
        )

    if actual_tool in compatible:
        return MemoryExecutionGuardDecision(
            allow=True,
            preferred_tool=preferred_tool,
            actual_tool=actual_tool,
            outcome="compatible_memory_tool",
            reason=tool_hint.reason,
            confidence=tool_hint.confidence,
            memory_type=tool_hint.memory_type,
        )

    if is_memory_tool(actual_tool):
        return MemoryExecutionGuardDecision(
            allow=False,
            preferred_tool=preferred_tool,
            actual_tool=actual_tool,
            outcome="incompatible_memory_tool",
            reason=(
                f"Memory intent expects `{preferred_tool}`, but model selected "
                f"incompatible memory tool `{actual_tool}`"
            ),
            confidence=tool_hint.confidence,
            memory_type=tool_hint.memory_type,
        )

    return MemoryExecutionGuardDecision(
        allow=False,
        preferred_tool=preferred_tool,
        actual_tool=actual_tool,
        outcome="non_memory_tool",
        reason=(
            f"Memory intent expects `{preferred_tool}`, but model selected "
            f"non-memory tool `{actual_tool}`"
        ),
        confidence=tool_hint.confidence,
        memory_type=tool_hint.memory_type,
    )


def build_memory_guard_payload(decision: MemoryExecutionGuardDecision) -> dict[str, object]:
    """Structured tool result payload for execution-stage memory guardrails."""

    preferred = decision.preferred_tool or "memory_retrieve"
    actual = decision.actual_tool or "unknown_tool"
    guidance = (
        f"This request is classified as a memory operation. "
        f"Use `{preferred}` instead of `{actual}`."
    )
    if decision.memory_type and preferred == "memory_store":
        guidance += f" Store it as `{decision.memory_type}` memory."

    payload: dict[str, object] = {
        "success": False,
        "error": "Blocked by memory policy",
        "blocked_by": "memory_policy",
        "expected_tool": preferred,
        "actual_tool": actual,
        "outcome": decision.outcome,
        "reason": decision.reason,
        "guidance": guidance,
        "user_message": (
            "I need to use the dedicated memory interface for this request "
            "before I continue."
        ),
    }
    if decision.memory_type:
        payload["suggested_memory_type"] = decision.memory_type
    return payload


class MemoryPolicy:
    """Memory policy used by runtime and prompt assembly."""

    def decide(self, query: str, load_memory: bool | str | None = True) -> MemoryPolicyDecision:
        q = (query or "").strip()
        q_lower = q.lower()

        tool_hint = self._select_tool_hint(q, q_lower)
        context_plan = self._build_context_plan(q, q_lower, load_memory)
        return MemoryPolicyDecision(context_plan=context_plan, tool_hint=tool_hint)

    def _select_tool_hint(self, query: str, q_lower: str) -> MemoryToolHint:
        if _matches_any(query, _PURGE_PATTERNS):
            return MemoryToolHint("memory_purge", 0.98, "User explicitly wants to forget stored memory")
        if _matches_any(query, _CORRECT_PATTERNS):
            return MemoryToolHint("memory_correct", 0.96, "User is correcting stored memory")
        if _matches_any(query, _STORE_PATTERNS):
            return MemoryToolHint(
                "memory_store",
                0.94,
                "User is asking to persist durable information",
                memory_type=self._infer_memory_type(q_lower),
            )
        if _matches_any(query, _PROFILE_PATTERNS):
            return MemoryToolHint("memory_profile", 0.95, "User asked about their standing profile")
        if _matches_any(query, _BROWSE_PATTERNS):
            return MemoryToolHint("memory_search", 0.9, "User wants broad memory browsing")
        if _matches_any(query, _RECALL_PATTERNS):
            return MemoryToolHint("memory_retrieve", 0.9, "User wants relevant recall for a prior topic")
        return MemoryToolHint()

    def _build_context_plan(
        self,
        query: str,
        q_lower: str,
        load_memory: bool | str | None,
    ) -> MemoryContextPlan:
        semantic_query = _semantic_query(query)

        if _matches_any(query, _PROFILE_PATTERNS):
            return MemoryContextPlan(
                mode=MemoryContextMode.PROFILE_ONLY,
                query=semantic_query,
                include_profile=True,
                source="memory_policy",
                reason="Profile-style memory question",
            )

        if _matches_any(query, _BROWSE_PATTERNS):
            return MemoryContextPlan(
                mode=MemoryContextMode.SEARCH,
                query=semantic_query,
                top_k=8,
                include_profile=False,
                source="memory_policy",
                reason="Broad memory browsing request",
            )

        if _matches_any(query, _RECALL_PATTERNS):
            return MemoryContextPlan(
                mode=MemoryContextMode.RETRIEVE,
                query=semantic_query,
                top_k=6,
                include_profile=False,
                source="memory_policy",
                reason="Targeted recall request",
            )

        if _matches_any(query, _CORRECT_PATTERNS) or _matches_any(query, _PURGE_PATTERNS):
            return MemoryContextPlan(
                mode=MemoryContextMode.SEARCH,
                query=semantic_query,
                top_k=6,
                include_profile=False,
                source="memory_policy",
                reason="Memory update/delete request should inspect indexed memories",
            )

        if _matches_any(query, _STORE_PATTERNS) and self._infer_memory_type(q_lower) == "profile":
            return MemoryContextPlan(
                mode=MemoryContextMode.PROFILE_ONLY,
                query=semantic_query,
                include_profile=True,
                source="memory_policy",
                reason="Preference write should load existing profile for continuity",
            )

        if load_memory is False:
            return MemoryContextPlan(
                mode=MemoryContextMode.NONE,
                source="routing_plan",
                reason="Routing plan skipped memory",
            )

        if load_memory == "profile":
            return MemoryContextPlan(
                mode=MemoryContextMode.PROFILE_ONLY,
                query=semantic_query,
                include_profile=True,
                source="routing_plan",
                reason="Routing plan requested profile-only memory",
            )

        if load_memory in (True, None):
            return MemoryContextPlan(
                mode=MemoryContextMode.RETRIEVE,
                query=semantic_query,
                top_k=5,
                include_profile=True,
                source="routing_plan",
                reason="Routing plan requested task-relevant memory",
            )

        return MemoryContextPlan(mode=MemoryContextMode.NONE, source="fallback", reason="No memory plan")

    def _infer_memory_type(self, q_lower: str) -> str:
        if _matches_any(q_lower, _PROFILE_MEMORY_PATTERNS):
            return "profile"
        if _matches_any(q_lower, _PROCEDURAL_MEMORY_PATTERNS):
            return "procedural"
        return "semantic"
