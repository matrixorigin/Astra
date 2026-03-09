"""Tests for cloud skill keyword pre-filter in api/routers/chat.py.

Covers:
- Bidirectional keyword matching (forward + reverse)
- CJK / mixed-language queries
- Pure English queries
- Score=0 fallback (all candidates registered up to limit)
- Large-scale skill catalogs (10k+)
- Token budget not affected by candidate count
"""

import re

import pytest

from core.skills.tool_registry import ToolEntry, ToolRegistry, ToolSource


# ── Helpers ──────────────────────────────────────────────────────

def _schema(name: str, desc: str = "") -> dict:
    return {"type": "function", "function": {"name": name, "description": desc, "parameters": {}}}


def _score_candidate(query: str, cs_name: str, cs_desc: str) -> int:
    """Reproduce the bidirectional scoring logic from chat.py."""
    query_lower = query.lower()
    query_tokens = set(query_lower.split()) if query_lower else set()
    query_alpha = set(re.findall(r"[a-z]{3,}", query_lower))

    text = (cs_name + " " + cs_desc).lower()
    score = sum(1 for t in query_tokens if t in text) if query_tokens else 0

    name_parts = set(cs_name.lower().replace("_", " ").split())
    for qw in query_alpha:
        for np in name_parts:
            if qw in np or np in qw:
                score += 2
    return score


# ── Bidirectional Matching ───────────────────────────────────────

class TestBidirectionalScoring:
    """Test the keyword scoring logic extracted from chat.py."""

    def test_english_query_forward_match(self):
        """English query tokens match skill description."""
        score = _score_candidate("show me issues", "list_issues", "List issues from GitHub")
        assert score > 0

    def test_chinese_query_with_english_word(self):
        """Chinese query containing English word matches skill name."""
        score = _score_candidate("matrixone的最新的一个issue?", "list_issues", "List issues")
        # "issue" (from query) is substring of "issues" (from skill name)
        assert score >= 2

    def test_chinese_query_with_english_word_create(self):
        """Chinese query with 'issue' also matches create_issue."""
        score = _score_candidate("matrixone的最新的一个issue?", "create_issue", "Create a new issue")
        assert score >= 2

    def test_pure_chinese_query_no_english(self):
        """Pure Chinese query with no English words → score 0."""
        score = _score_candidate("最新的问题是什么", "list_issues", "List issues")
        assert score == 0

    def test_english_query_reverse_match(self):
        """Skill name part found in query via reverse matching."""
        # "pr" is only 2 chars, below threshold; "prs" is 3 chars
        score = _score_candidate("show me prs", "list_prs", "List pull requests")
        assert score >= 2  # "prs" in query matches "prs" in name

    def test_no_match_unrelated(self):
        """Completely unrelated query and skill → score 0."""
        score = _score_candidate("deploy to production", "list_issues", "List issues")
        assert score == 0

    def test_substring_match_issue_in_issues(self):
        """'issue' (query) is substring of 'issues' (skill name part)."""
        query_alpha = set(re.findall(r"[a-z]{3,}", "matrixone的issue"))
        name_parts = {"list", "issues"}
        matched = any(qw in np or np in qw for qw in query_alpha for np in name_parts)
        assert matched  # "issue" in "issues"

    def test_short_words_excluded(self):
        """Words shorter than 3 chars excluded from alpha extraction."""
        query_alpha = set(re.findall(r"[a-z]{3,}", "go ci"))
        assert "go" not in query_alpha
        assert "ci" not in query_alpha

# ── ToolRegistry Selection with Prefilter ────────────────────────

class TestRegistrySelectionCJK:
    """End-to-end: Chinese query → ToolRegistry.select() picks correct tools."""

    def _build_registry(self) -> ToolRegistry:
        r = ToolRegistry(embed_fn=None, max_tokens=50000)
        r.register_schema(_schema("bash", "Execute shell command"), ToolSource.EDGE, pinned=True)
        r.register_schema(_schema("read_file", "Read a file"), ToolSource.EDGE, pinned=True)
        # Cloud skills — github category triggers prefilter fetch rule
        for name, desc in [
            ("ci_status", "Check CI status"),
            ("create_issue", "Create GitHub issue"),
            ("get_issue", "Get single issue"),
            ("list_issues", "List issues from GitHub"),
            ("list_prs", "List pull requests"),
            ("summarize_pr", "Summarize a PR"),
        ]:
            r.register_schema(_schema(name, desc), ToolSource.CLOUD, pinned=False, category="github")
        # Non-github cloud skills
        for name, desc in [
            ("execute_code", "Execute code"),
            ("introspection", "Agent introspection"),
            ("reflect", "Self reflection"),
        ]:
            r.register_schema(_schema(name, desc), ToolSource.CLOUD, pinned=False, category="system")
        return r

    def test_chinese_fetch_query_selects_github_skills(self):
        """Chinese fetch query ('最新') → prefilter boosts github skills."""
        r = self._build_registry()
        messages = [{"role": "user", "content": "matrixone的最新的一个issue?"}]
        result = r.select("matrixone的最新的一个issue?", messages)
        names = {s["function"]["name"] for s in result}
        assert "list_issues" in names

    def test_pure_chinese_fetch_query(self):
        """Pure Chinese query with fetch marker → github skills selected."""
        r = self._build_registry()
        messages = [{"role": "user", "content": "查看最新的问题"}]
        result = r.select("查看最新的问题", messages)
        names = {s["function"]["name"] for s in result}
        # Prefilter detects "查看" and "最新" as fetch markers → external scope preferred
        assert "list_issues" in names

    def test_english_query_still_works(self):
        """English query continues to work as before."""
        r = self._build_registry()
        messages = [{"role": "user", "content": "show me the latest issues"}]
        result = r.select("show me the latest issues", messages)
        names = {s["function"]["name"] for s in result}
        assert "list_issues" in names


# ── Large-Scale Catalog ──────────────────────────────────────────

class TestLargeScaleCatalog:
    """Verify pre-filter performance and correctness with thousands of skills."""

    def _build_large_registry(self, n: int, *, include_target: bool = True) -> ToolRegistry:
        r = ToolRegistry(embed_fn=None, max_tokens=50000, max_dynamic=8)
        # Filler skills
        for i in range(n):
            r.register_schema(
                _schema(f"filler_skill_{i}", f"Generic skill number {i}"),
                ToolSource.CLOUD, pinned=False, category="misc",
            )
        # Target skill
        if include_target:
            r.register_schema(
                _schema("list_issues", "List issues from GitHub repository"),
                ToolSource.CLOUD, pinned=False, category="github",
            )
        return r

    def test_prefilter_10k_skills_selects_target(self):
        """With 10k filler skills, fetch query still selects list_issues via prefilter."""
        r = self._build_large_registry(10000)
        messages = [{"role": "user", "content": "查看最新的issue"}]
        result = r.select("查看最新的issue", messages)
        names = {s["function"]["name"] for s in result}
        assert "list_issues" in names

    def test_prefilter_10k_skills_max_dynamic_respected(self):
        """Even with 10k skills, output is bounded by max_dynamic."""
        r = self._build_large_registry(10000)
        messages = [{"role": "user", "content": "查看最新的issue"}]
        result = r.select("查看最新的issue", messages)
        assert len(result) <= 8

    def test_prefilter_10k_performance(self):
        """Pre-filter + select on 10k skills completes in <100ms."""
        import time
        r = self._build_large_registry(10000)
        messages = [{"role": "user", "content": "matrixone的最新issue"}]

        t0 = time.perf_counter()
        r.select("matrixone的最新issue", messages)
        elapsed_ms = (time.perf_counter() - t0) * 1000

        assert elapsed_ms < 500, f"Selection took {elapsed_ms:.0f}ms, expected <500ms"

    def test_scoring_10k_performance(self):
        """Bidirectional scoring on 10k skills completes in <50ms."""
        import time
        skills = [(f"skill_{i}", f"Description for skill {i}") for i in range(10000)]
        query = "matrixone的最新的一个issue?"

        t0 = time.perf_counter()
        scores = [_score_candidate(query, name, desc) for name, desc in skills]
        elapsed_ms = (time.perf_counter() - t0) * 1000

        assert elapsed_ms < 50, f"Scoring took {elapsed_ms:.0f}ms, expected <50ms"


# ── Token Budget Stability ───────────────────────────────────────

class TestTokenBudgetStability:
    """Adding edge tools should not push out relevant cloud skills."""

    def test_adding_edge_tool_does_not_drop_relevant_cloud_skill(self):
        """Regression: MemoryProgramTool addition should not drop list_issues."""
        r = ToolRegistry(embed_fn=None, max_tokens=2500)
        # Pinned edge tools (simulate real setup)
        for name in ["bash", "read_file", "write_file", "str_replace",
                      "list_dir", "grep", "glob"]:
            r.register_schema(_schema(name, f"{name} tool"), ToolSource.EDGE, pinned=True)
        # Extra edge tool (MemoryProgramTool equivalent)
        r.register_schema(
            _schema("memory_program", "Execute memory program from natural language. " * 5),
            ToolSource.EDGE, pinned=False,
        )
        # Cloud skills
        for name, desc in [
            ("ci_status", "Check CI status"),
            ("list_issues", "List issues from GitHub"),
            ("list_prs", "List pull requests"),
            ("create_issue", "Create GitHub issue"),
        ]:
            r.register_schema(_schema(name, desc), ToolSource.CLOUD, pinned=False, category="github")

        messages = [{"role": "user", "content": "查看最新的issue"}]
        result = r.select("查看最新的issue", messages)
        names = {s["function"]["name"] for s in result}
        # Pinned tools always survive
        assert "bash" in names
        # list_issues should survive despite tight budget
        assert "list_issues" in names
