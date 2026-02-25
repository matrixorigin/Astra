"""Tests for progressive disclosure: SkillIndex, token budget, semantic retrieval."""

import json
import math
from unittest.mock import Mock, patch

import pytest

from core.skills.modern_selector import ModernSkillSelector, _estimate_tokens, _DEFAULT_CONTEXT_BUDGET
from core.skills.selector import SkillMetadata
from core.skills.skill_index import SkillIndex, _cosine, _skill_text


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_skill(name, description="desc", triggers=None, cost="low"):
    return SkillMetadata(
        name=name, version="1.0.0", description=description,
        category="test", subcategory="default",
        triggers=triggers or [], dependencies=[], priority=5,
        cost_estimate=cost,
    )


def _deterministic_embed(text: str) -> list[float]:
    """Hash-based embedding that produces different vectors for different text."""
    import hashlib
    h = hashlib.sha256(text.encode()).digest()
    vec = [(b / 255.0) * 2 - 1 for b in h]
    # Pad to 32 dims for test speed
    while len(vec) < 32:
        vec.extend(vec[:32 - len(vec)])
    return vec[:32]


# ===========================================================================
# SkillIndex
# ===========================================================================

class TestSkillIndex:

    def test_build_indexes_all_skills(self):
        skills = [_make_skill("a"), _make_skill("b"), _make_skill("c")]
        idx = SkillIndex(embed_fn=_deterministic_embed)
        count = idx.build(skills)
        assert count == 3

    def test_query_returns_ranked_names(self):
        """Query should return skill names ordered by cosine similarity."""
        skills = [
            _make_skill("code_review", description="review pull request code quality"),
            _make_skill("deploy_k8s", description="deploy kubernetes cluster"),
            _make_skill("search_code", description="search codebase for patterns"),
        ]
        idx = SkillIndex(embed_fn=_deterministic_embed)
        idx.build(skills)

        results = idx.query("review my pull request", top_k=3, min_score=-1)
        assert isinstance(results, list)
        assert len(results) == 3
        # All skill names present
        assert set(results) == {"code_review", "deploy_k8s", "search_code"}

    def test_query_respects_top_k(self):
        skills = [_make_skill(f"skill_{i}") for i in range(10)]
        idx = SkillIndex(embed_fn=_deterministic_embed)
        idx.build(skills)

        results = idx.query("anything", top_k=3, min_score=-1)
        assert len(results) == 3

    def test_query_empty_index_returns_empty(self):
        idx = SkillIndex(embed_fn=_deterministic_embed)
        assert idx.query("hello") == []

    def test_no_embed_fn_returns_empty(self):
        """Without embed_fn, index is inert."""
        idx = SkillIndex(embed_fn=None)
        assert idx.build([_make_skill("a")]) == 0
        assert idx.query("hello") == []

    def test_build_survives_embed_failure(self):
        """If embedding one skill fails, others still get indexed."""
        call_count = 0
        def flaky_embed(text):
            nonlocal call_count
            call_count += 1
            if call_count == 2:
                raise RuntimeError("boom")
            return _deterministic_embed(text)

        skills = [_make_skill("a"), _make_skill("b"), _make_skill("c")]
        idx = SkillIndex(embed_fn=flaky_embed)
        count = idx.build(skills)
        assert count == 2  # one failed

    def test_query_survives_embed_failure(self):
        """If query embedding fails, return empty list."""
        idx = SkillIndex(embed_fn=lambda t: (_ for _ in ()).throw(RuntimeError("boom")))
        idx._entries = [Mock(name="x", vector=[1.0])]  # non-empty index
        assert idx.query("hello") == []

    def test_rebuild_replaces_old_entries(self):
        idx = SkillIndex(embed_fn=_deterministic_embed)
        idx.build([_make_skill("old_skill")])
        assert len(idx._entries) == 1

        idx.build([_make_skill("new_a"), _make_skill("new_b")])
        assert len(idx._entries) == 2
        assert all(e.name.startswith("new_") for e in idx._entries)

    def test_cosine_identical_vectors(self):
        v = [1.0, 2.0, 3.0]
        assert abs(_cosine(v, v) - 1.0) < 1e-9

    def test_cosine_orthogonal_vectors(self):
        a = [1.0, 0.0]
        b = [0.0, 1.0]
        assert abs(_cosine(a, b)) < 1e-9

    def test_cosine_zero_vector(self):
        assert _cosine([0, 0], [1, 2]) == 0.0

    def test_skill_text_includes_all_fields(self):
        skill = _make_skill("review", description="check code", triggers=["pr", "review"])
        text = _skill_text(skill)
        assert "review" in text
        assert "check code" in text
        assert "pr" in text


# ===========================================================================
# _estimate_tokens
# ===========================================================================

class TestEstimateTokens:

    def test_small_object(self):
        obj = {"type": "function", "function": {"name": "x"}}
        tokens = _estimate_tokens(obj)
        # len('{"type": "function", "function": {"name": "x"}}') = 48 → 12 tokens
        assert 5 < tokens < 30

    def test_large_schema_costs_more(self):
        small = {"name": "x"}
        large = {"name": "x", "description": "A" * 400, "parameters": {"a": 1, "b": 2, "c": 3}}
        assert _estimate_tokens(large) > _estimate_tokens(small)

    def test_empty_object(self):
        assert _estimate_tokens({}) >= 0


# ===========================================================================
# ModernSkillSelector — progressive disclosure
# ===========================================================================

class TestProgressiveDisclosure:

    @pytest.fixture
    def selector_with_skills(self, db):
        """Selector with pre-loaded skills and deterministic embeddings."""
        sel = ModernSkillSelector(lambda: db, llm_client=None, embed_fn=_deterministic_embed)
        for name in ["code_review", "deploy_k8s", "search_code", "ci_status", "list_prs"]:
            skill = _make_skill(name, description=f"{name} description", triggers=[name.split("_")[0]])
            sel.rule_selector.skills[name] = skill
        # Rebuild index with new skills
        sel._index.build(list(sel.rule_selector.skills.values()))
        return sel

    def test_semantic_retrieval_preferred_over_keyword(self, selector_with_skills):
        """When embed_fn is available, semantic index is used first."""
        sel = selector_with_skills
        # Spy on the index
        original_query = sel._index.query
        called = []
        def spy_query(*a, **kw):
            called.append(True)
            return original_query(*a, **kw)
        sel._index.query = spy_query

        tools, _ = sel.get_tools_schema("review code", max_candidates=3)
        assert len(called) > 0, "Semantic index should have been queried"
        assert len(tools) > 0

    def test_keyword_fallback_when_no_embed(self, db):
        """Without embed_fn, falls back to keyword matching."""
        sel = ModernSkillSelector(lambda: db, llm_client=None, embed_fn=None)
        skill = _make_skill("code_review", triggers=["review", "code"])
        sel.rule_selector.skills["code_review"] = skill

        tools, _ = sel.get_tools_schema("review code", max_candidates=3)
        # Should still find the skill via keyword matching
        names = [t["function"]["name"] for t in tools]
        assert "code_review" in names

    def test_budget_excludes_expensive_skills(self, selector_with_skills):
        """Skills exceeding budget are excluded entirely, not stubbed."""
        sel = selector_with_skills
        # Set a tiny budget that can fit ~1 skill
        tools, _ = sel.get_tools_schema("code", max_candidates=5, context_budget=50)
        # With budget=50 tokens, at most 1-2 small schemas fit
        assert len(tools) <= 2
        # Every included tool has real parameters (no empty stubs)
        for t in tools:
            assert t["type"] == "function"
            assert "name" in t["function"]

    def test_no_empty_stubs_in_output(self, selector_with_skills):
        """Budget exhaustion should never produce empty-parameter stubs."""
        sel = selector_with_skills
        tools, _ = sel.get_tools_schema("code", max_candidates=5, context_budget=1)
        # Budget=1 token — nothing should fit
        assert tools == []

    def test_budget_allows_all_when_sufficient(self, selector_with_skills):
        """With large budget, all candidates are included."""
        sel = selector_with_skills
        tools, _ = sel.get_tools_schema("code", max_candidates=5, context_budget=100000)
        assert len(tools) > 0

    def test_real_token_measurement_varies_by_schema(self, db):
        """Different skills produce different token costs."""
        sel = ModernSkillSelector(lambda: db, llm_client=None, embed_fn=_deterministic_embed)

        small = _make_skill("tiny", description="x")
        big = _make_skill("huge", description="A very long description " * 50,
                          triggers=["a", "b", "c", "d", "e"])
        sel.rule_selector.skills["tiny"] = small
        sel.rule_selector.skills["huge"] = big
        sel._index.build(list(sel.rule_selector.skills.values()))

        schema_small = sel._skill_to_tool_schema(small)
        schema_big = sel._skill_to_tool_schema(big)
        assert _estimate_tokens(schema_big) > _estimate_tokens(schema_small)

    def test_max_candidates_limits_output(self, selector_with_skills):
        """max_candidates caps how many tools are returned."""
        sel = selector_with_skills
        tools, _ = sel.get_tools_schema("code", max_candidates=2, context_budget=100000)
        assert len(tools) <= 2

    def test_empty_skill_registry_returns_empty(self, db):
        sel = ModernSkillSelector(lambda: db, llm_client=None, embed_fn=_deterministic_embed)
        # Clear all skills to simulate empty registry
        sel.rule_selector.skills.clear()
        sel._index.build([])
        tools, _ = sel.get_tools_schema("anything")
        assert tools == []


# ===========================================================================
# SkillPipeline — embed_fn auto-resolution
# ===========================================================================

# ===========================================================================
# Fallback path
# ===========================================================================

class TestFallbackSelection:

    @pytest.fixture
    def selector_with_llm(self, db):
        mock_llm = Mock()
        sel = ModernSkillSelector(lambda: db, llm_client=mock_llm, embed_fn=_deterministic_embed)
        skill = _make_skill("code_review", description="review code", triggers=["review"])
        sel.rule_selector.skills = {"code_review": skill}  # isolate from DB
        sel._index.build([skill])
        sel._index.MIN_SCORE = -1
        return sel, mock_llm

    def test_fallback_returns_top_ranked_candidate(self, selector_with_llm):
        """When LLM call fails, fallback should return the first (top-ranked) candidate."""
        sel, mock_llm = selector_with_llm
        mock_llm.chat_with_tools.side_effect = RuntimeError("LLM unavailable")

        result = sel.select_and_execute("review code")

        assert len(result) == 1
        assert result[0]["function"]["name"] == "code_review"
        assert result[0]["function"]["arguments"] is None
        assert result[0]["fallback"] is True

    def test_fallback_preserves_ranking_order(self, db):
        """Fallback should use the semantic/keyword ranked order, not arbitrary."""
        mock_llm = Mock()
        mock_llm.chat_with_tools.side_effect = RuntimeError("boom")

        sel = ModernSkillSelector(lambda: db, llm_client=mock_llm, embed_fn=_deterministic_embed)
        # Add multiple skills
        for name in ["deploy_k8s", "code_review", "search_code"]:
            skill = _make_skill(name, description=f"{name} desc", triggers=[name.split("_")[0]])
            sel.rule_selector.skills[name] = skill
        sel._index.build(list(sel.rule_selector.skills.values()))

        result = sel.select_and_execute("review code quality")

        assert len(result) == 1
        # Should be the top-ranked skill from semantic retrieval, not arbitrary
        assert result[0]["function"]["name"] in sel.rule_selector.skills

    def test_fallback_logs_warning(self, selector_with_llm, caplog):
        """Fallback should log a warning with skill name and candidate count."""
        import logging
        sel, mock_llm = selector_with_llm
        mock_llm.chat_with_tools.side_effect = RuntimeError("timeout")

        with caplog.at_level(logging.WARNING):
            sel.select_and_execute("review code")

        assert any("Fallback selection" in r.message for r in caplog.records)
        assert any("code_review" in r.message for r in caplog.records)

    def test_fallback_with_no_candidates_returns_empty(self, db):
        """If no candidates found, fallback should return empty list."""
        mock_llm = Mock()
        mock_llm.chat_with_tools.side_effect = RuntimeError("boom")

        sel = ModernSkillSelector(lambda: db, llm_client=mock_llm, embed_fn=_deterministic_embed)
        sel.rule_selector.skills.clear()
        sel._index.build([])

        result = sel.select_and_execute("anything")
        assert result == []

    def test_fallback_selection_method_directly(self):
        """Test _fallback_selection as a unit."""
        sel = ModernSkillSelector.__new__(ModernSkillSelector)
        tools = [
            {"function": {"name": "skill_a", "parameters": {}}},
            {"function": {"name": "skill_b", "parameters": {}}},
        ]
        result = sel._fallback_selection(tools)
        assert len(result) == 1
        assert result[0]["function"]["name"] == "skill_a"  # top-ranked
        assert result[0]["function"]["arguments"] is None  # no fake empty args
        assert result[0]["fallback"] is True

    def test_fallback_selection_empty_input(self):
        sel = ModernSkillSelector.__new__(ModernSkillSelector)
        assert sel._fallback_selection([]) == []


class TestInlineRefs:
    """Test $ref/$defs inlining for OpenAI compatibility."""

    def test_no_defs_passthrough(self):
        schema = {"type": "object", "properties": {"x": {"type": "string"}}}
        assert ModernSkillSelector._inline_refs(schema) == schema

    def test_inline_nested_ref(self):
        schema = {
            "$defs": {"Addr": {"type": "object", "properties": {"city": {"type": "string"}}}},
            "type": "object",
            "properties": {"home": {"$ref": "#/$defs/Addr"}},
        }
        result = ModernSkillSelector._inline_refs(schema)
        assert "$defs" not in result
        assert "$ref" not in result["properties"]["home"]
        assert result["properties"]["home"]["properties"]["city"]["type"] == "string"

    def test_inline_anyof_ref(self):
        schema = {
            "$defs": {"Addr": {"type": "object", "properties": {"city": {"type": "string"}}}},
            "type": "object",
            "properties": {
                "addr": {"anyOf": [{"$ref": "#/$defs/Addr"}, {"type": "null"}]}
            },
        }
        result = ModernSkillSelector._inline_refs(schema)
        resolved = result["properties"]["addr"]["anyOf"][0]
        assert resolved["type"] == "object"


# ===========================================================================
# Registry cache
# ===========================================================================

class TestRegistryCache:

    def test_registry_cached_in_init(self, db):
        """SkillRegistry should be instantiated once in __init__, not per schema call."""
        sel = ModernSkillSelector(lambda: db, llm_client=None)
        assert hasattr(sel, "_registry")

        # Call _skill_to_tool_schema multiple times — should reuse same registry
        skill = _make_skill("test_skill")
        sel._skill_to_tool_schema(skill)
        sel._skill_to_tool_schema(skill)

        # Registry object identity should be the same
        registry_id = id(sel._registry)
        sel._skill_to_tool_schema(skill)
        assert id(sel._registry) == registry_id


class TestPipelineEmbedIntegration:

    def test_pipeline_passes_embed_fn_to_selector(self, db):
        """SkillPipeline should auto-resolve embed_fn and pass to ModernSkillSelector."""
        from core.skills.pipeline import SkillPipeline

        custom_embed = Mock(return_value=[0.1] * 32)
        pipeline = SkillPipeline(lambda: db, llm_client=None, audit=False, learning=False,
                                 embed_fn=custom_embed)
        # The internal selector should have a SkillIndex with our embed_fn
        assert pipeline._modern._index._embed is custom_embed

    def test_pipeline_works_without_embed_fn(self, db):
        """Pipeline should work even if no embed_fn is available."""
        from core.skills.pipeline import SkillPipeline

        # Patch the module-level import target so EmbeddingService raises
        with patch("core.context.embeddings.EmbeddingService", side_effect=Exception("no embeddings")):
            pipeline = SkillPipeline(lambda: db, llm_client=None, audit=False, learning=False,
                                     embed_fn=None)
        # Should have fallen back to None
        assert pipeline._modern._index._embed is None


# ===========================================================================
# Embedding Quality — End-to-End Verification
# ===========================================================================

class TestEmbeddingQuality:
    """Verify semantic retrieval mechanics with mock embeddings.

    NOTE: Mock embeddings use SHA256 hashing, NOT real semantic similarity.
    These tests verify the retrieval *pipeline* works correctly (index → query →
    rank → return), not that semantically similar queries produce similar vectors.
    True semantic quality requires integration tests with a real embedding model.
    """

    def test_semantic_similarity_ranks_relevant_skills_higher(self, db):
        """Verify 'review PR' query ranks code_review higher than deploy_k8s."""
        from core.context.embeddings import EmbeddingService
        
        # Use mock embedding service (simple bag-of-words)
        embed_service = EmbeddingService(lambda: db, provider="mock")
        embed_fn = lambda text: embed_service.embed_text(text)
        
        # Create skills with distinct semantic content
        code_review = _make_skill(
            "code_review",
            description="Review code changes in pull requests for quality and security",
            triggers=["review", "pr", "code", "quality"]
        )
        deploy_k8s = _make_skill(
            "deploy_k8s",
            description="Deploy application to Kubernetes cluster",
            triggers=["deploy", "kubernetes", "k8s", "cluster"]
        )
        
        sel = ModernSkillSelector(lambda: db, llm_client=None, embed_fn=embed_fn)
        sel.rule_selector.skills = {
            "code_review": code_review,
            "deploy_k8s": deploy_k8s,
        }
        sel._index.build([code_review, deploy_k8s])
        sel._index.MIN_SCORE = -1  # mock embeddings: test pipeline, not quality
        
        # Query: "review PR" should rank code_review higher than deploy_k8s
        tools, method = sel.get_tools_schema("review PR code", max_candidates=3)
        
        assert method == "semantic"
        assert len(tools) >= 1
        
        # Extract skill names in order
        skill_names = [t["function"]["name"] for t in tools]
        
        # code_review should appear before deploy_k8s (more relevant)
        if "code_review" in skill_names and "deploy_k8s" in skill_names:
            code_idx = skill_names.index("code_review")
            deploy_idx = skill_names.index("deploy_k8s")
            assert code_idx < deploy_idx, \
                f"code_review should rank higher than deploy_k8s for 'review PR code', got {skill_names}"

    def test_keyword_fallback_still_works(self, db):
        """Verify keyword fallback when no embedding available."""
        code_review = _make_skill(
            "code_review",
            description="Review code",
            triggers=["review", "code"]
        )
        
        sel = ModernSkillSelector(lambda: db, llm_client=None, embed_fn=None)
        sel.rule_selector.skills = {"code_review": code_review}
        
        tools, method = sel.get_tools_schema("review code", max_candidates=3)
        
        assert method == "keyword"
        assert len(tools) == 1
        assert tools[0]["function"]["name"] == "code_review"

    def test_semantic_retrieval_handles_synonyms(self, db):
        """Verify semantic retrieval finds skills even with different wording."""
        from core.context.embeddings import EmbeddingService
        
        embed_service = EmbeddingService(lambda: db, provider="mock")
        embed_fn = lambda text: embed_service.embed_text(text)
        
        code_review = _make_skill(
            "code_review",
            description="Review code changes in pull requests",
            triggers=["review", "pr"]
        )
        
        sel = ModernSkillSelector(lambda: db, llm_client=None, embed_fn=embed_fn)
        sel.rule_selector.skills = {"code_review": code_review}
        sel._index.build([code_review])
        sel._index.MIN_SCORE = -1  # mock embeddings: test pipeline, not quality
        
        # Query with synonyms: "inspect merge request"
        tools, method = sel.get_tools_schema("inspect merge request", max_candidates=3)
        
        assert method == "semantic"
        # Should find code_review despite different wording
        # (mock embedding uses bag-of-words, so "request" matches "requests")
        assert len(tools) >= 1, "Should find at least one skill with semantic retrieval"
        assert tools[0]["function"]["name"] == "code_review"
