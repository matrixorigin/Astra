"""Unified Intent Routing — single-pass classification for context, tools, and budgets.

Design doc: docs/design/intent-unification.md

Three formerly separate systems are now unified into one RoutingDecision:
  1. Tool filtering (EXTERNAL_FETCH / CONVERSATIONAL) → tool_filter, max_tool_rounds
  2. Context loading (preference / command / feedback / question) → plan
  3. Task type (CODE_REVIEW / DEBUGGING / PLANNING / GENERAL) → task_type

Architecture:
  Tier 0 (<1ms, 3 independent KeywordRegistries) → adaptive threshold → Tier 1 (~180ms, cheapest LLM) → fallback
"""

from __future__ import annotations

import asyncio
import logging
import re
from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Protocol, runtime_checkable

from core.db_consumer import DbFactory

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Enums
# ---------------------------------------------------------------------------


class ToolFilter(str, Enum):
    """Tool filtering mode derived from intent classification."""
    NONE = "none"                # DEFAULT — no filtering
    LOCAL_BLOCKED = "local_blocked"  # EXTERNAL_FETCH — block local tools
    ALL_BLOCKED = "all_blocked"      # CONVERSATIONAL — block all tools


class TaskType(str, Enum):
    """Task types for context optimization (absorbed from ContextManager)."""
    CODE_REVIEW = "code_review"
    PLANNING = "planning"
    DEBUGGING = "debugging"
    GENERAL = "general"


# ---------------------------------------------------------------------------
# Dataclasses
# ---------------------------------------------------------------------------

@dataclass(frozen=True)
class RoutingResult:
    """Output of a single classification tier."""
    intent: str | None  # preference, command, feedback, question, or None
    confidence: float   # 0.0, 0.80, or 0.95
    tier: int           # 0 or 1
    matched_by: str     # "regex", "heuristic", "both", "llm", "fallback"


@dataclass(frozen=True)
class ContextLoadingPlan:
    """What to load for a given intent."""
    load_tools: bool          # include tool schemas
    load_history: bool | int  # False=skip, True=full, int=last N turns
    load_memory: bool | str   # False=skip, True=full, "profile"=L0 only
    estimated_tokens: int


@dataclass
class Tier1Result:
    """Output of Tier 1 parallel execution."""
    routing: RoutingResult | None = None
    compressed_memory: str | None = None
    pruned_tools: list[str] | None = None


MAX_TOOL_ROUNDS = 10  # Default max tool rounds (imported by chat_loop)


@dataclass
class RoutingDecision:
    """Single source of truth for all intent-derived decisions.

    All fields are guaranteed to be populated after IntentRouter.route() —
    callers never need None-checks or fallback defaults.
    """
    plan: ContextLoadingPlan
    routing_result: RoutingResult
    tier1_result: Tier1Result | None = None
    threshold_used: float = 0.85
    # Tool filtering (absorbed from classify_intent / IntentClassification)
    tool_filter: ToolFilter = ToolFilter.NONE
    max_tool_rounds: int = MAX_TOOL_ROUNDS
    # Task type + topic shift (absorbed from ContextManager.classify_task)
    task_type: TaskType = TaskType.GENERAL
    topic_shift_score: float = 0.0


# Intent → ContextLoadingPlan (from doc "Context Loading by Intent" table)
INTENT_PLANS: dict[str, ContextLoadingPlan] = {
    "preference": ContextLoadingPlan(load_tools=False, load_history=False, load_memory="profile", estimated_tokens=100),
    "command":    ContextLoadingPlan(load_tools=True,  load_history=False, load_memory=False,     estimated_tokens=400),
    "feedback":   ContextLoadingPlan(load_tools=False, load_history=2,     load_memory=False,     estimated_tokens=600),
    "question":   ContextLoadingPlan(load_tools=True,  load_history=True,  load_memory=True,      estimated_tokens=2400),
}

# Full-context fallback plan (same as question)
_FALLBACK_PLAN = INTENT_PLANS["question"]


# ---------------------------------------------------------------------------
# Router Protocol + Registry
# ---------------------------------------------------------------------------

@runtime_checkable
class RoutingStrategy(Protocol):
    """Interface for pluggable routing strategies."""

    async def route(
        self,
        query: str,
        history_len: int = 0,
        memory_text: str | None = None,
        tool_names: list[str] | None = None,
        force_intent: str | None = None,
    ) -> RoutingDecision: ...


_ROUTER_REGISTRY: dict[str, type] = {}
_registry_lock = __import__("threading").Lock()


def register_router(name: str):
    """Decorator to register a routing strategy by name."""
    def _wrap(cls: type) -> type:
        with _registry_lock:
            if name in _ROUTER_REGISTRY:
                raise ValueError(
                    f"Router '{name}' already registered by {_ROUTER_REGISTRY[name].__name__}; "
                    f"cannot re-register with {cls.__name__}"
                )
            _ROUTER_REGISTRY[name] = cls
        return cls
    return _wrap


def get_router(name: str, db_factory: DbFactory) -> RoutingStrategy:
    """Instantiate a registered router by name."""
    with _registry_lock:
        cls = _ROUTER_REGISTRY[name]
    return cls(db_factory=db_factory)


def list_routers() -> list[str]:
    """Return sorted names of all registered routers."""
    with _registry_lock:
        return sorted(_ROUTER_REGISTRY.keys())


def _reset_registry_for_testing() -> None:
    """Remove all non-default routers. Test-only."""
    with _registry_lock:
        default_cls = _ROUTER_REGISTRY.get("default")
        _ROUTER_REGISTRY.clear()
        if default_cls:
            _ROUTER_REGISTRY["default"] = default_cls


# ---------------------------------------------------------------------------
# KeywordRegistry — single-dimension keyword matcher
# ---------------------------------------------------------------------------

@dataclass(frozen=True)
class RegistryMatch:
    """Result of a single KeywordRegistry.match() call."""
    label: str | None   # matched label (e.g. "CONVERSATIONAL") or None
    score: float        # 0.0 - 1.0
    matched_keywords: list[str] = field(default_factory=list)


def _compile_pattern(keyword: str) -> re.Pattern[str]:
    """Compile a word-boundary regex. CJK chars don't require word boundaries."""
    if any("\u4e00" <= ch <= "\u9fff" for ch in keyword):
        return re.compile(re.escape(keyword), re.IGNORECASE)
    return re.compile(r"\b" + re.escape(keyword) + r"\b", re.IGNORECASE)


class KeywordRegistry:
    """Single-dimension keyword matcher with word-boundary-aware matching.

    Each registry maps labels → keyword lists. match() returns the best-scoring label.
    """

    def __init__(self, name: str, keywords: dict[str, list[str]], negative_keywords: dict[str, list[str]] | None = None):
        self.name = name
        self._patterns: dict[str, list[tuple[str, re.Pattern[str]]]] = {}
        for label, words in keywords.items():
            self._patterns[label] = [(w, _compile_pattern(w)) for w in words]
        self._negative: dict[str, list[tuple[str, re.Pattern[str]]]] = {}
        if negative_keywords:
            for label, words in negative_keywords.items():
                self._negative[label] = [(w, _compile_pattern(w)) for w in words]

    def match(self, query: str) -> RegistryMatch:
        """Return the best-scoring label for the query."""
        query_stripped = query.strip()
        if not query_stripped:
            return RegistryMatch(label=None, score=0.0)

        best_label: str | None = None
        best_score = 0.0
        best_matched: list[str] = []

        for label, patterns in self._patterns.items():
            matched = [kw for kw, pat in patterns if pat.search(query_stripped)]
            if not matched:
                continue
            # Negative keywords suppress this label (e.g. code-context suppresses EXTERNAL_FETCH).
            # `neg` is [] when no negatives are configured — short-circuits the `any()` check.
            neg = self._negative.get(label, [])
            if neg and any(pat.search(query_stripped) for _, pat in neg):
                continue
            score = min(sum(len(kw) for kw in matched) / max(len(query_stripped), 1), 1.0)
            if score > best_score:
                best_score = score
                best_label = label
                best_matched = matched

        return RegistryMatch(label=best_label, score=best_score, matched_keywords=best_matched)


# ---------------------------------------------------------------------------
# Keyword registries for each dimension
# ---------------------------------------------------------------------------

# Dimension 1: Tool filtering (from old classify_intent)
_TOOL_FILTER_REGISTRY = KeywordRegistry(
    name="tool_filter",
    keywords={
        "CONVERSATIONAL": [
            "hello", "hi", "hey", "thanks", "thank you", "bye", "goodbye",
            "good morning", "good evening", "how are you", "what's up",
            "who are you", "what can you do", "help me",
            "yes", "no", "ok", "okay", "sure", "great", "nice",
            "please", "sorry", "excuse me",
            "你好", "您好", "谢谢", "感谢", "再见", "拜拜",
            "早上好", "晚上好", "你是谁", "你能做什么",
            "好的", "可以", "是的", "不是", "没问题",
            "请", "抱歉", "对不起",
        ],
        "EXTERNAL_FETCH": [
            "search online", "look up", "find online", "web search",
            "what is the latest", "current price", "today's",
            "fetch from", "download", "api call", "http",
            "weather", "news", "stock price",
            "check the website", "browse",
            "搜索", "查找", "查一下", "网上找",
            "最新的", "当前价格", "今天的",
            "下载", "获取", "抓取",
            "天气", "新闻", "股价",
        ],
    },
    negative_keywords={
        # Code-context keywords suppress EXTERNAL_FETCH
        "EXTERNAL_FETCH": [
            "file", "code", "class", "function", "method", "variable",
            "refactor", "implement", "debug", "fix", "bug", "test",
            "import", "module", "package", "repository", "repo",
            "algorithm", "sort", "tree", "array", "list", "dict",
        ],
    },
)

# Dimension 2: Intent (existing Tier 0 regex patterns)
_INTENT_PATTERNS: dict[str, list[re.Pattern]] = {
    "preference": [re.compile(r"记住|remember|I prefer|I use|需要|always use", re.I)],
    "command":    [re.compile(r"^(run|execute|delete|create|list)\b", re.I)],
    "feedback":   [re.compile(r"^(不对|wrong|no,|actually)", re.I)],
}

# Dimension 3: Task type (from old classify_task)
_TASK_TYPE_REGISTRY = KeywordRegistry(
    name="task_type",
    keywords={
        "code_review": ["review", "code review", "PR", "pull request", "refactor", "clean up"],
        "debugging":   ["debug", "error", "bug", "fix", "traceback", "exception", "crash", "fail"],
        "planning":    ["plan", "design", "architect", "roadmap", "strategy", "proposal"],
    },
)


# ---------------------------------------------------------------------------
# Tier 0: Unified three-dimension engine
# ---------------------------------------------------------------------------

class Tier0Engine:
    """<1ms regex + heuristic + keyword engine across three dimensions."""

    def classify(self, query: str, history_len: int = 0) -> RoutingResult:
        """Classify intent dimension (preference/command/feedback/question)."""
        regex_intent = self._regex_classify(query)
        heuristic_intent = self._heuristic_classify(query, history_len)

        if regex_intent and heuristic_intent and regex_intent == heuristic_intent:
            return RoutingResult(intent=regex_intent, confidence=0.95, tier=0, matched_by="both")
        if regex_intent:
            return RoutingResult(intent=regex_intent, confidence=0.80, tier=0, matched_by="regex")
        if heuristic_intent:
            return RoutingResult(intent=heuristic_intent, confidence=0.80, tier=0, matched_by="heuristic")
        return RoutingResult(intent=None, confidence=0.0, tier=0, matched_by="none")

    def classify_tool_filter(self, query: str) -> tuple[ToolFilter, int]:
        """Classify tool filtering dimension. Returns (filter, max_rounds).

        Short queries (<20 chars) with CONVERSATIONAL match get boosted confidence.
        CONVERSATIONAL > EXTERNAL_FETCH > NONE (most restrictive wins).
        """
        match = _TOOL_FILTER_REGISTRY.match(query)
        if match.label == "CONVERSATIONAL":
            score = match.score
            if len(query.strip()) < 20 and score > 0:
                score = min(score * 2, 1.0)
            if score >= 0.25:
                return ToolFilter.ALL_BLOCKED, 0
        elif match.label == "EXTERNAL_FETCH" and match.score >= 0.25:
            return ToolFilter.LOCAL_BLOCKED, 3
        return ToolFilter.NONE, MAX_TOOL_ROUNDS

    def classify_task_type(self, query: str) -> TaskType:
        """Classify task type dimension."""
        match = _TASK_TYPE_REGISTRY.match(query)
        if match.label:
            # Defensive: TaskType values are lowercase; guard against registry misconfiguration
            return TaskType(match.label.lower())
        return TaskType.GENERAL

    def _regex_classify(self, query: str) -> str | None:
        for intent, patterns in _INTENT_PATTERNS.items():
            for pat in patterns:
                if pat.search(query):
                    return intent
        return None

    def _heuristic_classify(self, query: str, history_len: int) -> str | None:
        stripped = query.strip()
        if not stripped:
            return None
        words = stripped.split()
        if len(words) <= 3 and stripped.endswith("?"):
            return None
        if history_len == 0 and not stripped.endswith("?"):
            return "command"
        return None


# ---------------------------------------------------------------------------
# User Correction Detection
# ---------------------------------------------------------------------------

_CORRECTION_PATTERN = re.compile(
    r"不对|错了|不是这样|你搞错|不正确|wrong|incorrect|that's not|no,\s|actually,?\s",
    re.I,
)


def detect_correction(query: str) -> bool:
    """Detect user correction patterns."""
    return bool(_CORRECTION_PATTERN.search(query.strip()))


# ---------------------------------------------------------------------------
# Tier 1: Cheapest LLM Parallel Engine
# ---------------------------------------------------------------------------

_CLASSIFY_PROMPT = (
    "Classify this user message into exactly one intent: preference, command, feedback, question.\n"
    "Reply with JSON: {\"intent\": \"...\", \"confidence\": 0.0-1.0}\n"
    "- preference: user states a preference or asks to remember something\n"
    "- command: user wants to execute an action (run, create, delete, list)\n"
    "- feedback: user corrects or disagrees with previous response\n"
    "- question: user asks a question or needs analysis\n"
    "Message: "
)

_COMPRESS_PROMPT = (
    "Compress this context. Keep all facts, preferences, and key details. "
    "Remove filler and redundancy. Output ONLY the compressed text."
)

_PRUNE_PROMPT = (
    "Given this user query and tool list, return ONLY the names of relevant tools as JSON array.\n"
    "Query: {query}\nTools: {tools}\nRelevant tools:"
)

_TIER1_TIMEOUT = 2.0


class Tier1Engine:
    """Cheapest-LLM parallel engine: classify || compress || prune_tools."""

    def __init__(self, db_factory: DbFactory):
        self._db_factory = db_factory

    async def run_parallel(
        self,
        query: str,
        memory_text: str | None = None,
        tool_names: list[str] | None = None,
        history_summary: str | None = None,
    ) -> Tier1Result:
        tasks = [self._classify(query, history_summary)]
        has_memory = memory_text and len(memory_text) > 100
        has_tools = tool_names and len(tool_names) > 3
        if has_memory:
            tasks.append(self._compress(memory_text))
        if has_tools:
            tasks.append(self._prune_tools(query, tool_names))

        results = await asyncio.gather(*tasks, return_exceptions=True)

        routing = results[0] if not isinstance(results[0], BaseException) else None
        idx = 1
        compressed = None
        if has_memory:
            compressed = results[idx] if not isinstance(results[idx], BaseException) else None
            idx += 1
        pruned = None
        if has_tools:
            pruned = results[idx] if not isinstance(results[idx], BaseException) else None

        return Tier1Result(routing=routing, compressed_memory=compressed, pruned_tools=pruned)

    async def _classify(self, query: str, history_summary: str | None) -> RoutingResult:
        import json as _json
        prompt = _CLASSIFY_PROMPT + query
        if history_summary:
            prompt += f"\nRecent context: {history_summary[:200]}"
        resp = await asyncio.wait_for(
            asyncio.to_thread(self._llm_call, prompt), timeout=_TIER1_TIMEOUT,
        )
        try:
            data = _json.loads(resp)
            intent = data.get("intent", "question")
            conf = float(data.get("confidence", 0.5))
            if intent not in INTENT_PLANS:
                intent = "question"
            return RoutingResult(intent=intent, confidence=conf, tier=1, matched_by="llm")
        except Exception:
            return RoutingResult(intent="question", confidence=0.5, tier=1, matched_by="llm")

    async def _compress(self, memory_text: str) -> str:
        return await asyncio.wait_for(
            asyncio.to_thread(self._llm_call, _COMPRESS_PROMPT + "\n\n" + memory_text),
            timeout=_TIER1_TIMEOUT,
        )

    async def _prune_tools(self, query: str, tool_names: list[str]) -> list[str]:
        import json as _json
        prompt = _PRUNE_PROMPT.format(query=query, tools=", ".join(tool_names))
        resp = await asyncio.wait_for(
            asyncio.to_thread(self._llm_call, prompt), timeout=_TIER1_TIMEOUT,
        )
        try:
            parsed = _json.loads(resp)
            if isinstance(parsed, list):
                return [t for t in parsed if t in tool_names]
        except Exception:
            pass
        return tool_names

    def _llm_call(self, prompt: str) -> str:
        from core.llm.client import LLMClient
        llm = LLMClient(self._db_factory)
        resp = llm.chat(
            messages=[{"role": "user", "content": prompt}],
            user_id="system",
            model="cheapest",
            temperature=0.0,
            task_hint="routing",
        )
        return resp.content.strip()


# ---------------------------------------------------------------------------
# IntentRouter — Full Cascade Orchestrator
# ---------------------------------------------------------------------------

# Tools considered "local" — blocked when tool_filter is LOCAL_BLOCKED
LOCAL_TOOLS = frozenset({
    "grep", "shell", "execute_bash", "file_read", "file_write",
    "git", "search", "reflect", "introspection",
    "scratchpad_write", "scratchpad_read", "scratchpad_close",
})


@register_router("default")
class IntentRouter:
    """Tier 0 → adaptive threshold → Tier 1 → threshold → fallback.

    Single entry point for all intent classification. Returns a fully-populated
    RoutingDecision with tool_filter, max_tool_rounds, task_type, and plan.
    """

    def __init__(self, db_factory: DbFactory):
        self._db_factory = db_factory
        self._tier0 = Tier0Engine()
        self._tier1 = Tier1Engine(db_factory)

    async def route(
        self,
        query: str,
        history_len: int = 0,
        memory_text: str | None = None,
        tool_names: list[str] | None = None,
        force_intent: str | None = None,
    ) -> RoutingDecision:
        from core.context.routing_metrics import adaptive_threshold

        threshold = adaptive_threshold()

        # Dimensions that are always computed by Tier 0 (independent of intent)
        tool_filter, max_tool_rounds = self._tier0.classify_tool_filter(query)
        task_type = self._tier0.classify_task_type(query)

        # Override: preference and conversational both block tools
        # preference intent → ALL_BLOCKED (no tools needed, just memory write)

        # Force intent (e.g. user correction → feedback)
        if force_intent and force_intent in INTENT_PLANS:
            result = RoutingResult(intent=force_intent, confidence=1.0, tier=0, matched_by="forced")
            # preference forces ALL_BLOCKED
            if force_intent == "preference":
                tool_filter = ToolFilter.ALL_BLOCKED
                max_tool_rounds = 0
            return RoutingDecision(
                plan=INTENT_PLANS[force_intent], routing_result=result,
                threshold_used=threshold,
                tool_filter=tool_filter, max_tool_rounds=max_tool_rounds,
                task_type=task_type,
            )

        # Tier 0: intent dimension
        tier0 = self._tier0.classify(query, history_len)
        if tier0.intent and tier0.confidence >= threshold:
            logger.info("Tier 0 routed: intent=%s conf=%.2f threshold=%.2f", tier0.intent, tier0.confidence, threshold)
            # preference intent → ALL_BLOCKED
            if tier0.intent == "preference":
                tool_filter = ToolFilter.ALL_BLOCKED
                max_tool_rounds = 0
            return RoutingDecision(
                plan=INTENT_PLANS[tier0.intent], routing_result=tier0,
                threshold_used=threshold,
                tool_filter=tool_filter, max_tool_rounds=max_tool_rounds,
                task_type=task_type,
            )

        # Tier 1
        try:
            tier1 = await self._tier1.run_parallel(query, memory_text, tool_names)
        except Exception as e:
            logger.warning("Tier 1 failed, falling back: %s", e)
            fallback = RoutingResult(intent="question", confidence=0.0, tier=1, matched_by="fallback")
            return RoutingDecision(
                plan=_FALLBACK_PLAN, routing_result=fallback, threshold_used=threshold,
                tool_filter=tool_filter, max_tool_rounds=max_tool_rounds,
                task_type=task_type,
            )

        if tier1.routing and tier1.routing.intent and tier1.routing.confidence >= threshold:
            plan = INTENT_PLANS.get(tier1.routing.intent, _FALLBACK_PLAN)
            logger.info("Tier 1 routed: intent=%s conf=%.2f", tier1.routing.intent, tier1.routing.confidence)
            if tier1.routing.intent == "preference":
                tool_filter = ToolFilter.ALL_BLOCKED
                max_tool_rounds = 0
            return RoutingDecision(
                plan=plan, routing_result=tier1.routing,
                tier1_result=tier1, threshold_used=threshold,
                tool_filter=tool_filter, max_tool_rounds=max_tool_rounds,
                task_type=task_type,
            )

        # Fallback — full context
        fallback = RoutingResult(intent="question", confidence=0.0, tier=1, matched_by="fallback")
        return RoutingDecision(
            plan=_FALLBACK_PLAN, routing_result=fallback,
            tier1_result=tier1, threshold_used=threshold,
            tool_filter=tool_filter, max_tool_rounds=max_tool_rounds,
            task_type=task_type,
        )

    def route_sync(self, **kwargs) -> RoutingDecision:
        """Sync wrapper for non-async callers."""
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            loop = None
        if loop and loop.is_running():
            import concurrent.futures
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                return pool.submit(asyncio.run, self.route(**kwargs)).result(timeout=5)
        return asyncio.run(self.route(**kwargs))
