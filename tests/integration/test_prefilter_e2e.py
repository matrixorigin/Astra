"""Integration tests for pre-filter: DB persistence and pipeline integration.

Tests tags persistence in skills_registry via SkillCatalog,
tags loading via SkillSelector, and pre-filter in SkillPipeline.
"""

import json

import pytest
from uuid_utils import uuid7

from api.models.skill import SkillRegistry as SkillModel
from core.skills.prefilter import ConversationState


@pytest.fixture(autouse=True)
def _clean_test_skills(db_session):
    """Remove test skills before and after each test.

    All test skill names start with known prefixes (test_, pf_, e2e_).
    Guarantees cleanup even if the test fails mid-way.
    """
    _TEST_PREFIXES = ("test_", "pf_", "e2e_")

    def _purge():
        for prefix in _TEST_PREFIXES:
            db_session.query(SkillModel).filter(
                SkillModel.skill_name.like(f"{prefix}%")
            ).delete(synchronize_session="fetch")
        db_session.commit()

    _purge()
    yield
    _purge()


# ── DB Persistence Tests ─────────────────────────────────────────


class TestTagsPersistence:
    """Tags stored and loaded correctly from skills_registry."""

    def test_register_skill_with_tags_persists_all_fields(self, db_factory, db_session):
        """Register a skill with tags → verify every field in DB."""
        from core.skills.base import Skill, SkillInput, SkillOutput, SkillRequirement
        from core.skills.catalog import SkillCatalog

        class DummySkill(Skill[SkillInput, SkillOutput]):
            name = "test_tagged_skill"
            version = "1.0.0"
            description = "A test skill with tags"
            requirements = SkillRequirement()

            async def execute(self, input):
                pass

        catalog = SkillCatalog(db_factory)
        tags_dict = {
            "scope": "historical",
            "data_source": "event_store",
            "intent_type": ["analytical"],
            "requires_history": True,
        }

        catalog.register(
            skill=DummySkill(),
            category="analysis",
            subcategory="events",
            triggers=["analyze"],
            tags=tags_dict,
        )

        # Re-query from DB — don't trust return value
        saved = db_session.query(SkillModel).filter(
            SkillModel.skill_name == "test_tagged_skill",
            SkillModel.is_active == 1,
        ).first()

        assert saved is not None
        assert saved.tags is not None
        assert saved.tags["scope"] == "historical"
        assert saved.tags["data_source"] == "event_store"
        assert saved.tags["intent_type"] == ["analytical"]
        assert saved.tags["requires_history"] is True

        # Verify other fields not corrupted
        assert saved.skill_name == "test_tagged_skill"
        assert saved.version == "1.0.0"
        assert saved.description == "A test skill with tags"
        assert saved.category == "analysis"
        assert saved.subcategory == "events"
        assert saved.is_active == 1
    def test_register_skill_without_tags_stores_null(self, db_factory, db_session):
        """Register without tags → tags column is NULL."""
        from core.skills.base import Skill, SkillInput, SkillOutput, SkillRequirement
        from core.skills.catalog import SkillCatalog

        class NoTagSkill(Skill[SkillInput, SkillOutput]):
            name = "test_no_tag_skill"
            version = "1.0.0"
            description = "No tags"
            requirements = SkillRequirement()

            async def execute(self, input):
                pass

        catalog = SkillCatalog(db_factory)
        catalog.register(skill=NoTagSkill(), category="general")

        saved = db_session.query(SkillModel).filter(
            SkillModel.skill_name == "test_no_tag_skill",
            SkillModel.is_active == 1,
        ).first()

        assert saved is not None
        assert saved.tags is None
    def test_register_with_invalid_tags_raises(self, db_factory):
        """Invalid tags rejected at registration time."""
        from core.skills.base import Skill, SkillInput, SkillOutput, SkillRequirement
        from core.skills.catalog import SkillCatalog

        class BadTagSkill(Skill[SkillInput, SkillOutput]):
            name = "test_bad_tag_skill"
            version = "1.0.0"
            description = "Bad tags"
            requirements = SkillRequirement()

            async def execute(self, input):
                pass

        catalog = SkillCatalog(db_factory)
        with pytest.raises(ValueError, match="Invalid scope"):
            catalog.register(
                skill=BadTagSkill(),
                tags={"scope": "invalid", "data_source": "external_api", "intent_type": []},
            )

    def test_update_skill_preserves_tags(self, db_factory, db_session):
        """Re-registering same skill updates tags."""
        from core.skills.base import Skill, SkillInput, SkillOutput, SkillRequirement
        from core.skills.catalog import SkillCatalog

        class UpdatableSkill(Skill[SkillInput, SkillOutput]):
            name = "test_update_tags_skill"
            version = "1.0.0"
            description = "Updatable"
            requirements = SkillRequirement()

            async def execute(self, input):
                pass

        catalog = SkillCatalog(db_factory)

        # First registration
        catalog.register(
            skill=UpdatableSkill(),
            tags={"scope": "external", "data_source": "external_api", "intent_type": ["fetch"]},
        )

        # Second registration with different tags
        catalog.register(
            skill=UpdatableSkill(),
            tags={
                "scope": "historical",
                "data_source": "event_store",
                "intent_type": ["analytical"],
            },
        )

        saved = db_session.query(SkillModel).filter(
            SkillModel.skill_name == "test_update_tags_skill",
            SkillModel.is_active == 1,
        ).first()

        assert saved.tags["scope"] == "historical"
        assert saved.tags["data_source"] == "event_store"
# ── SkillSelector Tags Loading Tests ─────────────────────────────


class TestSelectorTagsLoading:
    """SkillSelector loads tags from DB into SkillMetadata."""

    def test_loads_explicit_tags(self, db_factory, db_session):
        """Tags from DB are loaded into SkillMetadata.tags."""
        # Insert a skill with tags directly
        skill_id = "test_load_tags@1.0.0"
        db_session.query(SkillModel).filter(SkillModel.skill_id == skill_id).delete()
        db_session.add(SkillModel(
            skill_id=skill_id,
            skill_name="test_load_tags",
            version="1.0.0",
            description="Test",
            is_active=1,
            status="active",
            category="analysis",
            tags={"scope": "historical", "data_source": "event_store",
                  "intent_type": ["analytical"], "requires_history": True},
        ))
        db_session.commit()

        from core.skills.selector import SkillSelector
        selector = SkillSelector(db_factory)
        skill = selector.skills.get("test_load_tags")

        assert skill is not None
        assert skill.tags is not None
        assert skill.tags.scope == "historical"
        assert skill.tags.data_source == "event_store"
        assert skill.tags.intent_type == ("analytical",)
        assert skill.tags.requires_history is True
    def test_infers_tags_from_category_when_null(self, db_factory, db_session):
        """When tags is NULL, infer from category."""
        skill_id = "test_infer_tags@1.0.0"
        db_session.query(SkillModel).filter(SkillModel.skill_id == skill_id).delete()
        db_session.add(SkillModel(
            skill_id=skill_id,
            skill_name="test_infer_tags",
            version="1.0.0",
            description="Test",
            is_active=1,
            status="active",
            category="github",
            tags=None,  # No explicit tags
        ))
        db_session.commit()

        from core.skills.selector import SkillSelector
        selector = SkillSelector(db_factory)
        skill = selector.skills.get("test_infer_tags")

        assert skill is not None
        assert skill.tags is not None
        assert skill.tags.scope == "external"
        assert skill.tags.data_source == "external_api"
    def test_unknown_category_no_tags(self, db_factory, db_session):
        """Unknown category + no explicit tags → tags is None."""
        skill_id = "test_no_infer@1.0.0"
        db_session.query(SkillModel).filter(SkillModel.skill_id == skill_id).delete()
        db_session.add(SkillModel(
            skill_id=skill_id,
            skill_name="test_no_infer",
            version="1.0.0",
            description="Test",
            is_active=1,
            status="active",
            category="custom_unknown",
            tags=None,
        ))
        db_session.commit()

        from core.skills.selector import SkillSelector
        selector = SkillSelector(db_factory)
        skill = selector.skills.get("test_no_infer")

        assert skill is not None
        assert skill.tags is None
# ── Pipeline Integration Tests ───────────────────────────────────


class TestPipelinePreFilter:
    """Pre-filter integration in SkillPipeline.get_tools_schema().

    Test skills must have triggers that match the query so keyword retrieval
    finds them.  Pre-filter then reorders the retrieved tools.
    """

    def test_pre_filter_applied_flag(self, db_factory, db_session):
        """Historical skill ranks first for history+analytical query."""
        from unittest.mock import Mock

        # Register two skills with triggers that match the query "分析前一个上下文"
        for sid, name, scope, triggers in [
            ("pf_introspect@1.0.0", "pf_introspect", "current_session",
             ["上下文", "状态"]),
            ("pf_event_reader@1.0.0", "pf_event_reader", "historical",
             ["分析", "上下文"]),
        ]:
            db_session.query(SkillModel).filter(SkillModel.skill_id == sid).delete()
            db_session.add(SkillModel(
                skill_id=sid, skill_name=name, version="1.0.0",
                description=f"Test {name}", is_active=1, status="active",
                category="test",
                triggers=triggers,
                tags={"scope": scope,
                      "data_source": "event_store" if scope == "historical" else "session_metadata",
                      "intent_type": ["analytical"] if scope == "historical" else ["introspect"],
                      "requires_history": scope == "historical"},
            ))
        db_session.commit()

        llm = Mock()
        llm.chat_with_tools = Mock(return_value={"tool_calls": []})

        from core.skills.pipeline import SkillPipeline
        pipeline = SkillPipeline(db_factory, llm, audit=False, learning=False)

        state = ConversationState(references_history=True, is_analytical=True)
        result = pipeline.get_tools_schema(
            query="分析前一个上下文",
            session_id="test-session",
            conversation_state=state,
        )

        # Both skills should be retrieved (triggers match query)
        assert len(result.tools) >= 2, f"Expected >=2 tools, got {len(result.tools)}"

        # Historical skill must come first (via retrieval or pre-filter)
        tool_names = [t["function"]["name"] for t in result.tools]
        assert tool_names.index("pf_event_reader") < tool_names.index("pf_introspect"), (
            f"event_reader should rank before introspect, got: {tool_names}"
        )
    def test_no_conversation_state_no_prefilter(self, db_factory, db_session):
        """Without conversation_state, pre_filter_applied is False."""
        from unittest.mock import Mock

        llm = Mock()
        llm.chat_with_tools = Mock(return_value={"tool_calls": []})

        from core.skills.pipeline import SkillPipeline
        pipeline = SkillPipeline(db_factory, llm, audit=False, learning=False)

        result = pipeline.get_tools_schema(
            query="hello",
            session_id="test-session",
        )

        assert result.pre_filter_applied is False


# ── End-to-End: Message → ConversationState → PreFilter → Selector ───


class TestEndToEndPreFilter:
    """Full chain: message → ConversationState → pre_filter → correct order.

    Test skills must have triggers matching the query so keyword retrieval
    finds them.  Pre-filter then reorders the retrieved results.
    """

    def test_session_019cbb9e_scenario(self, db_factory, db_session):
        """Reproduce real failure: history+analytical → prefer historical."""
        from unittest.mock import Mock

        from core.skills.pipeline import SkillPipeline
        from core.skills.prefilter import ConversationState

        # Setup: two skills mimicking the real conflict, with triggers
        skills_data = [
            ("e2e_introspection@1.0.0", "e2e_introspection",
             "Inspect current session context and agent state",
             ["上下文", "状态", "context"],
             {"scope": "current_session",
              "data_source": "session_metadata",
              "intent_type": ["introspect"],
              "requires_history": False}),
            ("e2e_event_reader@1.0.0", "e2e_event_reader",
             "Analyze historical events and decision chains",
             ["分析", "决策链", "上下文", "analyze"],
             {"scope": "historical",
              "data_source": "event_store",
              "intent_type": ["analytical"],
              "requires_history": True}),
        ]
        for sid, name, desc, triggers, tags in skills_data:
            db_session.query(SkillModel).filter(SkillModel.skill_id == sid).delete()
            db_session.add(SkillModel(
                skill_id=sid, skill_name=name, version="1.0.0",
                description=desc, is_active=1, status="active",
                category="analysis", triggers=triggers, tags=tags,
            ))
        db_session.commit()

        # Step 1: Build messages exactly as chat_loop would see them
        messages = [
            {"role": "user", "content": "分析一下前一个上下文的情况还有决策链评估"},
        ]

        # Step 2: Extract ConversationState from messages (same as chat_loop does)
        state = ConversationState.from_messages(messages)
        assert state.references_history is True, "Should detect '前一个上下文' as history reference"
        assert state.is_analytical is True, "Should detect '分析' as analytical"

        # Step 3: Run through pipeline
        llm = Mock()
        llm.chat_with_tools = Mock(return_value={"tool_calls": []})
        pipeline = SkillPipeline(db_factory, llm, audit=False, learning=False)

        result = pipeline.get_tools_schema(
            query="分析一下前一个上下文的情况还有决策链评估",
            session_id="test-e2e",
            conversation_state=state,
        )

        # Step 4: Verify historical skill comes first
        assert len(result.tools) >= 2, f"Expected >=2 tools, got {len(result.tools)}"

        tool_names = [t["function"]["name"] for t in result.tools]
        reader_idx = tool_names.index("e2e_event_reader")
        intro_idx = tool_names.index("e2e_introspection")
        assert reader_idx < intro_idx, (
            f"event_reader should rank before introspection, "
            f"got order: {tool_names}"
        )

    def test_from_messages_to_prefilter_english(self, db_factory, db_session):
        """English message: 'analyze the previous session decisions' → historical preferred."""
        from unittest.mock import Mock

        from core.skills.pipeline import SkillPipeline
        from core.skills.prefilter import ConversationState

        for sid, name, scope, triggers in [
            ("e2e_en_intro@1.0.0", "e2e_en_intro", "current_session",
             ["session", "context", "analyze"]),
            ("e2e_en_hist@1.0.0", "e2e_en_hist", "historical",
             ["analyze", "decisions", "session"]),
        ]:
            db_session.query(SkillModel).filter(SkillModel.skill_id == sid).delete()
            db_session.add(SkillModel(
                skill_id=sid, skill_name=name, version="1.0.0",
                description=f"Test {name}", is_active=1, status="active",
                category="analysis", triggers=triggers,
                tags={"scope": scope,
                      "data_source": "event_store" if scope == "historical" else "session_metadata",
                      "intent_type": ["analytical"] if scope == "historical" else ["introspect"],
                      "requires_history": scope == "historical"},
            ))
        db_session.commit()

        messages = [
            {"role": "user", "content": "analyze the previous session decisions"},
        ]
        state = ConversationState.from_messages(messages)
        assert state.references_history is True
        assert state.is_analytical is True

        llm = Mock()
        llm.chat_with_tools = Mock(return_value={"tool_calls": []})
        pipeline = SkillPipeline(db_factory, llm, audit=False, learning=False)

        result = pipeline.get_tools_schema(
            query="analyze the previous session decisions",
            session_id="test-e2e-en",
            conversation_state=state,
        )
        assert len(result.tools) >= 2, f"Expected >=2 tools, got {len(result.tools)}"

        # Historical skill must come first
        tool_names = [t["function"]["name"] for t in result.tools]
        assert tool_names.index("e2e_en_hist") < tool_names.index("e2e_en_intro"), (
            f"historical should rank first, got: {tool_names}"
        )

    def test_fetch_message_prefers_external(self, db_factory, db_session):
        """'show me the PR list' → external scope preferred over historical."""
        from unittest.mock import Mock

        from core.skills.pipeline import SkillPipeline
        from core.skills.prefilter import ConversationState

        for sid, name, scope, intent, triggers in [
            ("e2e_fetch_ext@1.0.0", "e2e_fetch_ext", "external", ["fetch"],
             ["PR", "list", "show"]),
            ("e2e_fetch_hist@1.0.0", "e2e_fetch_hist", "historical", ["analytical"],
             ["PR", "analyze"]),
        ]:
            db_session.query(SkillModel).filter(SkillModel.skill_id == sid).delete()
            db_session.add(SkillModel(
                skill_id=sid, skill_name=name, version="1.0.0",
                description=f"Test {name}", is_active=1, status="active",
                category="test", triggers=triggers,
                tags={
                    "scope": scope,
                    "data_source": (
                        "external_api" if scope == "external"
                        else "event_store"
                    ),
                    "intent_type": intent,
                    "requires_history": False,
                },
            ))
        db_session.commit()

        messages = [{"role": "user", "content": "show me the PR list"}]
        state = ConversationState.from_messages(messages)
        assert state.is_fetch is True
        assert state.references_history is False

        llm = Mock()
        llm.chat_with_tools = Mock(return_value={"tool_calls": []})
        pipeline = SkillPipeline(db_factory, llm, audit=False, learning=False)

        result = pipeline.get_tools_schema(
            query="show me the PR list",
            session_id="test-e2e-fetch",
            conversation_state=state,
        )
        tool_names = [t["function"]["name"] for t in result.tools]

        # External skill must be present for a fetch query
        assert "e2e_fetch_ext" in tool_names, (
            f"external fetch skill missing for fetch query, got: {tool_names}"
        )
        # If historical skill survived the upstream selector's top-N cutoff,
        # it must rank below external.  (The selector may legitimately exclude
        # it — pre_filter never removes, but the selector upstream does.)
        if "e2e_fetch_hist" in tool_names:
            assert tool_names.index("e2e_fetch_ext") < tool_names.index("e2e_fetch_hist"), (
                f"external should rank before historical for fetch, got: {tool_names}"
            )


# ── Edge Tool Unification Tests ──────────────────────────────────


class TestEdgeToolUnification:
    """Edge tools registered in skills_registry with tags; pre_filter in select_tools_for_turn."""

    def test_edge_tools_registered_in_db(self, db_session):
        """All 16 edge tools have metadata in skills_registry with source=edge."""
        from sqlalchemy import text
        import json

        rows = db_session.execute(text(
            "SELECT skill_name, category, tags FROM skills_registry "
            "WHERE source = 'edge' AND is_active = 1"
        )).fetchall()

        names = {r[0] for r in rows}
        expected = {
            "reflect", "get_agent_info",
            "read_file", "write_file", "str_replace", "list_dir",
            "grep", "glob", "bash",
            "git_status", "git_diff", "git_log",
            "find_skills", "set_skill_setting",
            "bind_skill_resource", "validate_skill_config",
        }
        missing = expected - names
        assert not missing, f"Edge tools missing from DB: {missing}"

        # Verify reflect has scope=historical
        reflect_row = next(r for r in rows if r[0] == "reflect")
        tags = json.loads(reflect_row[2]) if isinstance(reflect_row[2], str) else reflect_row[2]
        assert tags["scope"] == "historical"
        assert tags["data_source"] == "event_store"

        # Verify get_agent_info has scope=current_session
        gai_row = next(r for r in rows if r[0] == "get_agent_info")
        tags = json.loads(gai_row[2]) if isinstance(gai_row[2], str) else gai_row[2]
        assert tags["scope"] == "current_session"

    def test_select_tools_for_turn_applies_prefilter_for_history_query(self, db_session):
        """select_tools_for_turn reorders tools via pre_filter for history+analytical queries."""
        from unittest.mock import MagicMock
        from api.routers.chat import select_tools_for_turn
        from core.llm.models import LLMProvider, LLMResponse

        # Simulate merged tools schema with reflect and get_agent_info
        tools = [
            {"function": {"name": "get_agent_info",
                          "description": "Query CURRENT runtime state"}},
            {"function": {"name": "reflect",
                          "description": "Diagnose PAST behavior"}},
            {"function": {"name": "list_prs",
                          "description": "List pull requests"}},
        ]
        messages = [{"role": "user",
                     "content": "分析一下前一个上下文的情况还有决策链评估"}]

        # LLM picks whatever is first in catalog — we want to verify pre_filter
        # reorders so reflect comes before get_agent_info in the catalog prompt
        catalog_seen: list[str] = []

        class SpyLLM:
            def chat(self, msgs, **kwargs):
                catalog_seen.append(msgs[0]["content"])
                return LLMResponse(
                    content="reflect", model="test",
                    provider=LLMProvider.OPENAI,
                    tokens_prompt=0, tokens_completion=0,
                    tokens_total=0, latency_ms=0, cost_usd=0.0,
                )

        result = select_tools_for_turn(tools, messages, None, "u1", SpyLLM())

        assert result.selected_tool == "reflect"

        # Verify reflect appeared before get_agent_info in the catalog prompt
        # sent to the LLM.  No guards — if catalog_seen is empty or names are
        # missing, the test must fail (not pass vacuously).
        assert len(catalog_seen) == 1, (
            f"Expected exactly 1 LLM call, got {len(catalog_seen)}"
        )
        prompt = catalog_seen[0]
        assert "reflect" in prompt and "get_agent_info" in prompt, (
            "Both tools must appear in catalog prompt"
        )
        assert prompt.index("reflect") < prompt.index("get_agent_info"), (
            "Pre-filter should put reflect before get_agent_info "
            "for history+analytical queries"
        )

    def test_reflect_description_is_past_focused(self):
        """reflect description must clearly signal PAST/historical scope."""
        from cli.tools.reflect import ReflectTool
        tool = ReflectTool()
        desc = tool.description.lower()
        assert "past" in desc, "reflect must mention PAST"
        assert "current" not in desc or "not" in desc, (
            "reflect must not claim to handle current state"
        )

    def test_get_agent_info_description_is_current_focused(self):
        """get_agent_info description must clearly signal CURRENT/live scope."""
        from cli.tools.introspection import GetAgentInfoTool
        tool = GetAgentInfoTool()
        desc = tool.description.lower()
        assert "current" in desc, "get_agent_info must mention CURRENT"
        assert "past" not in desc or "not" in desc, (
            "get_agent_info must not claim to handle past events"
        )

    def test_no_client_side_tool_filtering(self):
        """edge_chat_loop must not do client-side keyword filtering."""
        import cli.edge_chat_loop as ecl
        assert not hasattr(ecl, "_filter_relevant_tools"), (
            "_filter_relevant_tools removed — filtering is server-side"
        )
        assert not hasattr(ecl, "_CORE_TOOLS"), (
            "_CORE_TOOLS removed — no hardcoded always-include list"
        )
        assert not hasattr(ecl, "_TOOL_KEYWORDS"), (
            "_TOOL_KEYWORDS removed — keyword matching replaced by pre_filter"
        )

    def test_fetch_intent_does_not_deprioritize_local_tools(self):
        """'show me the file' must not push read_file below GitHub skills."""
        from core.skills.prefilter import ConversationState, SkillTags, ToolWrapper, pre_filter

        tools = [
            ToolWrapper("list_prs",
                        SkillTags("external", "external_api", ("fetch",), False),
                        {"function": {"name": "list_prs"}}),
            ToolWrapper("read_file",
                        SkillTags("local", "local_filesystem", ("fetch",), False),
                        {"function": {"name": "read_file"}}),
            ToolWrapper("reflect",
                        SkillTags("historical", "event_store", ("analytical",), True),
                        {"function": {"name": "reflect"}}),
        ]
        state = ConversationState(is_fetch=True, references_history=False)
        reordered, applied = pre_filter(tools, state)
        names = [w.name for w in reordered]
        # Both external and local should be preferred over historical
        assert names.index("read_file") < names.index("reflect"), (
            f"local fetch tool should rank above historical, got: {names}"
        )
        assert names.index("list_prs") < names.index("reflect"), (
            f"external fetch tool should rank above historical, got: {names}"
        )

    def test_edge_tool_db_description_matches_tool_class(self, db_session):
        """DB metadata description must be consistent with the tool class."""
        from sqlalchemy import text
        from cli.tools.reflect import ReflectTool
        from cli.tools.introspection import GetAgentInfoTool

        for ToolClass, db_name in [
            (ReflectTool, "reflect"),
            (GetAgentInfoTool, "get_agent_info"),
        ]:
            tool = ToolClass()
            row = db_session.execute(text(
                "SELECT description FROM skills_registry "
                "WHERE skill_name = :name AND source = 'edge' AND is_active = 1"
            ), {"name": db_name}).fetchone()
            assert row is not None, f"{db_name} not found in DB"
            db_desc = row[0].lower()
            # Both must agree on temporal scope (past vs current)
            if db_name == "reflect":
                assert "past" in db_desc, f"DB description for {db_name} must mention PAST"
                assert "past" in tool.description.lower()
            elif db_name == "get_agent_info":
                assert "current" in db_desc, f"DB description for {db_name} must mention CURRENT"
                assert "current" in tool.description.lower()


# ── Reflect Output Quality Tests ─────────────────────────────────


class TestReflectOutputQuality:
    """Structural guarantees on reflect output: context data present,
    self-events excluded, output size bounded."""

    @pytest.fixture(autouse=True)
    def _seed_session(self, db_session):
        """Create a minimal session with events + ctx_snapshot."""
        from sqlalchemy import text
        self.sid = str(uuid7())
        uid = "test_user"
        chain = str(uuid7())
        _cols = (
            "event_id, event_type, user_id, session_id, "
            "agent_id, agent_version, "
            "causal_chain_id, content, created_at"
        )
        _base = {"uid": uid, "sid": self.sid, "chain": chain}

        def _ins(etype, content, skill=None, extra_cols="", extra_vals="", extra_params=None):
            cols = _cols + (f", skill_name{extra_cols}" if skill else extra_cols)
            vals = (
                ":eid, :etype, :uid, :sid, "
                "'test-agent', '1.0', "
                ":chain, :content, NOW()"
            )
            if skill:
                vals += ", :skill"
            vals += extra_vals
            params = {**_base, "eid": str(uuid7()), "etype": etype,
                      "content": content}
            if skill:
                params["skill"] = skill
            if extra_params:
                params.update(extra_params)
            db_session.execute(text(
                f"INSERT INTO agent_events ({cols}) VALUES ({vals})"
            ), params)

        _ins("user_query", "show me PRs")
        _ins("tool_call", '{"name":"list_prs","arguments":"{}"}',
             skill="list_prs")
        _ins("tool_result", '{"name":"list_prs","result":"ok"}',
             skill="list_prs")
        # reflect events (should be filtered out by compaction)
        _ins("tool_call", '{"name":"reflect","arguments":"{}"}',
             skill="reflect")
        _ins("tool_result", '{"name":"reflect","result":"{}"}',
             skill="reflect")
        # llm_response with token usage
        _ins("llm_response", "Here are the PRs",
             extra_cols=", llm_model_used, token_usage",
             extra_vals=", :model, :usage",
             extra_params={
                 "model": "test-model",
                 "usage": '{"prompt_tokens":500,"completion_tokens":100}',
             })
        # ctx_snapshot with token budget
        db_session.execute(text(
            "INSERT INTO ctx_snapshots "
            "(context_capture_id, session_id, event_id, "
            "token_budget, created_at) "
            "VALUES (:snap, :sid, :eid, :budget, NOW())"
        ), {"snap": str(uuid7()), "sid": self.sid,
            "eid": str(uuid7()),
            "budget": json.dumps({
                "identity": 23, "self_model": 232,
                "tool_schemas": 186, "project_context": 96,
                "memory": 9, "user_query": 11,
            })})
        db_session.commit()
        self.uid = uid
        yield
        db_session.execute(text(
            "DELETE FROM agent_events WHERE session_id = :sid"
        ), {"sid": self.sid})
        db_session.execute(text(
            "DELETE FROM ctx_snapshots WHERE session_id = :sid"
        ), {"sid": self.sid})
        db_session.commit()

    def _build(self, db_factory, focus="auto"):
        from core.agent.reflect_service import ReflectService
        svc = ReflectService(db_factory)
        return svc.build_evidence(
            self.sid, self.uid, focus, last_n=20,
            question="analyze context and decision chain",
        )

    def test_context_budgets_present(self, db_factory):
        """reflect must return context budget breakdown."""
        result = self._build(db_factory)
        budgets = result.get("context_budgets", [])
        assert len(budgets) >= 1, "context_budgets must have data"
        b = budgets[0]
        assert "tool_schemas" in b
        assert "self_model" in b
        assert b["tool_schemas"] == 186

    def test_reflect_self_events_excluded(self, db_factory):
        """reflect must not include its own events in event_summary."""
        result = self._build(db_factory)
        for evt in result["event_summary"]:
            assert evt.get("skill") != "reflect", (
                f"reflect self-event leaked: {evt}"
            )
            assert evt.get("tool_name") != "reflect", (
                f"reflect self-event leaked: {evt}"
            )

    def test_cloud_tool_result_in_event_summary(self, db_factory):
        """Cloud skill tool_result must appear in event_summary."""
        result = self._build(db_factory)
        tool_results = [
            e for e in result["event_summary"]
            if e.get("type") == "tool_result"
        ]
        assert len(tool_results) >= 1, (
            "tool_result for list_prs must be in event_summary"
        )
        assert tool_results[0].get("tool_name") == "list_prs"

    def test_output_size_bounded(self, db_factory):
        """reflect output must stay within reasonable size."""
        for focus in ("history", "tool_selection"):
            result = self._build(db_factory, focus=focus)
            output = json.dumps(result, ensure_ascii=False)
            assert len(output) < 5000, (
                f"focus={focus} output too large: {len(output)} chars"
            )

    def test_edge_tools_compacted_to_names(self, db_factory):
        """edge_tools must be name-only strings, not full schemas."""
        def peek(_sid):
            return {"tools": [
                {"function": {"name": "read_file",
                              "description": "x" * 200}},
                {"function": {"name": "bash",
                              "description": "y" * 200}},
            ]}
        from core.agent.reflect_service import ReflectService
        svc = ReflectService(db_factory, peek_session=peek)
        result = svc.build_evidence(
            self.sid, self.uid, "tool_selection", 20,
        )
        et = result.get("edge_tools", [])
        assert len(et) == 2
        assert all(isinstance(t, str) for t in et), (
            f"edge_tools should be strings, got: {et}"
        )

    def test_cloud_skills_no_parameters(self, db_factory):
        """cloud_skills must not include full parameter schemas."""
        from unittest.mock import MagicMock
        registry = MagicMock()
        skill = MagicMock()
        skill.name = "list_prs"
        skill.description = "List pull requests"
        skill.to_openai_schema.return_value = {
            "function": {
                "name": "list_prs",
                "parameters": {"type": "object", "properties": {
                    "repo": {"type": "string"},
                }},
            }
        }
        registry.list_skills.return_value = [skill]
        from core.agent.reflect_service import ReflectService
        svc = ReflectService(db_factory, skill_registry=registry)
        result = svc.build_evidence(
            self.sid, self.uid, "tool_selection", 20,
        )
        for s in result.get("cloud_skills", []):
            assert "parameters" not in s, (
                f"cloud_skill {s['name']} should not have parameters"
            )


# ── _get_tag_map + _prefilter_tools DB integration ───────────────


class TestGetTagMapAndPrefilterFromDB:
    """Verify _get_tag_map loads tags from DB and _prefilter_tools reorders tools."""

    def test_tag_map_loads_from_db(self, db_session):
        """_get_tag_map returns tags for skills stored in skills_registry."""
        import api.routers.chat as chat_mod

        # Insert a skill with explicit tags
        db_session.query(SkillModel).filter(
            SkillModel.skill_id == "pf_hist@1.0.0"
        ).delete()
        db_session.add(SkillModel(
            skill_id="pf_hist@1.0.0", skill_name="pf_hist", version="1.0.0",
            description="Historical skill", is_active=1, status="active",
            category="test",
            tags={"scope": "historical", "data_source": "event_store",
                  "intent_type": ["analytical"], "requires_history": True},
        ))
        db_session.commit()

        # Invalidate module-level cache so _get_tag_map hits DB
        chat_mod._tag_cache.clear()
        chat_mod._tag_cache_ts = 0.0

        tag_map = chat_mod._get_tag_map()

        assert "pf_hist" in tag_map, f"pf_hist not in tag_map: {list(tag_map.keys())[:10]}"
        assert tag_map["pf_hist"] is not None
        assert tag_map["pf_hist"].scope == "historical"

    def test_prefilter_tools_reorders_via_db_tags(self, db_session):
        """_prefilter_tools uses DB tags to reorder: historical before current_session."""
        import api.routers.chat as chat_mod

        # Insert two skills with different scopes
        for sid, name, scope in [
            ("pf_current@1.0.0", "pf_current", "current_session"),
            ("pf_historical@1.0.0", "pf_historical", "historical"),
        ]:
            db_session.query(SkillModel).filter(SkillModel.skill_id == sid).delete()
            db_session.add(SkillModel(
                skill_id=sid, skill_name=name, version="1.0.0",
                description=f"Test {name}", is_active=1, status="active",
                category="test",
                tags={"scope": scope, "data_source": "event_store",
                      "intent_type": ["analytical"], "requires_history": scope == "historical"},
            ))
        db_session.commit()

        # Invalidate cache
        chat_mod._tag_cache.clear()
        chat_mod._tag_cache_ts = 0.0

        # Tools in wrong order: current before historical
        tools = [
            {"function": {"name": "pf_current", "description": "Current state"}},
            {"function": {"name": "pf_historical", "description": "Past events"}},
        ]
        # History+analytical query triggers Rule 1: historical first
        messages = [{"role": "user", "content": "分析一下之前的历史记录"}]

        result = chat_mod._prefilter_tools(tools, messages)

        names = [t["function"]["name"] for t in result]
        assert names.index("pf_historical") < names.index("pf_current"), (
            f"historical should come before current_session, got: {names}"
        )

    def test_prefilter_tools_no_tags_no_crash(self, db_session):
        """Skills without tags in DB don't crash _prefilter_tools."""
        import api.routers.chat as chat_mod

        db_session.query(SkillModel).filter(
            SkillModel.skill_id == "pf_notags@1.0.0"
        ).delete()
        db_session.add(SkillModel(
            skill_id="pf_notags@1.0.0", skill_name="pf_notags", version="1.0.0",
            description="No tags", is_active=1, status="active",
            category="test", tags=None,
        ))
        db_session.commit()

        chat_mod._tag_cache.clear()
        chat_mod._tag_cache_ts = 0.0

        tools = [
            {"function": {"name": "pf_notags", "description": "No tags"}},
            {"function": {"name": "pf_other", "description": "Other"}},
        ]
        messages = [{"role": "user", "content": "hello"}]

        # Must not crash — returns tools in original order
        result = chat_mod._prefilter_tools(tools, messages)
        assert len(result) == 2

    def test_tag_map_cache_hit(self, db_session):
        """Second call within TTL returns cached result without DB query."""
        import api.routers.chat as chat_mod

        # Prime the cache
        chat_mod._tag_cache.clear()
        chat_mod._tag_cache_ts = 0.0
        first = chat_mod._get_tag_map()

        # Second call should hit cache (no DB query)
        second = chat_mod._get_tag_map()
        assert second is first  # Same dict object = cache hit

    def test_tag_map_db_failure_returns_empty(self, db_session, monkeypatch):
        """When SkillSelector raises, _get_tag_map returns empty dict."""
        import api.routers.chat as chat_mod

        chat_mod._tag_cache.clear()
        chat_mod._tag_cache_ts = 0.0

        # Make SkillSelector raise
        monkeypatch.setattr(
            "api.routers.chat.SkillSelector",
            lambda *a, **kw: (_ for _ in ()).throw(RuntimeError("DB down")),
        )

        result = chat_mod._get_tag_map()
        assert result == {}

    def test_prefilter_tools_exception_returns_original(self, db_session, monkeypatch):
        """When _get_tag_map raises inside _prefilter_tools, original order preserved."""
        import api.routers.chat as chat_mod

        monkeypatch.setattr(
            chat_mod, "_get_tag_map", lambda: (_ for _ in ()).throw(RuntimeError("boom")),
        )

        tools = [
            {"function": {"name": "a", "description": "A"}},
            {"function": {"name": "b", "description": "B"}},
        ]
        result = chat_mod._prefilter_tools(tools, [{"role": "user", "content": "hello"}])
        assert [t["function"]["name"] for t in result] == ["a", "b"]
