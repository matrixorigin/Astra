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


