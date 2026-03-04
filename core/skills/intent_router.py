"""Intent router — classify user queries to optimize tool selection and round limits.

Two restrictive intents:
- EXTERNAL_FETCH: block local tools, max 3 rounds (web search, API calls)
- CONVERSATIONAL: block all tools, max 0 rounds (greetings, opinions, chitchat)

Everything else: DEFAULT (no restriction).

Design notes:
- Keyword matching uses word-boundary-aware checks (not bare substring) to avoid
  false positives like "search" matching inside "research the codebase".
- Negative keywords suppress EXTERNAL_FETCH when the query is clearly about code.
- Threshold is set conservatively (0.25) — false DEFAULT is harmless, false
  EXTERNAL_FETCH blocks local tools and breaks the user's workflow.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field

from core.logging_config import get_logger

logger = get_logger(__name__)


@dataclass
class IntentClassification:
    """Result of intent classification."""

    intent: str  # "DEFAULT", "EXTERNAL_FETCH", "CONVERSATIONAL"
    confidence: float  # 0.0 - 1.0
    matched_keywords: list[str] = field(default_factory=list)


# Confidence threshold — below this, fall back to DEFAULT.
# Set conservatively: false DEFAULT is harmless, false restrictive intent is harmful.
_CONFIDENCE_THRESHOLD = 0.25

# Keyword patterns for CONVERSATIONAL intent
_CONVERSATIONAL_KEYWORDS_EN = frozenset({
    "hello", "hi", "hey", "thanks", "thank you", "bye", "goodbye",
    "good morning", "good evening", "how are you", "what's up",
    "who are you", "what can you do", "help me",
    "yes", "no", "ok", "okay", "sure", "great", "nice",
    "please", "sorry", "excuse me",
})

_CONVERSATIONAL_KEYWORDS_ZH = frozenset({
    "你好", "您好", "谢谢", "感谢", "再见", "拜拜",
    "早上好", "晚上好", "你是谁", "你能做什么",
    "好的", "可以", "是的", "不是", "没问题",
    "请", "抱歉", "对不起",
})

# Keyword patterns for EXTERNAL_FETCH intent
_EXTERNAL_FETCH_KEYWORDS_EN = frozenset({
    "search online", "look up", "find online", "web search",
    "what is the latest", "current price", "today's",
    "fetch from", "download", "api call", "http",
    "weather", "news", "stock price",
    "check the website", "browse",
})

_EXTERNAL_FETCH_KEYWORDS_ZH = frozenset({
    "搜索", "查找", "查一下", "网上找",
    "最新的", "当前价格", "今天的",
    "下载", "获取", "抓取",
    "天气", "新闻", "股价",
})

# Negative keywords: if present, suppress EXTERNAL_FETCH → DEFAULT.
# These indicate the user is working with local code, not asking for web content.
_CODE_CONTEXT_KEYWORDS = frozenset({
    "file", "code", "class", "function", "method", "variable",
    "refactor", "implement", "debug", "fix", "bug", "test",
    "import", "module", "package", "repository", "repo",
    "algorithm", "sort", "tree", "array", "list", "dict",
})

# Pre-compiled word-boundary patterns for all keyword sets.
# Built at import time so the cache is bounded and immutable at runtime.
_WORD_BOUNDARY_CACHE: dict[str, re.Pattern[str]] = {}


def _compile_pattern(keyword: str) -> re.Pattern[str]:
    """Compile a word-boundary regex for a keyword."""
    # For CJK characters, don't require word boundaries (they don't have spaces)
    if any("\u4e00" <= ch <= "\u9fff" for ch in keyword):
        return re.compile(re.escape(keyword), re.IGNORECASE)
    return re.compile(r"\b" + re.escape(keyword) + r"\b", re.IGNORECASE)


# Pre-populate at import time — bounded to the static keyword sets above
for _kw in (
    _CONVERSATIONAL_KEYWORDS_EN | _CONVERSATIONAL_KEYWORDS_ZH
    | _EXTERNAL_FETCH_KEYWORDS_EN | _EXTERNAL_FETCH_KEYWORDS_ZH
    | _CODE_CONTEXT_KEYWORDS
):
    _WORD_BOUNDARY_CACHE[_kw] = _compile_pattern(_kw)


def _word_boundary_pattern(keyword: str) -> re.Pattern[str]:
    """Look up pre-compiled pattern. Falls back to compile for unknown keywords."""
    pat = _WORD_BOUNDARY_CACHE.get(keyword)
    if pat is None:
        pat = _compile_pattern(keyword)
        _WORD_BOUNDARY_CACHE[keyword] = pat
    return pat


def _keyword_score(query: str, keywords: frozenset[str]) -> tuple[float, list[str]]:
    """Score query against keyword set using word-boundary matching.

    Returns (score, matched_keywords). Score = ratio of matched keyword chars
    to query length, capped at 1.0.
    """
    query_stripped = query.strip()
    if not query_stripped:
        return 0.0, []
    matched: list[str] = []
    for kw in keywords:
        if _word_boundary_pattern(kw).search(query_stripped):
            matched.append(kw)
    if not matched:
        return 0.0, []
    matched_chars = sum(len(kw) for kw in matched)
    score = min(matched_chars / max(len(query_stripped), 1), 1.0)
    return score, matched


def _has_code_context(query: str) -> bool:
    """Check if query contains code-related keywords that suppress EXTERNAL_FETCH."""
    query_lower = query.lower()
    return any(
        _word_boundary_pattern(kw).search(query_lower)
        for kw in _CODE_CONTEXT_KEYWORDS
    )


def classify_intent(query: str) -> IntentClassification:
    """Classify user query intent.

    Returns IntentClassification with intent, confidence, and matched keywords.
    Falls back to DEFAULT if confidence is below threshold.
    """
    # Check CONVERSATIONAL first (higher priority for short queries)
    conv_score_en, conv_matched_en = _keyword_score(query, _CONVERSATIONAL_KEYWORDS_EN)
    conv_score_zh, conv_matched_zh = _keyword_score(query, _CONVERSATIONAL_KEYWORDS_ZH)
    conv_score = max(conv_score_en, conv_score_zh)
    conv_matched = conv_matched_en + conv_matched_zh

    # Short queries with conversational keywords are very likely conversational
    if len(query.strip()) < 20 and conv_score > 0:
        conv_score = min(conv_score * 2, 1.0)

    # Check EXTERNAL_FETCH
    ext_score_en, ext_matched_en = _keyword_score(query, _EXTERNAL_FETCH_KEYWORDS_EN)
    ext_score_zh, ext_matched_zh = _keyword_score(query, _EXTERNAL_FETCH_KEYWORDS_ZH)
    ext_score = max(ext_score_en, ext_score_zh)
    ext_matched = ext_matched_en + ext_matched_zh

    # Suppress EXTERNAL_FETCH when query has code-context keywords
    if ext_score > 0 and _has_code_context(query):
        ext_score = 0.0
        ext_matched = []

    # Pick highest scoring intent
    if conv_score >= ext_score and conv_score >= _CONFIDENCE_THRESHOLD:
        return IntentClassification(
            intent="CONVERSATIONAL",
            confidence=conv_score,
            matched_keywords=conv_matched,
        )
    if ext_score > conv_score and ext_score >= _CONFIDENCE_THRESHOLD:
        return IntentClassification(
            intent="EXTERNAL_FETCH",
            confidence=ext_score,
            matched_keywords=ext_matched,
        )

    return IntentClassification(
        intent="DEFAULT",
        confidence=0.0,
        matched_keywords=[],
    )
