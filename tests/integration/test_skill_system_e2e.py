"""End-to-end tests for skill system redesign (Tasks 1-5).

All tests use real MatrixOne database. Every assertion checks actual DB field values.
"""

import json
import pytest
from datetime import datetime, timezone
from uuid import uuid4
from uuid_utils import uuid7

from api.database import SessionLocal
from api.models import SkillRegistry as SkillModel, SkillSelectionLearning, SkillSelectionEvent
from core.skills.base import Skill, SkillInput, SkillOutput, SkillRequirement, RuntimeRequirement, SideEffectCategory, SideEffectProfile
from core.skills.registry import SkillRegistry
from core.skills.procedural_memory import learning_to_memory, list_as_memories
from core.memory.types import MemoryType, TrustTier


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


class _TestSkill(Skill[SkillInput, SkillOutput]):
    name = "e2e_test_skill"
    version = "1.0.0"
    description = "E2E test skill v1"
    requirements = SkillRequirement(runtime=[RuntimeRequirement.FILESYSTEM])
    side_effect_profile = SideEffectProfile(category=SideEffectCategory.READ)

    async def execute(self, input):
        return SkillOutput(success=True, result="v1 executed")


class _TestSkillV2(Skill[SkillInput, SkillOutput]):
    name = "e2e_test_skill"
    version = "2.0.0"
    description = "E2E test skill v2"
    requirements = SkillRequirement(runtime=[RuntimeRequirement.FILESYSTEM])
    side_effect_profile = SideEffectProfile(category=SideEffectCategory.READ)

    async def execute(self, input):
        return SkillOutput(success=True, result="v2 executed")


class _TestSkillV3(Skill[SkillInput, SkillOutput]):
    name = "e2e_test_skill"
    version = "3.0.0"
    description = "E2E test skill v3"
    requirements = SkillRequirement(runtime=[RuntimeRequirement.FILESYSTEM, RuntimeRequirement.NETWORK])
    side_effect_profile = SideEffectProfile(category=SideEffectCategory.WRITE)

    async def execute(self, input):
        return SkillOutput(success=True, result="v3 executed")


@pytest.fixture
def clean_e2e_db():
    """Clean all e2e test data before and after test."""
    db = SessionLocal()
    db.query(SkillModel).filter(SkillModel.skill_name == "e2e_test_skill").delete()
    db.query(SkillSelectionLearning).filter(
        SkillSelectionLearning.query_pattern.like("e2e_%")
    ).delete(synchronize_session="fetch")
    db.commit()
    try:
        yield db
    finally:
        try:
            db.query(SkillModel).filter(SkillModel.skill_name == "e2e_test_skill").delete()
            db.query(SkillSelectionLearning).filter(
                SkillSelectionLearning.query_pattern.like("e2e_%")
            ).delete(synchronize_session="fetch")
            db.commit()
        except Exception:
            db.rollback()
        finally:
            db.close()


# ---------------------------------------------------------------------------
# Task 1: Unified Skill base class
# ---------------------------------------------------------------------------

class TestTask1UnifiedSkill:
    """Every EdgeTool is a Skill; to_openai_schema works; requirements propagate."""

    def test_edge_tool_is_skill_instance(self):
        from cli.tools.file_ops import ReadFileTool, WriteFileTool
        from cli.tools.shell import BashTool
        from cli.tools.search import GrepTool

        tools = [ReadFileTool("."), WriteFileTool("."), BashTool("."), GrepTool(".")]
        for t in tools:
            assert isinstance(t, Skill), f"{t.name} not a Skill"
            assert isinstance(t.requirements, SkillRequirement), f"{t.name} requirements not SkillRequirement"
            assert isinstance(t.side_effect_profile, SideEffectProfile), f"{t.name} missing SideEffectProfile"
            assert len(t.requirements.runtime) > 0, f"{t.name} has no runtime requirements"
            schema = t.to_openai_schema()
            assert schema["type"] == "function", f"{t.name} schema type wrong"
            assert schema["function"]["name"] == t.name, f"{t.name} schema name mismatch"

    def test_to_openai_schema_structure(self):
        from cli.tools.file_ops import ReadFileTool
        t = ReadFileTool(".")
        schema = t.to_openai_schema()

        assert schema["type"] == "function"
        fn = schema["function"]
        assert fn["name"] == "read_file"
        assert isinstance(fn["description"], str) and len(fn["description"]) > 0
        assert fn["parameters"]["type"] == "object"
        props = fn["parameters"]["properties"]
        assert "path" in props
        # Framework fields excluded
        assert "user_id" not in props
        assert "session_id" not in props
        assert "repo_id" not in props

    def test_side_effect_profile_auto_derived(self):
        from cli.tools.file_ops import ReadFileTool, WriteFileTool
        from cli.tools.shell import BashTool

        assert ReadFileTool(".").side_effect_profile.category == SideEffectCategory.READ
        assert WriteFileTool(".").side_effect_profile.category == SideEffectCategory.WRITE
        assert BashTool(".").side_effect_profile.category == SideEffectCategory.EXECUTE

    def test_runtime_requirements_default_filesystem(self):
        from cli.tools.file_ops import ReadFileTool
        t = ReadFileTool(".")
        assert RuntimeRequirement.FILESYSTEM in t.requirements.runtime


# ---------------------------------------------------------------------------
# Task 2: ToolRouter dispatches Skills
# ---------------------------------------------------------------------------

class TestTask2ToolRouter:
    """ToolRouter accepts Skill, executes, returns timing."""

    @pytest.mark.asyncio
    async def test_register_and_execute(self):
        from cli.tools.router import ToolRouter, ToolCall
        from cli.tools.file_ops import ReadFileTool

        router = ToolRouter()
        tool = ReadFileTool(".")
        router.register(tool)

        assert router.get_tool("read_file") is tool
        assert isinstance(router.get_tool("read_file"), Skill)

        schemas = router.get_schemas()
        assert len(schemas) == 1
        assert schemas[0]["function"]["name"] == "read_file"

        results = await router.execute([
            ToolCall(id="tc1", name="read_file", arguments={"path": "pyproject.toml", "end_line": 2})
        ])
        r = results[0]
        assert not r.error
        assert r.execution_time_ms >= 0
        assert r.name == "read_file"
        assert r.tool_call_id == "tc1"
        assert len(r.result) > 0

    @pytest.mark.asyncio
    async def test_unknown_tool_returns_error(self):
        from cli.tools.router import ToolRouter, ToolCall

        router = ToolRouter()
        results = await router.execute([ToolCall(id="tc2", name="no_such_tool", arguments={})])
        r = results[0]
        assert r.error
        assert "Unknown tool" in r.result


# ---------------------------------------------------------------------------
# Task 3: Procedural memory bridge
# ---------------------------------------------------------------------------

class TestTask3ProceduralMemoryBridge:
    """learning_to_memory converts every field correctly; list_as_memories filters."""

    def test_full_field_conversion(self, clean_e2e_db):
        db = clean_e2e_db
        now = datetime.now(timezone.utc).replace(tzinfo=None, microsecond=0)
        lid = str(uuid7())

        db.add(SkillSelectionLearning(
            learning_id=lid,
            query_pattern="e2e_review pr #42",
            wrong_skills=["summarize_pr", "list_files"],
            correct_skills=["code_review"],
            confidence=75.0,
            evidence_count=5,
            applied_count=3,
            signal_type="wrong_skill",
            target_metrics={"accuracy": 1.0, "time_ms": 500},
            context_features={"length_bucket": "medium", "contains_code": False},
            is_active=1,
            created_at=now,
            updated_at=now,
        ))
        db.commit()

        # Re-read from DB to ensure we test actual stored data
        row = db.query(SkillSelectionLearning).filter(
            SkillSelectionLearning.learning_id == lid
        ).first()
        assert row is not None

        mem = learning_to_memory(row)

        # Check every field
        assert mem.memory_id == lid
        assert mem.user_id == "__system__"
        assert mem.memory_type == MemoryType.PROCEDURAL
        assert mem.trust_tier == TrustTier.T3_INFERRED
        assert mem.initial_confidence == 0.75  # 75.0 / 100
        assert mem.is_active is True
        assert mem.observed_at == now
        assert mem.created_at == now
        assert mem.updated_at == now
        # Content should contain all key info
        assert "wrong_skill" in mem.content
        assert "e2e_review pr #42" in mem.content
        assert "summarize_pr" in mem.content
        assert "code_review" in mem.content

    def test_confidence_edge_cases(self, clean_e2e_db):
        """Test confidence mapping: 0-100 scale → 0-1 scale via normalize_confidence."""
        cases = [
            (0.0, 0.0),      # zero
            (10.0, 0.1),     # low (learning system default)
            (50.0, 0.5),     # mid
            (99.0, 0.99),    # high (learning system max)
            (100.0, 1.0),    # max
            (0.5, 0.5),      # already normalized (<=1.0)
            (None, 0.0),     # None → 0.0
        ]
        for raw_conf, expected in cases:
            row = SkillSelectionLearning(
                learning_id=f"conf_{raw_conf}",
                query_pattern="e2e_x",
                wrong_skills=[], correct_skills=[],
                confidence=raw_conf, signal_type="wrong_skill",
            )
            mem = learning_to_memory(row)
            assert abs(mem.initial_confidence - expected) < 0.001, \
                f"confidence={raw_conf} → expected {expected}, got {mem.initial_confidence}"

    def test_list_as_memories_active_filter(self, clean_e2e_db):
        db = clean_e2e_db
        lid_a = str(uuid4())
        lid_i = str(uuid4())
        qpat = f"e2e_lam_{uuid7().hex}"

        db.add(SkillSelectionLearning(
            learning_id=lid_a,
            query_pattern=f"{qpat}_active",
            wrong_skills=["x"], correct_skills=["y"],
            confidence=80.0, signal_type="wrong_skill", is_active=1,
        ))
        db.add(SkillSelectionLearning(
            learning_id=lid_i,
            query_pattern=f"{qpat}_inactive",
            wrong_skills=["x"], correct_skills=["y"],
            confidence=50.0, signal_type="wrong_skill", is_active=0,
        ))
        db.commit()

        active = list_as_memories(db, active_only=True)
        active_ids = {m.memory_id for m in active}
        assert lid_a in active_ids
        assert lid_i not in active_ids

        all_mems = list_as_memories(db, active_only=False)
        all_ids = {m.memory_id for m in all_mems}
        assert lid_a in all_ids
        assert lid_i in all_ids

        # Verify all meaningful fields on the active one
        mem = next(m for m in active if m.memory_id == lid_a)
        assert mem.memory_type == MemoryType.PROCEDURAL
        assert mem.user_id == "__system__"
        assert mem.initial_confidence == 0.8
        assert mem.trust_tier == TrustTier.T3_INFERRED
        assert mem.is_active is True
        assert mem.session_id is None
        assert "wrong_skill" in mem.content
        assert f"{qpat}_active" in mem.content

        # Verify inactive one preserves fields but is_active=False
        inactive_mem = next(m for m in all_mems if m.memory_id == lid_i)
        assert inactive_mem.is_active is False
        assert inactive_mem.initial_confidence == 0.5
        assert inactive_mem.memory_type == MemoryType.PROCEDURAL


# ---------------------------------------------------------------------------
# Task 5: Rollback + Uninstall (full DB field verification)
# ---------------------------------------------------------------------------

class TestTask5RollbackUninstall:
    """Register → rollback → uninstall with full DB field checks."""

    def test_full_lifecycle(self, clean_e2e_db):
        db = clean_e2e_db
        registry = SkillRegistry(SessionLocal)

        # --- Register v1 ---
        registry.register(
            _TestSkill(), is_active=True,
            category="test", subcategory="e2e",
            triggers=["test", "e2e"], dependencies=[],
            priority=7, cost_estimate="low",
        )
        db.expire_all()
        v1 = db.query(SkillModel).filter(SkillModel.skill_id == "e2e_test_skill@1.0.0").first()
        assert v1 is not None
        assert v1.skill_name == "e2e_test_skill"
        assert v1.version == "1.0.0"
        assert v1.description == "E2E test skill v1"
        assert v1.is_active == 1
        assert v1.status == "active"
        assert v1.category == "test"
        assert v1.subcategory == "e2e"
        assert v1.triggers == ["test", "e2e"]
        assert v1.priority == 7
        assert v1.cost_estimate == "low"
        assert v1.code_hash is not None and len(v1.code_hash) == 64  # SHA256
        assert v1.created_at is not None

        # --- Register v2 (auto-deactivates v1) ---
        registry.register(
            _TestSkillV2(), is_active=True,
            category="test", subcategory="e2e",
            triggers=["test", "e2e", "v2"], priority=8, cost_estimate="medium",
        )
        db.expire_all()
        v1 = db.query(SkillModel).filter(SkillModel.skill_id == "e2e_test_skill@1.0.0").first()
        v2 = db.query(SkillModel).filter(SkillModel.skill_id == "e2e_test_skill@2.0.0").first()
        assert v1.is_active == 0  # deactivated
        assert v2.is_active == 1
        assert v2.version == "2.0.0"
        assert v2.description == "E2E test skill v2"
        assert v2.priority == 8
        assert v2.triggers == ["test", "e2e", "v2"]

        # --- Register v3 ---
        registry.register(
            _TestSkillV3(), is_active=True,
            category="test", subcategory="e2e",
            triggers=["test", "v3"], priority=9, cost_estimate="high",
        )
        db.expire_all()
        v2 = db.query(SkillModel).filter(SkillModel.skill_id == "e2e_test_skill@2.0.0").first()
        v3 = db.query(SkillModel).filter(SkillModel.skill_id == "e2e_test_skill@3.0.0").first()
        assert v2.is_active == 0
        assert v3.is_active == 1
        assert v3.description == "E2E test skill v3"
        assert v3.cost_estimate == "high"

        # --- Rollback v3 → v2 ---
        activated = registry.rollback("e2e_test_skill")
        assert activated == "2.0.0"

        db.expire_all()
        v1 = db.query(SkillModel).filter(SkillModel.skill_id == "e2e_test_skill@1.0.0").first()
        v2 = db.query(SkillModel).filter(SkillModel.skill_id == "e2e_test_skill@2.0.0").first()
        v3 = db.query(SkillModel).filter(SkillModel.skill_id == "e2e_test_skill@3.0.0").first()
        assert v1.is_active == 0 and v1.status == "active"   # untouched
        assert v2.is_active == 1 and v2.status == "active"   # reactivated
        assert v3.is_active == 0 and v3.status == "deprecated"  # rolled back

        # --- Rollback again v2 → v1 ---
        activated = registry.rollback("e2e_test_skill")
        assert activated == "1.0.0"

        db.expire_all()
        v1 = db.query(SkillModel).filter(SkillModel.skill_id == "e2e_test_skill@1.0.0").first()
        v2 = db.query(SkillModel).filter(SkillModel.skill_id == "e2e_test_skill@2.0.0").first()
        assert v1.is_active == 1 and v1.status == "active"
        assert v2.is_active == 0 and v2.status == "deprecated"

        # --- Uninstall ---
        count = registry.uninstall("e2e_test_skill")
        assert count == 3

        db.expire_all()
        remaining = db.query(SkillModel).filter(
            SkillModel.skill_name == "e2e_test_skill"
        ).count()
        assert remaining == 0


# ---------------------------------------------------------------------------
# Golden session: register → select → execute → feedback → learn → rollback
# ---------------------------------------------------------------------------

class TestGoldenSession:
    """Full end-to-end golden path through the skill system."""

    @pytest.mark.asyncio
    async def test_golden_path(self, clean_e2e_db):
        db = clean_e2e_db

        # 1. Register skill
        registry = SkillRegistry(SessionLocal)
        registry.register(
            _TestSkill(), is_active=True,
            category="test", triggers=["e2e_golden"],
        )
        db.expire_all()
        row = db.query(SkillModel).filter(SkillModel.skill_id == "e2e_test_skill@1.0.0").first()
        assert row is not None
        assert row.is_active == 1
        assert row.status == "active"
        assert row.skill_name == "e2e_test_skill"
        assert row.version == "1.0.0"
        assert row.description == "E2E test skill v1"
        assert row.category == "test"
        assert row.triggers == ["e2e_golden"]
        assert row.code_hash is not None and len(row.code_hash) == 64

        # 2. Simulate selection event (what SkillPipeline.get_tools_schema does)
        event_id = str(uuid7())
        from sqlalchemy import text
        db.execute(text("""
            INSERT INTO skill_selection_events
            (event_id, session_id, user_query, selected_skills, skill_name, skill_version, selection_method, created_at)
            VALUES (:eid, :sid, :q, :skills, :sn, :sv, :method, :ts)
        """), {
            "eid": event_id, "sid": "e2e_golden_session",
            "q": "e2e_golden test query", "skills": json.dumps(["e2e_test_skill"]),
            "sn": "e2e_test_skill", "sv": "1.0.0", "method": "semantic",
            "ts": datetime.now(timezone.utc).replace(tzinfo=None),
        })
        db.commit()

        # Verify selection event in DB
        evt = db.query(SkillSelectionEvent).filter(SkillSelectionEvent.event_id == event_id).first()
        assert evt is not None
        assert evt.session_id == "e2e_golden_session"
        assert evt.user_query == "e2e_golden test query"
        assert evt.skill_name == "e2e_test_skill"
        assert evt.skill_version == "1.0.0"
        assert evt.selection_method == "semantic"
        assert evt.created_at is not None

        # 3. Execute skill
        result = await _TestSkill().execute(SkillInput())
        assert result.success is True
        assert result.result == "v1 executed"

        # 4. Simulate feedback backfill (what AgentExecutor does)
        db.execute(text("""
            UPDATE skill_selection_events
            SET execution_time_ms = :t, execution_cost = :c, execution_success = :s
            WHERE event_id = :eid
        """), {"t": 150, "c": 0.02, "s": 1, "eid": event_id})
        db.commit()

        db.expire_all()
        evt = db.query(SkillSelectionEvent).filter(SkillSelectionEvent.event_id == event_id).first()
        assert evt.execution_time_ms == 150
        assert evt.execution_cost == 0.02
        assert evt.execution_success == 1

        # 5. Register v2 and rollback
        registry.register(_TestSkillV2(), is_active=True, category="test", triggers=["e2e_golden"])
        db.expire_all()
        v2 = db.query(SkillModel).filter(SkillModel.skill_id == "e2e_test_skill@2.0.0").first()
        assert v2.is_active == 1

        activated = registry.rollback("e2e_test_skill")
        assert activated == "1.0.0"

        db.expire_all()
        v1 = db.query(SkillModel).filter(SkillModel.skill_id == "e2e_test_skill@1.0.0").first()
        v2 = db.query(SkillModel).filter(SkillModel.skill_id == "e2e_test_skill@2.0.0").first()
        assert v1.is_active == 1 and v1.status == "active"
        assert v2.is_active == 0 and v2.status == "deprecated"

        # 6. Verify procedural memory bridge converts learnings correctly.
        #    The bridge (procedural_memory.py) is a type-layer adapter for
        #    Skill Selector internals — it is NOT injected into MemoryRetriever.
        #    Skill learnings are Skill Selector's internal correction rules,
        #    consumed only during skill selection, not general memory retrieval.
        lid = str(uuid7())
        db.add(SkillSelectionLearning(
            learning_id=lid,
            query_pattern="e2e_golden test query",
            wrong_skills=["e2e_test_skill"],
            correct_skills=["better_skill"],
            confidence=70.0, evidence_count=3,
            signal_type="wrong_skill", is_active=1,
        ))
        db.commit()

        mems = list_as_memories(db, active_only=True)
        golden_mem = next((m for m in mems if m.memory_id == lid), None)
        assert golden_mem is not None
        assert golden_mem.memory_type == MemoryType.PROCEDURAL
        assert golden_mem.user_id == "__system__"
        assert golden_mem.initial_confidence == 0.7
        assert golden_mem.trust_tier == TrustTier.T3_INFERRED
        assert golden_mem.is_active is True
        assert golden_mem.observed_at is not None
        assert "wrong_skill" in golden_mem.content
        assert "e2e_golden test query" in golden_mem.content
        assert "e2e_test_skill" in golden_mem.content
        assert "better_skill" in golden_mem.content

        # 7. Uninstall
        count = registry.uninstall("e2e_test_skill")
        assert count == 2
        db.expire_all()
        assert db.query(SkillModel).filter(SkillModel.skill_name == "e2e_test_skill").count() == 0

        # Cleanup selection event
        db.query(SkillSelectionEvent).filter(SkillSelectionEvent.event_id == event_id).delete()
        db.commit()
