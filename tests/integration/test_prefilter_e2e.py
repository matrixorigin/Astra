"""Integration tests for pre-filter: DB persistence and pipeline integration.

Tests tags persistence in skills_registry via SkillCatalog,
tags loading via SkillSelector, and pre-filter in SkillPipeline.
"""

import pytest

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
        assert len(result.tools) >= 2, f"Expected >=2 tools, got {len(result.tools)}"

        # External skill must come first for fetch intent
        tool_names = [t["function"]["name"] for t in result.tools]
        assert tool_names.index("e2e_fetch_ext") < tool_names.index("e2e_fetch_hist"), (
            f"external should rank first for fetch, got: {tool_names}"
        )
