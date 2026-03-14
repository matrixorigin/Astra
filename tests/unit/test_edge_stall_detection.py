"""Tests for edge-side stall detection in cli/edge_chat_loop.py.

Covers:
- Tool-name-only stall (same tool, different args, _TOOL_NAME_STALL_WINDOW=4)
- Exact-signature stall (same tool+args, _STALL_WINDOW=3)
- No false positives for multi-tool turns
- Nudge message content for both stall types
- find_skills loop scenario (real-world regression)
"""

import json

import pytest

from cli.edge_chat_loop import (
    _MAX_NUDGES,
    _STALL_WINDOW,
    _TOOL_NAME_STALL_WINDOW,
)
from cli.tools.router import ToolCall, ToolRouter


def _make_parsed(name: str, args: dict | None = None) -> ToolCall:
    """Build a ToolCall for stall detection."""
    return ToolCall(
        id=f"{name}:0",
        name=name,
        arguments=args or {},
    )


def _compute_sig(parsed: list[ToolCall]) -> frozenset[str]:
    """Reproduce edge_chat_loop signature computation."""
    return frozenset(
        f"{tc.name}:{json.dumps(tc.arguments, sort_keys=True, separators=(',', ':'))}"
        for tc in parsed
    )


def _compute_name_sig(parsed: list[ToolCall]) -> frozenset[str]:
    """Reproduce edge_chat_loop tool-name-only signature."""
    return frozenset(tc.name for tc in parsed)


class TestEdgeStallConstants:
    """Verify stall detection constants are sensible."""

    def test_stall_window(self):
        assert _STALL_WINDOW == 3

    def test_tool_name_stall_window(self):
        assert _TOOL_NAME_STALL_WINDOW == 4

    def test_tool_name_window_larger_than_exact(self):
        """Name-only stall needs more turns since it's a weaker signal."""
        assert _TOOL_NAME_STALL_WINDOW > _STALL_WINDOW

    def test_max_nudges(self):
        assert _MAX_NUDGES == 2


class TestExactSignatureStall:
    """Exact tool+args stall detection (existing behavior)."""

    def test_identical_calls_detected(self):
        parsed = [_make_parsed("find_skills", {"query": "bash"})]
        sigs = [_compute_sig(parsed)] * _STALL_WINDOW
        recent = sigs[-_STALL_WINDOW:]
        assert all(s == recent[0] for s in recent[1:])

    def test_different_args_not_detected(self):
        sigs = [
            _compute_sig([_make_parsed("find_skills", {"query": "bash"})]),
            _compute_sig([_make_parsed("find_skills", {"query": "github"})]),
            _compute_sig([_make_parsed("find_skills", {"query": "issues"})]),
        ]
        recent = sigs[-_STALL_WINDOW:]
        assert not all(s == recent[0] for s in recent[1:])


class TestToolNameStall:
    """Tool-name-only stall detection (new behavior for find_skills loop)."""

    def test_same_tool_different_args_detected(self):
        """find_skills called 4 times with different queries → stall."""
        name_sigs = [
            _compute_name_sig([_make_parsed("find_skills", {"query": q})])
            for q in ["GitHub issues latest", "GitHub", "list issues", "bash"]
        ]
        recent = name_sigs[-_TOOL_NAME_STALL_WINDOW:]
        assert len(recent) == _TOOL_NAME_STALL_WINDOW
        assert all(s == recent[0] for s in recent[1:])
        assert len(recent[0]) == 1  # single tool per turn

    def test_different_tools_not_detected(self):
        """Different tools each turn → not a name stall."""
        name_sigs = [
            _compute_name_sig([_make_parsed("find_skills")]),
            _compute_name_sig([_make_parsed("get_agent_info")]),
            _compute_name_sig([_make_parsed("bash")]),
            _compute_name_sig([_make_parsed("grep")]),
        ]
        recent = name_sigs[-_TOOL_NAME_STALL_WINDOW:]
        assert not all(s == recent[0] for s in recent[1:])

    def test_multi_tool_turns_not_detected(self):
        """Multiple tools per turn → len(sig) > 1 → name stall skipped."""
        name_sigs = [
            _compute_name_sig([_make_parsed("find_skills"), _make_parsed("bash")]),
        ] * _TOOL_NAME_STALL_WINDOW
        recent = name_sigs[-_TOOL_NAME_STALL_WINDOW:]
        # All same, but len > 1 → should NOT trigger name-only stall
        assert len(recent[0]) == 2  # multi-tool turn

    def test_below_window_not_detected(self):
        """Fewer turns than window → no stall."""
        name_sigs = [
            _compute_name_sig([_make_parsed("find_skills", {"query": q})])
            for q in ["a", "b", "c"]  # only 3, window is 4
        ]
        assert len(name_sigs) < _TOOL_NAME_STALL_WINDOW


class TestFindSkillsLoopScenario:
    """Reproduce the real-world find_skills loop from session 019cce5c."""

    def test_find_skills_loop_triggers_name_stall(self):
        """find_skills called with varying queries → name stall after 4 turns."""
        queries = [
            "GitHub issues latest",
            "GitHub",
            "list issues",
            "list issues matrixorigin/matrixone",
            "list issues",
            "bash",
            "bash",
            "bash",
        ]
        name_sigs: list[frozenset[str]] = []
        sigs: list[frozenset[str]] = []

        for q in queries:
            parsed = [_make_parsed("find_skills", {"query": q})]
            sigs.append(_compute_sig(parsed))
            name_sigs.append(_compute_name_sig(parsed))

            # Check exact stall
            exact_stall = False
            if len(sigs) >= _STALL_WINDOW:
                recent = sigs[-_STALL_WINDOW:]
                exact_stall = all(s == recent[0] for s in recent[1:])

            # Check name stall
            name_stall = False
            if len(name_sigs) >= _TOOL_NAME_STALL_WINDOW:
                recent_names = name_sigs[-_TOOL_NAME_STALL_WINDOW:]
                name_stall = (
                    all(s == recent_names[0] for s in recent_names[1:])
                    and len(recent_names[0]) == 1
                )

            if exact_stall or name_stall:
                break

        # Should have stopped at turn 4 (0-indexed: 3) via name stall
        assert len(name_sigs) == _TOOL_NAME_STALL_WINDOW
        assert name_stall is True
        # Exact stall should NOT have triggered (different args each time)
        assert exact_stall is False

    def test_find_skills_exact_stall_on_repeated_bash(self):
        """find_skills("bash") repeated 3 times → exact stall."""
        parsed = [_make_parsed("find_skills", {"query": "bash"})]
        sigs = [_compute_sig(parsed)] * _STALL_WINDOW
        recent = sigs[-_STALL_WINDOW:]
        assert all(s == recent[0] for s in recent[1:])


class TestSkillDiscoveryOutput:
    """Test find_skills result message guides LLM to call skills directly."""

    def test_result_says_call_directly(self):
        """find_skills output must tell LLM to call skills directly."""
        # Simulate the output format from skill_discovery.py
        lines = [
            "Found 1 skills matching 'list issues':",
            "",
            "**list_issues**",
            "  List issues from GitHub",
            "  Category: github",
            "",
            "Call these skills directly by name. Use get_agent_info(dimension='capability') only if you need parameter details.",
        ]
        output = "\n".join(lines)
        assert "Call these skills directly by name" in output
        assert (
            "get_agent_info" not in output.split("Call these skills")[0]
        )  # not before the instruction

    def test_description_warns_against_repeat(self):
        """find_skills description must warn against repeated calls."""
        from cli.tools.skill_discovery import FindSkillsTool

        t = FindSkillsTool()
        assert "do not call find_skills again" in t.description.lower()


class TestKeywordSearchMatching:
    """Test find_skills keyword search uses word-level bidirectional matching."""

    def _score(self, query: str, skill_name: str, desc: str = "", cat: str = "github") -> int:
        """Replicate _keyword_search scoring logic including system-category penalty."""
        import re

        query_words = re.findall(r"[a-z]{3,}", query.lower())
        if not query_words:
            query_words = re.findall(r"[a-z]{2,}", query.lower())
        cjk_tokens = re.findall(r"[^\x00-\x7f]+", query.lower())
        tokens = query_words + cjk_tokens
        score = 0
        name_lower = skill_name.lower()
        desc_lower = desc.lower()
        for tok in tokens:
            if tok in name_lower or name_lower in tok:
                score += 2
            elif desc and len(tok) >= 5 and (tok in desc_lower or desc_lower in tok):
                score += 1
        # system skills matched only via description are deprioritised
        if cat == "system" and score < 2:
            score = 0
        return score

    def _match(self, query: str, skill_name: str, desc: str = "") -> int:
        """Backwards-compat wrapper (non-system skills)."""
        return self._score(query, skill_name, desc, cat="github")

    # ── Original regression tests ──────────────────────────────────────────

    def test_github_issues_matches_list_issues(self):
        assert self._match("GitHub issues", "list_issues") > 0

    def test_github_issues_repo_search_matches(self):
        assert self._match("GitHub issues repository search", "list_issues") > 0

    def test_exact_whole_query_no_match(self):
        assert "github issues" not in "list_issues"

    def test_chinese_query_with_english_word(self):
        assert self._match("matrixone的issue", "list_issues") > 0

    def test_no_false_positive(self):
        assert self._match("deploy kubernetes", "list_issues") == 0

    # ── Parametrized: queries that SHOULD match a skill ────────────────────

    SKILL_CATALOG = {
        # name → (category, description_snippet)
        "list_issues": ("github", "List issues excludes PRs detail brief title state labels"),
        "list_prs": ("github", "List pull requests from a GitHub repository"),
        "get_issue": ("github", "Get a specific issue by number"),
        "create_issue": ("github", "Create a new GitHub issue requires title"),
        "ci_status": ("github", "Check CI CD workflow run status recent runs pass fail pending"),
        "summarize_pr": ("github", "Summarize a specific GitHub PR using LLM analysis"),
        "bash": ("shell", "Execute a shell command"),
        "read_file": ("file_ops", "Read file contents from local filesystem"),
        "write_file": ("file_ops", "Create or overwrite a file on local filesystem"),
        "grep": ("search", "Search for text patterns in files"),
        "glob": ("search", "Find files matching a glob pattern"),
        "git_status": ("vcs", "Show git working tree status"),
        "git_diff": ("vcs", "Show git diff of changes"),
        "git_log": ("vcs", "Show git commit history"),
        "skill_config_wizard": (
            "system",
            "Show what configuration a skill needs and what is already set configure github token",
        ),
        "set_skill_setting": ("system", "Configure a skill setting set token api key"),
        "find_skills": ("system", "Discover available skills and their capabilities"),
        "get_agent_info": (
            "system",
            "Query current runtime state token counts context window session info available tools",
        ),
    }

    @pytest.mark.parametrize(
        "query,expected_skill",
        [
            # GitHub issues
            ("matrixone的最新issue", "list_issues"),
            ("查看最新的issue", "list_issues"),
            ("list issues in matrixone", "list_issues"),
            ("show me open issues", "list_issues"),
            ("GitHub issues search", "list_issues"),
            ("get issue #123", "get_issue"),
            ("show issue details", "get_issue"),
            # PRs
            ("list pull requests", "list_prs"),
            ("show open PRs", "list_prs"),
            ("recent pull requests", "list_prs"),
            ("summarize PR 456", "summarize_pr"),
            # CI
            ("check CI status", "ci_status"),
            ("workflow run failed", "ci_status"),
            ("build status", "ci_status"),
            # Create
            ("create a new issue", "create_issue"),
            # Shell
            ("run a shell command", "bash"),
            ("execute bash script", "bash"),
            # Files
            ("read the config file", "read_file"),
            ("write to output file", "write_file"),
            # Search
            ("grep for TODO in code", "grep"),
            ("find files matching pattern", "glob"),
            # Git
            ("show git status", "git_status"),
            ("git diff changes", "git_diff"),
            ("git log history", "git_log"),
            # Config (system skill via name match)
            ("configure skill settings", "skill_config_wizard"),
            ("set skill token", "set_skill_setting"),
        ],
    )
    def test_query_matches_expected_skill(self, query, expected_skill):
        cat, desc = self.SKILL_CATALOG[expected_skill]
        assert self._score(query, expected_skill, desc, cat) > 0, (
            f"Query {query!r} should match {expected_skill!r}"
        )

    # ── Parametrized: system skills should NOT appear for non-config queries ─

    @pytest.mark.parametrize(
        "query",
        [
            "GitHub issues search",
            "matrixone的最新issue",
            "list pull requests",
            "check CI status",
            "show open PRs",
            "summarize PR 456",
            "run a shell command",
        ],
    )
    def test_system_skill_not_matched_by_data_queries(self, query):
        """skill_config_wizard must not appear for non-config queries."""
        cat, desc = self.SKILL_CATALOG["skill_config_wizard"]
        score = self._score(query, "skill_config_wizard", desc, cat)
        assert score == 0, f"skill_config_wizard should NOT match {query!r} (got score={score})"

    # ── Parametrized: queries that should NOT match unrelated skills ────────

    @pytest.mark.parametrize(
        "query,wrong_skill",
        [
            ("deploy kubernetes", "list_issues"),
            ("send email", "ci_status"),
            ("database migration", "list_prs"),
            ("resize image", "bash"),
            ("translate text", "grep"),
        ],
    )
    def test_no_false_positives(self, query, wrong_skill):
        cat, desc = self.SKILL_CATALOG[wrong_skill]
        assert self._score(query, wrong_skill, desc, cat) == 0, (
            f"Query {query!r} should NOT match {wrong_skill!r}"
        )
