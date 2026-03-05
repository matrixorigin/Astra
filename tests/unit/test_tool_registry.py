"""Tests for core/skills/tool_registry.py — ToolRegistry selection pipeline."""

import pytest

from core.skills.tool_registry import (
    ToolEntry,
    ToolRegistry,
    ToolSource,
    _DEFAULT_PINNED,
    _cosine_similarity,
)


# ── Fixtures ─────────────────────────────────────────────────────

def _schema(name: str, desc: str = "") -> dict:
    return {"type": "function", "function": {"name": name, "description": desc, "parameters": {}}}


def _entry(name: str, pinned: bool = False, source: ToolSource = ToolSource.CLOUD, desc: str = "", category: str = "") -> ToolEntry:
    return ToolEntry(name=name, description=desc or name, schema=_schema(name, desc or name), source=source, pinned=pinned, category=category)


@pytest.fixture
def registry() -> ToolRegistry:
    r = ToolRegistry(max_tokens=5000)
    r.register(_entry("bash", pinned=True, source=ToolSource.EDGE))
    r.register(_entry("read_file", pinned=True, source=ToolSource.EDGE))
    r.register(_entry("list_prs", category="github"))
    r.register(_entry("ci_status", category="github"))
    r.register(_entry("create_issue", category="github"))
    return r


# ── Registration ─────────────────────────────────────────────────

class TestRegistration:
    def test_register_and_get(self):
        r = ToolRegistry()
        e = _entry("bash")
        r.register(e)
        assert r.get("bash") is e
        assert r.size == 1

    def test_register_overwrites(self):
        r = ToolRegistry()
        r.register(_entry("bash", desc="v1"))
        r.register(_entry("bash", desc="v2"))
        assert r.get("bash").description == "v2"
        assert r.size == 1

    def test_register_schema(self):
        r = ToolRegistry()
        r.register_schema(_schema("grep", "search"), source=ToolSource.EDGE)
        assert r.get("grep") is not None
        assert r.get("grep").source == ToolSource.EDGE

    def test_register_schema_auto_pinned(self):
        r = ToolRegistry()
        r.register_schema(_schema("bash"), source=ToolSource.EDGE)
        assert r.get("bash").pinned is True

    def test_register_schema_not_pinned(self):
        r = ToolRegistry()
        r.register_schema(_schema("list_prs"), source=ToolSource.CLOUD)
        assert r.get("list_prs").pinned is False

    def test_unregister(self):
        r = ToolRegistry()
        r.register(_entry("bash"))
        r.unregister("bash")
        assert r.get("bash") is None
        assert r.size == 0

    def test_clear_all(self):
        r = ToolRegistry()
        r.register(_entry("a", source=ToolSource.EDGE))
        r.register(_entry("b", source=ToolSource.CLOUD))
        r.clear()
        assert r.size == 0

    def test_clear_by_source(self):
        r = ToolRegistry()
        r.register(_entry("a", source=ToolSource.EDGE))
        r.register(_entry("b", source=ToolSource.CLOUD))
        r.clear(source=ToolSource.CLOUD)
        assert r.size == 1
        assert r.get("a") is not None


# ── Pinned vs Dynamic ───────────────────────────────────────────

class TestPinnedDynamic:
    def test_pinned_tools(self, registry):
        pinned = registry.pinned_tools()
        assert {t.name for t in pinned} == {"bash", "read_file"}

    def test_dynamic_tools(self, registry):
        dynamic = registry.dynamic_tools()
        assert {t.name for t in dynamic} == {"list_prs", "ci_status", "create_issue"}

    def test_default_pinned_names(self):
        assert "bash" in _DEFAULT_PINNED
        assert "read_file" in _DEFAULT_PINNED
        assert "grep" in _DEFAULT_PINNED


# ── Selection ────────────────────────────────────────────────────

class TestSelect:
    def test_select_includes_all_pinned(self, registry):
        result = registry.select("hello")
        names = {t["function"]["name"] for t in result}
        assert "bash" in names
        assert "read_file" in names

    def test_select_includes_dynamic(self, registry):
        result = registry.select("show me prs")
        names = {t["function"]["name"] for t in result}
        assert "list_prs" in names

    def test_select_no_query_returns_pinned_only_when_no_dynamic(self):
        r = ToolRegistry()
        r.register(_entry("bash", pinned=True))
        result = r.select("")
        assert len(result) == 1
        assert result[0]["function"]["name"] == "bash"

    def test_select_empty_registry(self):
        r = ToolRegistry()
        assert r.select("anything") == []

    def test_select_respects_max_dynamic(self):
        r = ToolRegistry(max_dynamic=2, max_tokens=50000)
        r.register(_entry("bash", pinned=True))
        for i in range(10):
            r.register(_entry(f"tool_{i}"))
        result = r.select("query")
        # 1 pinned + 2 dynamic max
        assert len(result) == 3

    def test_select_returns_schemas(self, registry):
        result = registry.select("test")
        for schema in result:
            assert "type" in schema
            assert "function" in schema
            assert "name" in schema["function"]


# ── Token Budget ─────────────────────────────────────────────────

class TestTokenBudget:
    def test_pinned_never_dropped(self):
        r = ToolRegistry(max_tokens=1)  # Tiny budget
        r.register(_entry("bash", pinned=True))
        r.register(_entry("list_prs"))
        result = r.select("test")
        names = {t["function"]["name"] for t in result}
        assert "bash" in names  # Pinned survives

    def test_dynamic_dropped_when_over_budget(self):
        r = ToolRegistry(max_tokens=50)
        r.register(_entry("bash", pinned=True))
        r.register(_entry("tool_a"))
        r.register(_entry("tool_b"))
        r.register(_entry("tool_c"))
        result = r.select("test")
        names = {t["function"]["name"] for t in result}
        # Pinned always survives, some dynamic dropped
        assert "bash" in names
        assert len(result) < 4

    def test_schema_tokens_estimate(self):
        e = _entry("bash", desc="Run shell commands")
        assert e.schema_tokens > 0


# ── Embedding Selection ──────────────────────────────────────────

class TestEmbeddingSelect:
    def test_with_embed_fn(self):
        """When embed_fn is provided, top-K by similarity."""
        call_log = []

        def fake_embed(text: str) -> list[float]:
            call_log.append(text)
            # "list_prs" and query about PRs both get [1,0], ci_status gets [0,1]
            if "pr" in text.lower() or "pull" in text.lower():
                return [1.0, 0.0]
            return [0.0, 1.0]

        r = ToolRegistry(embed_fn=fake_embed, max_dynamic=1, max_tokens=50000)
        r.register(_entry("bash", pinned=True))
        r.register(_entry("list_prs", desc="List pull requests"))
        r.register(_entry("ci_status", desc="Check CI status"))

        result = r.select("show me pull requests")
        names = {t["function"]["name"] for t in result}
        assert "bash" in names  # pinned
        assert "list_prs" in names  # top-1 by embedding
        assert len(call_log) > 0

    def test_without_embed_fn_truncates(self):
        """Without embed_fn, just truncate to max_dynamic."""
        r = ToolRegistry(max_dynamic=2, max_tokens=50000)
        for i in range(5):
            r.register(_entry(f"tool_{i}"))
        result = r.select("query")
        assert len(result) == 2


# ── Cosine Similarity ────────────────────────────────────────────

class TestCosineSimilarity:
    def test_identical_vectors(self):
        assert _cosine_similarity([1, 0], [1, 0]) == pytest.approx(1.0)

    def test_orthogonal_vectors(self):
        assert _cosine_similarity([1, 0], [0, 1]) == pytest.approx(0.0)

    def test_opposite_vectors(self):
        assert _cosine_similarity([1, 0], [-1, 0]) == pytest.approx(-1.0)

    def test_zero_vector(self):
        assert _cosine_similarity([0, 0], [1, 0]) == 0.0

    def test_different_lengths(self):
        assert _cosine_similarity([1], [1, 0]) == 0.0


# ── Prefilter Integration ────────────────────────────────────────

class TestPrefilterIntegration:
    def test_prefilter_reorders_for_history_query(self):
        """History+analytical query should prefer historical-scoped tools."""
        r = ToolRegistry(max_tokens=50000)
        r.register(_entry("bash", pinned=True, source=ToolSource.EDGE))
        r.register(_entry("list_prs", category="github"))
        r.register(_entry("event_reader", category="system"))

        messages = [
            {"role": "user", "content": "list prs"},
            {"role": "assistant", "content": "done", "tool_calls": [
                {"function": {"name": "list_prs", "arguments": "{}"}}
            ]},
            {"role": "user", "content": "分析一下前一个上下文"},
        ]
        result = r.select("分析一下前一个上下文", messages)
        # Should still return tools (prefilter reorders, doesn't remove)
        assert len(result) >= 1

    def test_no_messages_no_crash(self):
        r = ToolRegistry(max_tokens=50000)
        r.register(_entry("bash", pinned=True))
        r.register(_entry("list_prs"))
        result = r.select("test", None)
        assert len(result) == 2


# ── register_skill ───────────────────────────────────────────────

class TestRegisterSkill:
    def test_register_skill_from_base(self):
        """Register a Skill instance via register_skill()."""
        from unittest.mock import MagicMock
        skill = MagicMock()
        skill.name = "test_skill"
        skill.description = "A test skill"
        skill.to_openai_schema.return_value = _schema("test_skill", "A test skill")

        r = ToolRegistry()
        r.register_skill(skill, source=ToolSource.CLOUD, category="test")
        entry = r.get("test_skill")
        assert entry is not None
        assert entry.source == ToolSource.CLOUD
        assert entry.category == "test"
        assert entry.pinned is False  # test_skill not in default pinned

    def test_register_skill_auto_pinned(self):
        from unittest.mock import MagicMock
        skill = MagicMock()
        skill.name = "bash"
        skill.description = "Run shell"
        skill.to_openai_schema.return_value = _schema("bash")

        r = ToolRegistry()
        r.register_skill(skill, source=ToolSource.EDGE)
        assert r.get("bash").pinned is True
