"""Tests for context-aware pre-filtering: SkillTags, ConversationState, pre_filter().

Covers:
- SkillTags validation, serialization, category inference
- ConversationState extraction from messages
- Pre-filter reordering logic with all rules
- Database persistence of tags via SkillCatalog
"""

from dataclasses import dataclass

import pytest

from core.skills.prefilter import (
    ConversationState,
    SkillTags,
    pre_filter,
    validate_tags,
)

# ── SkillTags Tests ──────────────────────────────────────────────


class TestSkillTagsValidation:
    """Registration-time validation of skill tags."""

    def test_valid_tags(self):
        tags = SkillTags.from_dict({
            "scope": "external",
            "data_source": "external_api",
            "intent_type": ["fetch", "mutate"],
            "requires_history": False,
        })
        assert tags.scope == "external"
        assert tags.data_source == "external_api"
        assert tags.intent_type == ("fetch", "mutate")  # sorted tuple
        assert tags.requires_history is False

    def test_invalid_scope_raises(self):
        with pytest.raises(ValueError, match="Invalid scope"):
            SkillTags.from_dict({
                "scope": "invalid_value",
                "data_source": "external_api",
                "intent_type": ["fetch"],
            })

    def test_invalid_data_source_raises(self):
        with pytest.raises(ValueError, match="Invalid data_source"):
            SkillTags.from_dict({
                "scope": "external",
                "data_source": "unknown_source",
                "intent_type": ["fetch"],
            })

    def test_invalid_intent_type_raises(self):
        with pytest.raises(ValueError, match="Invalid intent_type"):
            SkillTags.from_dict({
                "scope": "external",
                "data_source": "external_api",
                "intent_type": ["fetch", "nonexistent"],
            })

    def test_empty_scope_raises(self):
        with pytest.raises(ValueError, match="Invalid scope"):
            SkillTags.from_dict({
                "scope": "",
                "data_source": "external_api",
                "intent_type": [],
            })

    def test_empty_intent_type_is_valid(self):
        tags = SkillTags.from_dict({
            "scope": "external",
            "data_source": "external_api",
            "intent_type": [],
            "requires_history": True,
        })
        assert tags.intent_type == ()
        assert tags.requires_history is True

    def test_requires_history_defaults_false(self):
        tags = SkillTags.from_dict({
            "scope": "historical",
            "data_source": "event_store",
            "intent_type": ["analytical"],
        })
        assert tags.requires_history is False

    def test_validate_tags_function(self):
        tags = validate_tags({
            "scope": "current_session",
            "data_source": "session_metadata",
            "intent_type": ["introspect"],
        })
        assert isinstance(tags, SkillTags)
        assert tags.scope == "current_session"

    def test_validate_tags_rejects_invalid(self):
        with pytest.raises(ValueError):
            validate_tags({"scope": "bad"})


class TestSkillTagsSerialization:
    """Round-trip serialization of SkillTags."""

    def test_to_dict(self):
        tags = SkillTags(
            scope="historical",
            data_source="event_store",
            intent_type=("analytical",),
            requires_history=True,
        )
        d = tags.to_dict()
        assert d == {
            "scope": "historical",
            "data_source": "event_store",
            "intent_type": ["analytical"],
            "requires_history": True,
        }

    def test_roundtrip(self):
        original = SkillTags(
            scope="cross_session",
            data_source="memory_store",
            intent_type=("analytical", "fetch"),
            requires_history=True,
        )
        restored = SkillTags.from_dict(original.to_dict())
        assert restored == original

    def test_intent_type_sorted_on_creation(self):
        tags = SkillTags.from_dict({
            "scope": "external",
            "data_source": "external_api",
            "intent_type": ["mutate", "fetch", "analytical"],
        })
        assert tags.intent_type == ("analytical", "fetch", "mutate")

    def test_frozen_immutable(self):
        tags = SkillTags(scope="external", data_source="external_api",
                         intent_type=("fetch",), requires_history=False)
        with pytest.raises(AttributeError):
            tags.scope = "historical"


class TestSkillTagsCategoryInference:
    """Default tag inference from skill category."""

    def test_github_category(self):
        tags = SkillTags.infer_from_category("github")
        assert tags is not None
        assert tags.scope == "external"
        assert tags.data_source == "external_api"
        assert "fetch" in tags.intent_type
        assert tags.requires_history is False

    def test_system_category(self):
        tags = SkillTags.infer_from_category("system")
        assert tags is not None
        assert tags.scope == "current_session"
        assert tags.data_source == "session_metadata"

    def test_code_execution_category(self):
        tags = SkillTags.infer_from_category("code_execution")
        assert tags is not None
        assert tags.scope == "current_session"

    def test_unknown_category_returns_none(self):
        assert SkillTags.infer_from_category("unknown_category") is None
        assert SkillTags.infer_from_category("") is None


# ── ConversationState Tests ──────────────────────────────────────


class TestConversationStateExtraction:
    """Signal extraction from message history."""

    def test_chinese_history_markers(self):
        msgs = [{"role": "user", "content": "分析一下前一个上下文的情况"}]
        state = ConversationState.from_messages(msgs)
        assert state.references_history is True
        assert state.is_analytical is True

    def test_chinese_previous_turn(self):
        msgs = [{"role": "user", "content": "上一轮的决策链评估"}]
        state = ConversationState.from_messages(msgs)
        assert state.references_history is True
        assert state.is_analytical is True

    def test_english_history_markers(self):
        msgs = [{"role": "user", "content": "analyze the previous context"}]
        state = ConversationState.from_messages(msgs)
        assert state.references_history is True
        assert state.is_analytical is True

    def test_fetch_intent(self):
        msgs = [{"role": "user", "content": "show me the latest PRs"}]
        state = ConversationState.from_messages(msgs)
        assert state.is_fetch is True
        assert state.references_history is False

    def test_mutate_intent(self):
        msgs = [{"role": "user", "content": "create a new issue for this bug"}]
        state = ConversationState.from_messages(msgs)
        assert state.is_mutate is True

    def test_chinese_fetch(self):
        msgs = [{"role": "user", "content": "查看最新的PR情况"}]
        state = ConversationState.from_messages(msgs)
        assert state.is_fetch is True

    def test_chinese_mutate(self):
        msgs = [{"role": "user", "content": "创建一个新的issue"}]
        state = ConversationState.from_messages(msgs)
        assert state.is_mutate is True

    def test_no_signals(self):
        msgs = [{"role": "user", "content": "hello world"}]
        state = ConversationState.from_messages(msgs)
        assert state.references_history is False
        assert state.is_analytical is False
        assert state.is_fetch is False
        assert state.is_mutate is False

    def test_empty_messages(self):
        state = ConversationState.from_messages([])
        assert state.references_history is False
        assert state.turn_count == 0
        assert state.previous_skill is None

    def test_turn_count(self):
        msgs = [
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "reply"},
            {"role": "user", "content": "second"},
        ]
        state = ConversationState.from_messages(msgs)
        assert state.turn_count == 2

    def test_has_tool_results(self):
        msgs = [
            {"role": "user", "content": "query"},
            {"role": "tool", "content": "result"},
            {"role": "user", "content": "分析之前的结果"},
        ]
        state = ConversationState.from_messages(msgs)
        assert state.has_tool_results is True

    def test_no_tool_results(self):
        msgs = [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"},
        ]
        state = ConversationState.from_messages(msgs)
        assert state.has_tool_results is False

    def test_previous_skill_extraction(self):
        msgs = [
            {"role": "user", "content": "list prs"},
            {"role": "assistant", "content": "here are prs", "tool_calls": [
                {"function": {"name": "list_prs", "arguments": "{}"}}
            ]},
            {"role": "user", "content": "分析一下前一个上下文"},
        ]
        state = ConversationState.from_messages(msgs)
        assert state.previous_skill == "list_prs"

    def test_previous_skill_none_when_no_tool_calls(self):
        msgs = [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"},
            {"role": "user", "content": "what?"},
        ]
        state = ConversationState.from_messages(msgs)
        assert state.previous_skill is None

    def test_uses_last_user_message(self):
        msgs = [
            {"role": "user", "content": "create an issue"},  # mutate
            {"role": "assistant", "content": "done"},
            {"role": "user", "content": "show me the latest PRs"},  # fetch, not mutate
        ]
        state = ConversationState.from_messages(msgs)
        assert state.is_fetch is True
        assert state.is_mutate is False  # only last user message matters

    def test_non_string_content_handled(self):
        msgs = [{"role": "user", "content": None}]
        state = ConversationState.from_messages(msgs)
        assert state.references_history is False

    def test_to_dict(self):
        state = ConversationState(
            references_history=True,
            is_analytical=True,
            turn_count=3,
            previous_skill="list_prs",
        )
        d = state.to_dict()
        assert d["references_history"] is True
        assert d["is_analytical"] is True
        assert d["is_fetch"] is False
        assert d["is_mutate"] is False
        assert d["turn_count"] == 3
        assert d["has_tool_results"] is False
        assert d["previous_skill"] == "list_prs"

    def test_frozen_immutable(self):
        state = ConversationState()
        with pytest.raises(AttributeError):
            state.references_history = True


# ── Pre-filter Logic Tests ───────────────────────────────────────


@dataclass
class MockSkill:
    """Minimal skill mock for pre-filter tests."""
    name: str
    tags: SkillTags | None = None


class TestPreFilterRules:
    """Pre-filter reordering logic."""

    def test_history_analytical_prefers_historical(self):
        """Rule 1: history + analytical → prefer historical scope."""
        introspection = MockSkill("introspection", SkillTags(
            scope="current_session", data_source="session_metadata",
            intent_type=("introspect",), requires_history=False))
        event_reader = MockSkill("event_reader", SkillTags(
            scope="historical", data_source="event_store",
            intent_type=("analytical",), requires_history=True))

        state = ConversationState(references_history=True, is_analytical=True)
        result, applied = pre_filter([introspection, event_reader], state)

        assert applied is True
        assert result[0].name == "event_reader"
        assert result[1].name == "introspection"
        assert len(result) == 2  # never removes

    def test_history_analytical_cross_session_also_preferred(self):
        """Rule 1: cross_session scope is also preferred for history+analytical."""
        current = MockSkill("current", SkillTags(
            scope="current_session", data_source="session_metadata",
            intent_type=("introspect",), requires_history=False))
        memory = MockSkill("memory", SkillTags(
            scope="cross_session", data_source="memory_store",
            intent_type=("analytical",), requires_history=True))

        state = ConversationState(references_history=True, is_analytical=True)
        result, applied = pre_filter([current, memory], state)

        assert applied is True
        assert result[0].name == "memory"

    def test_fetch_prefers_external(self):
        """Rule 2: fetch without history → prefer external scope."""
        local = MockSkill("local", SkillTags(
            scope="current_session", data_source="session_metadata",
            intent_type=("introspect",), requires_history=False))
        github = MockSkill("github", SkillTags(
            scope="external", data_source="external_api",
            intent_type=("fetch",), requires_history=False))

        state = ConversationState(is_fetch=True)
        result, applied = pre_filter([local, github], state)

        assert applied is True
        assert result[0].name == "github"

    def test_fetch_with_history_does_not_trigger_rule2(self):
        """Rule 2 requires no history reference."""
        local = MockSkill("local", SkillTags(
            scope="current_session", data_source="session_metadata",
            intent_type=("introspect",), requires_history=False))
        github = MockSkill("github", SkillTags(
            scope="external", data_source="external_api",
            intent_type=("fetch",), requires_history=False))

        state = ConversationState(is_fetch=True, references_history=True, is_analytical=True)
        # Rule 1 fires (history+analytical), not Rule 2
        _result, applied = pre_filter([local, github], state)
        # local is deprioritized by Rule 1, github goes to normal bucket
        assert applied is True

    def test_mutate_prefers_mutate_intent(self):
        """Rule 3: mutate → prefer skills with mutate intent."""
        reader = MockSkill("reader", SkillTags(
            scope="external", data_source="external_api",
            intent_type=("fetch",), requires_history=False))
        creator = MockSkill("creator", SkillTags(
            scope="external", data_source="external_api",
            intent_type=("mutate",), requires_history=False))

        state = ConversationState(is_mutate=True)
        result, applied = pre_filter([reader, creator], state)

        assert applied is True
        assert result[0].name == "creator"

    def test_no_signals_no_filtering(self):
        """No signals → pass through unchanged."""
        a = MockSkill("a", SkillTags(
            scope="external", data_source="external_api",
            intent_type=("fetch",), requires_history=False))
        b = MockSkill("b", SkillTags(
            scope="current_session", data_source="session_metadata",
            intent_type=("introspect",), requires_history=False))

        state = ConversationState()
        result, applied = pre_filter([a, b], state)

        assert applied is False
        assert result[0] is a
        assert result[1] is b

    def test_none_state_no_filtering(self):
        a = MockSkill("a")
        result, applied = pre_filter([a], None)
        assert applied is False
        assert result[0] is a

    def test_empty_skills_no_crash(self):
        state = ConversationState(references_history=True, is_analytical=True)
        result, applied = pre_filter([], state)
        assert result == []
        assert applied is False

    def test_skills_without_tags_go_to_normal_bucket(self):
        """Untagged skills are not removed or deprioritized."""
        tagged = MockSkill("tagged", SkillTags(
            scope="historical", data_source="event_store",
            intent_type=("analytical",), requires_history=True))
        untagged = MockSkill("untagged", tags=None)

        state = ConversationState(references_history=True, is_analytical=True)
        result, applied = pre_filter([untagged, tagged], state)

        assert applied is True
        assert result[0].name == "tagged"  # preferred
        assert result[1].name == "untagged"  # normal (not removed)
        assert len(result) == 2

    def test_all_same_scope_no_reorder(self):
        """If all skills have the same scope, no reordering happens."""
        a = MockSkill("a", SkillTags(
            scope="external", data_source="external_api",
            intent_type=("fetch",), requires_history=False))
        b = MockSkill("b", SkillTags(
            scope="external", data_source="external_api",
            intent_type=("fetch",), requires_history=False))

        state = ConversationState(is_fetch=True)
        _result, applied = pre_filter([a, b], state)
        # Both are preferred, order unchanged
        assert applied is False  # order didn't change

    def test_preserves_all_skills(self):
        """Pre-filter never removes skills, only reorders."""
        skills = [
            MockSkill("a", SkillTags(scope="current_session", data_source="session_metadata",
                                     intent_type=("introspect",), requires_history=False)),
            MockSkill("b", SkillTags(scope="historical", data_source="event_store",
                                     intent_type=("analytical",), requires_history=True)),
            MockSkill("c", SkillTags(scope="external", data_source="external_api",
                                     intent_type=("fetch",), requires_history=False)),
            MockSkill("d", tags=None),
        ]
        state = ConversationState(references_history=True, is_analytical=True)
        result, _applied = pre_filter(skills, state)

        assert len(result) == 4
        result_names = {s.name for s in result}
        assert result_names == {"a", "b", "c", "d"}


# ── Real Failure Case Test ───────────────────────────────────────


class TestMultiTurnContinuity:
    """Rule 0: previous_skill boosted to front for multi-turn follow-ups."""

    def test_previous_skill_boosted_to_front(self):
        """When previous_skill exists and turn_count > 1, boost it."""
        find_skills = MockSkill("find_skills", SkillTags(
            scope="current_session", data_source="session_metadata",
            intent_type=("introspect",), requires_history=False))
        list_prs = MockSkill("list_prs", SkillTags(
            scope="external", data_source="external_api",
            intent_type=("fetch",), requires_history=False))
        read_file = MockSkill("read_file", SkillTags(
            scope="local", data_source="local_filesystem",
            intent_type=("fetch",), requires_history=False))

        state = ConversationState(previous_skill="list_prs", turn_count=2)
        result, applied = pre_filter([find_skills, list_prs, read_file], state)

        assert applied is True
        assert result[0].name == "list_prs"
        assert len(result) == 3  # never removes

    def test_previous_skill_already_first_no_reorder(self):
        """If previous_skill is already first, no reorder needed."""
        list_prs = MockSkill("list_prs", SkillTags(
            scope="external", data_source="external_api",
            intent_type=("fetch",), requires_history=False))
        other = MockSkill("other", tags=None)

        state = ConversationState(previous_skill="list_prs", turn_count=2)
        result, applied = pre_filter([list_prs, other], state)

        # Already first — no change
        assert applied is False

    def test_turn_1_no_continuity(self):
        """On turn 1, previous_skill should not trigger continuity."""
        a = MockSkill("a", tags=None)
        b = MockSkill("b", tags=None)

        state = ConversationState(previous_skill="a", turn_count=1)
        result, applied = pre_filter([b, a], state)

        assert applied is False

    def test_previous_skill_not_in_list(self):
        """If previous_skill is not in the skill list, no crash."""
        a = MockSkill("a", tags=None)
        b = MockSkill("b", tags=None)

        state = ConversationState(previous_skill="nonexistent", turn_count=2)
        result, applied = pre_filter([a, b], state)

        assert applied is False

    def test_reproduces_session_019cbc98(self):
        """Reproduce: 'tidb呢' after list_prs on matrixone.

        Turn 1: user asks 'matrixone 最新的两个pr情况？' → list_prs succeeds.
        Turn 2: user asks 'tidb呢' → should still prefer list_prs.
        """
        list_prs = MockSkill("list_prs", SkillTags(
            scope="external", data_source="external_api",
            intent_type=("fetch",), requires_history=False))
        find_skills = MockSkill("find_skills", SkillTags(
            scope="current_session", data_source="session_metadata",
            intent_type=("introspect",), requires_history=False))

        # Simulate full server-side history
        history = [
            {"role": "system", "content": "You are..."},
            {"role": "user", "content": "matrixone 最新的两个pr情况？"},
            {"role": "assistant", "content": "...", "tool_calls": [
                {"function": {"name": "list_prs", "arguments": "{}"}}
            ]},
            {"role": "tool", "content": "...", "tool_call_id": "x"},
            {"role": "assistant", "content": "根据查询结果..."},
            {"role": "user", "content": "tidb呢"},
        ]
        state = ConversationState.from_messages(history)

        assert state.previous_skill == "list_prs"
        assert state.turn_count == 2

        result, applied = pre_filter([find_skills, list_prs], state)

        assert applied is True
        assert result[0].name == "list_prs"


class TestRealFailureCase:
    """Reproduce the actual failure from session 019cbb9e."""

    def test_session_019cbb9e_disambiguation(self):
        """User asked '分析一下前一个上下文的情况还有决策链评估'.
        introspection was selected but event_reader was correct."""
        introspection = MockSkill("introspection", SkillTags(
            scope="current_session", data_source="session_metadata",
            intent_type=("introspect",), requires_history=False))
        event_reader = MockSkill("event_reader", SkillTags(
            scope="historical", data_source="event_store",
            intent_type=("analytical",), requires_history=True))

        msgs = [
            {"role": "user", "content": "matrixone 最新的两个pr情况？"},
            {"role": "assistant", "content": "...", "tool_calls": [
                {"function": {"name": "list_prs", "arguments": "{}"}}
            ]},
            {"role": "user", "content": "分析一下前一个上下文的情况还有决策链评估"},
        ]
        state = ConversationState.from_messages(msgs)

        assert state.references_history is True
        assert state.is_analytical is True
        assert state.previous_skill == "list_prs"
        assert state.turn_count == 2

        result, applied = pre_filter([introspection, event_reader], state)

        assert applied is True
        assert result[0].name == "event_reader"
        assert result[1].name == "introspection"


# ── Keyword Fallback Tests ───────────────────────────────────────

class TestKeywordFallback:
    """When LLM tool selection fails, keyword matching picks the right tool."""

    @pytest.fixture(autouse=True)
    def _setup_env(self, monkeypatch):
        monkeypatch.setenv("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
        monkeypatch.setenv("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)

    @pytest.fixture
    def tools(self):
        return [
            {"type": "function", "function": {"name": "list_prs", "description": "x", "parameters": {}}},
            {"type": "function", "function": {"name": "ci_status", "description": "x", "parameters": {}}},
            {"type": "function", "function": {"name": "list_issues", "description": "x", "parameters": {}}},
            {"type": "function", "function": {"name": "summarize_pr", "description": "x", "parameters": {}}},
            {"type": "function", "function": {"name": "create_issue", "description": "x", "parameters": {}}},
            {"type": "function", "function": {"name": "execute_code", "description": "x", "parameters": {}}},
            {"type": "function", "function": {"name": "bash", "description": "x", "parameters": {}}},
        ]

    def test_pr_keyword_matches(self, tools):
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "matrixone 最新的两个pr情况？", "test")
        assert r.selected_tool == "list_prs"
        assert r.fallback_reason == "test"

    def test_ci_keyword_matches(self, tools):
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "ci怎么样", "test")
        assert r.selected_tool == "ci_status"

    def test_issue_keyword_matches(self, tools):
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "查看issue", "test")
        assert r.selected_tool == "list_issues"

    def test_no_match_returns_all(self, tools):
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "ragflow?", "test")
        assert r.selected_tool is None
        assert len(r.tools) == len(tools)
        assert r.fallback_reason == "test"

    def test_reproduces_session_019cbcd3(self, tools):
        """Session 019cbcd3: 'matrixone 最新的两个pr情况？' with failed LLM selection."""
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "matrixone 最新的两个pr情况？", "llm_error:PermissionError")
        assert r.selected_tool == "list_prs"
        assert len(r.tools) == 1

    # ── Multi-word patterns must not be shadowed by single-word ──

    def test_summarize_pr_not_shadowed_by_list_prs(self, tools):
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "summarize pr #42", "test")
        assert r.selected_tool == "summarize_pr"

    def test_review_pr_not_shadowed(self, tools):
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "review pr changes", "test")
        assert r.selected_tool == "summarize_pr"

    def test_create_issue_not_shadowed_by_list_issues(self, tools):
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "create issue about login", "test")
        assert r.selected_tool == "create_issue"

    # ── False-positive resistance ────────────────────────────────

    def test_debug_does_not_match_list_issues(self, tools):
        """'bug' was removed — 'debug' must not trigger list_issues."""
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "debug this code", "test")
        assert r.selected_tool is None

    def test_generic_question_does_not_match_list_issues(self, tools):
        """'问题' was removed — generic Chinese 'question' must not trigger list_issues."""
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "这个问题怎么解决", "test")
        assert r.selected_tool is None

    def test_execute_sql_does_not_match_execute_code(self, tools):
        """'execute' single-word was removed — 'execute SQL' must not trigger execute_code."""
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "execute this SQL query", "test")
        assert r.selected_tool is None

    def test_run_code_multiword_still_matches(self, tools):
        """Multi-word 'run code' still works even though single-word 'execute' was removed."""
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "run code to parse the file", "test")
        assert r.selected_tool == "execute_code"


    # ── Previous skill fallback (Stage 2) ────────────────────────

    def test_previous_skill_fallback_on_bare_followup(self, tools):
        """'ragflow?' has no keyword match → falls back to previous_skill."""
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "ragflow?", "llm_error:X",
                              previous_skill="list_prs", turn_count=2)
        assert r.selected_tool == "list_prs"
        assert "prev_skill" in r.fallback_reason

    def test_previous_skill_blocked_on_turn_1(self, tools):
        """Turn 1 must NOT use previous_skill — no conversation context yet."""
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "ragflow?", "llm_error:X",
                              previous_skill="list_prs", turn_count=1)
        assert r.selected_tool is None
        assert len(r.tools) == len(tools)

    def test_previous_skill_not_in_tools_returns_all(self, tools):
        """previous_skill not in available tools → all tools."""
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "ragflow?", "test",
                              previous_skill="nonexistent_tool", turn_count=2)
        assert r.selected_tool is None
        assert len(r.tools) == len(tools)

    def test_keyword_match_takes_priority_over_previous_skill(self, tools):
        """Keyword match (Stage 1) wins over previous_skill (Stage 2)."""
        from api.routers.chat import _keyword_fallback
        r = _keyword_fallback(tools, "查看ci状态", "test",
                              previous_skill="list_prs", turn_count=2)
        assert r.selected_tool == "ci_status"  # keyword wins, not previous_skill


class TestSelectToolsFallbackChain:
    """End-to-end tests for the full fallback chain in select_tools_for_turn.

    Verifies the 4-stage cascade:
      Stage 0: pre-filter reorder (tested in TestPreFilterRules)
      Stage 1: LLM selection
      Stage 2: keyword fallback
      Stage 3: previous_skill fallback
      Stage 4: all tools (last resort)
    """

    @pytest.fixture(autouse=True)
    def _setup_env(self, monkeypatch):
        monkeypatch.setenv("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
        monkeypatch.setenv("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)

    @pytest.fixture
    def tools(self):
        return [
            {"type": "function", "function": {"name": "list_prs", "description": "List PRs", "parameters": {}}},
            {"type": "function", "function": {"name": "ci_status", "description": "CI status", "parameters": {}}},
            {"type": "function", "function": {"name": "bash", "description": "Run bash", "parameters": {}}},
        ]

    @pytest.fixture
    def history_with_list_prs(self):
        """Session history where Turn 1 used list_prs."""
        return [
            {"role": "user", "content": "matrixone pr"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"function": {"name": "list_prs", "arguments": "{}"}}
            ]},
            {"role": "tool", "content": "result"},
            {"role": "assistant", "content": "done"},
            {"role": "user", "content": "ragflow?"},
        ]

    def test_llm_unavailable_keyword_match(self, tools):
        """LLM=None + keyword present → keyword fallback picks tool."""
        from api.routers.chat import select_tools_for_turn
        msgs = [{"role": "user", "content": "查看pr"}]
        r = select_tools_for_turn(tools, msgs, None, "u1", None)
        assert r.selected_tool == "list_prs"
        assert "llm_error" in r.fallback_reason

    def test_llm_unavailable_no_keyword_with_history(self, tools, history_with_list_prs):
        """LLM=None + no keyword + previous_skill → previous_skill fallback."""
        from api.routers.chat import select_tools_for_turn
        msgs = [{"role": "user", "content": "ragflow?"}]
        r = select_tools_for_turn(tools, msgs, None, "u1", None,
                                  session_history=history_with_list_prs)
        assert r.selected_tool == "list_prs"
        assert "prev_skill" in r.fallback_reason

    def test_llm_unavailable_no_keyword_no_history(self, tools):
        """LLM=None + no keyword + no history → all tools."""
        from api.routers.chat import select_tools_for_turn
        msgs = [{"role": "user", "content": "ragflow?"}]
        r = select_tools_for_turn(tools, msgs, None, "u1", None)
        assert r.selected_tool is None
        assert len(r.tools) == len(tools)

    def test_llm_unavailable_turn1_no_previous_skill(self, tools):
        """Turn 1 with LLM=None + no keyword → all tools (no false positive)."""
        from api.routers.chat import select_tools_for_turn
        history = [{"role": "user", "content": "ragflow?"}]
        r = select_tools_for_turn(tools, history, None, "u1", None,
                                  session_history=history)
        assert r.selected_tool is None
        assert len(r.tools) == len(tools)

    def test_reproduces_session_019cbcdc_ragflow(self, tools, history_with_list_prs):
        """Regression: session 019cbcdc Turn 2 'ragflow?' must select list_prs."""
        from api.routers.chat import select_tools_for_turn
        r = select_tools_for_turn(
            tools,
            [{"role": "user", "content": "ragflow?"}],
            None, "u1", None,
            session_history=history_with_list_prs,
        )
        assert r.selected_tool == "list_prs"


class TestKeepActiveToolsEdgeCases:
    """Edge cases for _keep_active_tools."""

    @pytest.fixture(autouse=True)
    def _setup_env(self, monkeypatch):
        monkeypatch.setenv("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
        monkeypatch.setenv("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)

    def test_no_used_names_returns_all(self):
        """tool_results with no extractable names → all tools returned."""
        from api.routers.chat import _keep_active_tools
        tools = [
            {"function": {"name": "a", "description": "A"}},
            {"function": {"name": "b", "description": "B"}},
        ]
        result = _keep_active_tools(tools, [{"result": "plain text, not json"}])
        assert len(result.tools) == 2

    def test_json_parse_failure_skipped(self):
        """Malformed JSON in result doesn't crash, name is skipped."""
        from api.routers.chat import _keep_active_tools
        tools = [
            {"function": {"name": "a", "description": "A"}},
            {"function": {"name": "b", "description": "B"}},
        ]
        result = _keep_active_tools(tools, [{"result": "{invalid json"}])
        assert len(result.tools) == 2  # No names extracted → all tools


class TestSelectToolsNoQueryNoResults:
    """select_tools_for_turn with no user query and no tool results."""

    @pytest.fixture(autouse=True)
    def _setup_env(self, monkeypatch):
        monkeypatch.setenv("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
        monkeypatch.setenv("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)

    def test_no_user_query_no_tool_results_returns_all(self):
        """Messages with no user role and no tool_results → all tools."""
        from api.routers.chat import select_tools_for_turn
        tools = [
            {"function": {"name": "a", "description": "A"}},
            {"function": {"name": "b", "description": "B"}},
        ]
        # Only assistant message, no user, no tool_results
        messages = [{"role": "assistant", "content": "thinking..."}]
        result = select_tools_for_turn(tools, messages, None, "u1", None)
        assert len(result.tools) == 2
        assert result.selected_tool is None
