"""Integration tests for ToolRegistry + SkillCatalog + DB pipeline.

Layers:
  1. SkillCatalog.register() → DB field-level verification
  2. ToolRegistry built from SkillCatalog → pinned/dynamic split
  3. ToolRegistry.select() → correct tools for queries
  4. ChatLoop wiring → ToolRegistry schemas reach LLM
"""

import pytest
from unittest.mock import MagicMock

from api.models.skill import SkillRegistry as SkillRegistryModel
from core.skills.base import Skill, SkillInput, SkillOutput
from core.skills.catalog import SkillCatalog
from core.skills.tool_registry import ToolRegistry, ToolSource


# ── Test Skills ──────────────────────────────────────────────────


class _In(SkillInput):
    repo: str = ""


class _Out(SkillOutput):
    items: list = []


class ListPRsSkill(Skill[_In, _Out]):
    name = "test_list_prs"
    version = "1.0.0"
    description = "List pull requests in a GitHub repository"

    async def execute(self, inp: _In) -> _Out:
        return _Out(success=True, result="ok", items=[])


class CIStatusSkill(Skill[_In, _Out]):
    name = "test_ci_status"
    version = "2.1.0"
    description = "Check CI/CD workflow status"

    async def execute(self, inp: _In) -> _Out:
        return _Out(success=True, result="ok", items=[])


class CreateIssueSkill(Skill[_In, _Out]):
    name = "test_create_issue"
    version = "1.0.0"
    description = "Create a new GitHub issue"

    async def execute(self, inp: _In) -> _Out:
        return _Out(success=True, result="ok", items=[])


# ── Layer 1: SkillCatalog → DB Persistence ───────────────────────


class TestCatalogDBPersistence:
    """Verify every field written by SkillCatalog.register()."""

    def test_register_persists_all_fields(self, db, db_factory):
        catalog = SkillCatalog(db_factory)
        catalog.register(
            ListPRsSkill(),
            source="builtin",
            category="github",
            subcategory="pr_management",
            triggers=["pr", "pull request"],
            dependencies=["github_token"],
            priority=8,
            cost_estimate="medium",
            status="active",
            tags={"scope": "external", "data_source": "external_api",
                  "intent_type": ["fetch"], "requires_history": False},
        )

        row = db.query(SkillRegistryModel).filter_by(skill_id="test_list_prs@1.0.0").first()
        assert row is not None

        assert row.skill_id == "test_list_prs@1.0.0"
        assert row.skill_name == "test_list_prs"
        assert row.version == "1.0.0"
        assert row.description == "List pull requests in a GitHub repository"
        assert row.is_active == 1
        assert row.status == "active"
        assert row.source == "builtin"
        assert row.category == "github"
        assert row.subcategory == "pr_management"
        assert row.priority == 8
        assert row.cost_estimate == "medium"
        assert row.code_hash is not None and len(row.code_hash) == 64
        assert row.triggers == ["pr", "pull request"]
        assert row.dependencies == ["github_token"]
        assert row.tags["scope"] == "external"
        assert row.tags["intent_type"] == ["fetch"]
        assert row.tags["requires_history"] is False
        assert row.created_at is not None

    def test_upsert_updates_not_duplicates(self, db, db_factory):
        catalog = SkillCatalog(db_factory)
        catalog.register(ListPRsSkill(), category="github")
        catalog.register(ListPRsSkill(), category="github_v2")

        rows = db.query(SkillRegistryModel).filter_by(skill_id="test_list_prs@1.0.0").all()
        assert len(rows) == 1
        assert rows[0].category == "github_v2"

    def test_draft_status_sets_inactive(self, db, db_factory):
        catalog = SkillCatalog(db_factory)
        catalog.register(CIStatusSkill(), status="draft")

        row = db.query(SkillRegistryModel).filter_by(skill_name="test_ci_status").first()
        assert row.is_active == 0
        assert row.status == "draft"

    def test_invalid_status_raises(self, db_factory):
        with pytest.raises(ValueError, match="Invalid status"):
            SkillCatalog(db_factory).register(ListPRsSkill(), status="bogus")

    def test_invalid_source_raises(self, db_factory):
        with pytest.raises(ValueError, match="Invalid source"):
            SkillCatalog(db_factory).register(ListPRsSkill(), source="bogus")


# ── Layer 2: ToolRegistry from SkillCatalog ──────────────────────


class TestRegistryFromCatalog:
    """Build ToolRegistry from SkillCatalog skills."""

    @pytest.fixture
    def catalog(self, db_factory) -> SkillCatalog:
        c = SkillCatalog(db_factory)
        c.register(ListPRsSkill(), category="github")
        c.register(CIStatusSkill(), category="github")
        c.register(CreateIssueSkill(), category="github")
        return c

    def test_all_skills_registered(self, catalog):
        reg = ToolRegistry()
        for s in catalog.list_skills():
            reg.register_skill(s, source=ToolSource.CLOUD)
        assert reg.size == 3
        assert reg.get("test_list_prs") is not None
        assert reg.get("test_ci_status") is not None
        assert reg.get("test_create_issue") is not None

    def test_cloud_skills_are_dynamic(self, catalog):
        reg = ToolRegistry()
        for s in catalog.list_skills():
            reg.register_skill(s, source=ToolSource.CLOUD)
        assert len(reg.pinned_tools()) == 0
        assert len(reg.dynamic_tools()) == 3

    def test_edge_tools_pinned(self, catalog):
        reg = ToolRegistry()
        for name in ("bash", "grep", "read_file"):
            reg.register_schema(
                {"type": "function", "function": {"name": name, "description": name, "parameters": {}}},
                source=ToolSource.EDGE,
            )
        for s in catalog.list_skills():
            reg.register_skill(s, source=ToolSource.CLOUD)

        assert {t.name for t in reg.pinned_tools()} == {"bash", "grep", "read_file"}
        assert len(reg.dynamic_tools()) == 3

    def test_schema_structure(self, catalog):
        reg = ToolRegistry()
        for s in catalog.list_skills():
            reg.register_skill(s, source=ToolSource.CLOUD)

        entry = reg.get("test_list_prs")
        assert entry.schema["type"] == "function"
        assert entry.schema["function"]["name"] == "test_list_prs"
        assert "parameters" in entry.schema["function"]


# ── Layer 3: ToolRegistry.select() ───────────────────────────────


class TestRegistrySelect:
    """Verify select() returns correct tools."""

    @pytest.fixture
    def registry(self, db_factory) -> ToolRegistry:
        catalog = SkillCatalog(db_factory)
        catalog.register(ListPRsSkill(), category="github")
        catalog.register(CIStatusSkill(), category="github")
        catalog.register(CreateIssueSkill(), category="github")

        reg = ToolRegistry(max_tokens=50000)
        for name in ("bash", "read_file", "grep"):
            reg.register_schema(
                {"type": "function", "function": {"name": name, "description": f"{name} tool", "parameters": {}}},
                source=ToolSource.EDGE,
            )
        for s in catalog.list_skills():
            reg.register_skill(s, source=ToolSource.CLOUD, category="github")
        return reg

    def test_pinned_always_included(self, registry):
        names = {s["function"]["name"] for s in registry.select("random")}
        assert {"bash", "read_file", "grep"}.issubset(names)

    def test_all_tools_within_budget(self, registry):
        assert len(registry.select("test")) == 6

    def test_max_dynamic_enforced(self, db_factory):
        catalog = SkillCatalog(db_factory)
        catalog.register(ListPRsSkill(), category="github")
        catalog.register(CIStatusSkill(), category="github")
        catalog.register(CreateIssueSkill(), category="github")

        reg = ToolRegistry(max_dynamic=1, max_tokens=50000)
        for s in catalog.list_skills():
            reg.register_skill(s, source=ToolSource.CLOUD)

        assert len(reg.select("test")) == 1  # 0 pinned + 1 dynamic

    def test_embedding_selects_most_relevant(self, db_factory):
        catalog = SkillCatalog(db_factory)
        catalog.register(ListPRsSkill(), category="github")
        catalog.register(CIStatusSkill(), category="github")

        def fake_embed(text: str) -> list[float]:
            return [1.0, 0.0] if "pr" in text.lower() or "pull" in text.lower() else [0.0, 1.0]

        reg = ToolRegistry(embed_fn=fake_embed, max_dynamic=1, max_tokens=50000)
        for s in catalog.list_skills():
            reg.register_skill(s, source=ToolSource.CLOUD)

        names = {s["function"]["name"] for s in reg.select("show pull requests")}
        assert "test_list_prs" in names
        assert "test_ci_status" not in names

    def test_conversation_history_prefilter(self, registry):
        messages = [
            {"role": "user", "content": "show prs"},
            {"role": "assistant", "content": "done",
             "tool_calls": [{"function": {"name": "test_list_prs", "arguments": "{}"}}]},
            {"role": "user", "content": "now check CI"},
        ]
        result = registry.select("now check CI", messages)
        assert len(result) >= 3  # prefilter reorders, doesn't remove


# ── Layer 4: ChatLoop Wiring ─────────────────────────────────────


class TestChatLoopWiring:
    """ChatLoop uses ToolRegistry to select tools for LLM."""

    def test_tool_registry_is_wired(self, db_factory):
        from core.agent.chat_loop import ChatLoop

        catalog = SkillCatalog(db_factory)
        catalog.register(ListPRsSkill(), category="github")

        reg = ToolRegistry(max_tokens=50000)
        reg.register_schema(
            {"type": "function", "function": {"name": "bash", "description": "Run shell", "parameters": {}}},
            source=ToolSource.EDGE,
        )
        for s in catalog.list_skills():
            reg.register_skill(s, source=ToolSource.CLOUD)

        loop = ChatLoop(
            selector=reg,
            executor=MagicMock(),
            llm_client=MagicMock(),
            event_logger=MagicMock(),
            context_manager=MagicMock(),
            firewall=MagicMock(),
        )
        assert isinstance(loop._tool_registry, ToolRegistry)

    def test_select_returns_valid_schemas(self, db_factory):
        catalog = SkillCatalog(db_factory)
        catalog.register(ListPRsSkill(), category="github")

        reg = ToolRegistry(max_tokens=50000)
        reg.register_schema(
            {"type": "function", "function": {"name": "bash", "description": "Run shell", "parameters": {}}},
            source=ToolSource.EDGE,
        )
        for s in catalog.list_skills():
            reg.register_skill(s, source=ToolSource.CLOUD)

        schemas = reg.select("show prs")
        assert len(schemas) >= 1
        for s in schemas:
            assert s["type"] == "function"
            assert "name" in s["function"]
