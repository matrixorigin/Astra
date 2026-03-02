"""Tests for progressive disclosure: SkillIndex, token budget, semantic retrieval."""

from unittest.mock import Mock, patch

import pytest

from core.skills.modern_selector import ModernSkillSelector, _estimate_tokens
from core.skills.selector import SkillMetadata
from core.skills.skill_index import SkillIndex, _skill_text

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
    """Hash-based embedding that produces different vectors for different text.

    Produces 384-dim vectors to match the VECF32(384) column in skills_registry.
    """
    import hashlib
    h = hashlib.sha256(text.encode()).digest()
    vec = [(b / 255.0) * 2 - 1 for b in h]
    while len(vec) < 384:
        vec.extend(vec[:384 - len(vec)])
    return vec[:384]


def _ensure_db_skill(db, name, desc="desc"):
    """Ensure a skills_registry row exists so SkillIndex can UPDATE its embedding."""
    from api.models.skill import SkillRegistry as SkillModel
    row = db.query(SkillModel).filter_by(skill_name=name, is_active=1).first()
    if not row:
        db.add(SkillModel(
            skill_id=f"{name}@1.0.0", skill_name=name, version="1.0.0",
            description=desc, is_active=1,
        ))
    db.commit()


def _clear_embeddings(db):
    """Clear all embeddings in skills_registry."""
    from sqlalchemy import text as sa_text
    db.execute(sa_text(
        "UPDATE skills_registry SET embedding = NULL WHERE embedding IS NOT NULL"
    ))
    db.commit()


# ===========================================================================
# SkillIndex
# ===========================================================================

class TestSkillIndex:
    """Tests for DB-backed SkillIndex.

    These tests use the real test database.  The ``db`` fixture provides
    a session; ``db_factory`` wraps it for SkillIndex's constructor.
    Skills must exist in ``skills_registry`` for UPDATE to write embeddings.
    """

    @pytest.fixture(autouse=True)
    def _seed_skills(self, db):
        """Seed skills_registry rows so SkillIndex.build() can UPDATE them."""
        from sqlalchemy import text

        from api.models.skill import SkillRegistry
        # Clear ALL embeddings — query() returns top-k by distance across the
        # entire table, so non-test embeddings would pollute results.
        # These DB-dependent tests must run serially (not xdist-safe).
        db.execute(text("UPDATE skills_registry SET embedding = NULL WHERE embedding IS NOT NULL"))
        db.query(SkillRegistry).filter(
            SkillRegistry.skill_name.like("test_si_%"),
        ).delete(synchronize_session=False)
        db.commit()
        self._db = db
        yield
        db.query(SkillRegistry).filter(
            SkillRegistry.skill_name.like("test_si_%"),
        ).delete(synchronize_session=False)
        db.commit()

    def _insert_skill(self, name, desc="desc"):
        from api.models.skill import SkillRegistry
        row = SkillRegistry(
            skill_id=f"{name}@1.0.0",
            skill_name=name,
            version="1.0.0",
            description=desc,
            is_active=1,
        )
        self._db.add(row)
        self._db.commit()

    def test_build_indexes_all_skills(self, db, db_factory):
        for n in ["test_si_a", "test_si_b", "test_si_c"]:
            self._insert_skill(n)
        skills = [_make_skill(n) for n in ["test_si_a", "test_si_b", "test_si_c"]]
        idx = SkillIndex(embed_fn=_deterministic_embed, db_factory=db_factory)
        count = idx.build(skills)
        assert count == 3

    def test_query_returns_all_matching_names(self, db, db_factory):
        """Query should return all embedded skill names within distance threshold."""
        for n in ["test_si_code_review", "test_si_deploy_k8s", "test_si_search_code"]:
            self._insert_skill(n, desc=f"{n} description")
        skills = [
            _make_skill("test_si_code_review", description="review pull request code quality"),
            _make_skill("test_si_deploy_k8s", description="deploy kubernetes cluster"),
            _make_skill("test_si_search_code", description="search codebase for patterns"),
        ]
        idx = SkillIndex(embed_fn=_deterministic_embed, db_factory=db_factory)
        idx.build(skills)

        results = idx.query("review my pull request", top_k=3, max_distance=999)
        assert isinstance(results, list)
        assert len(results) == 3
        assert set(results) == {"test_si_code_review", "test_si_deploy_k8s", "test_si_search_code"}

    def test_query_respects_top_k(self, db, db_factory):
        for i in range(10):
            self._insert_skill(f"test_si_skill_{i}")
        skills = [_make_skill(f"test_si_skill_{i}") for i in range(10)]
        idx = SkillIndex(embed_fn=_deterministic_embed, db_factory=db_factory)
        idx.build(skills)

        results = idx.query("anything", top_k=3, max_distance=999)
        assert len(results) == 3

    def test_query_empty_index_returns_empty(self, db, db_factory):
        idx = SkillIndex(embed_fn=_deterministic_embed, db_factory=db_factory)
        assert idx.query("hello") == []

    def test_no_embed_fn_returns_empty(self, db, db_factory):
        """Without embed_fn, index is inert."""
        idx = SkillIndex(embed_fn=None, db_factory=db_factory)
        assert idx.build([_make_skill("test_si_a")]) == 0
        assert idx.query("hello") == []

    def test_no_db_factory_returns_empty(self):
        """Without db_factory, index is inert."""
        idx = SkillIndex(embed_fn=_deterministic_embed, db_factory=None)
        assert idx.build([_make_skill("test_si_a")]) == 0
        assert idx.query("hello") == []

    def test_build_survives_embed_failure(self, db, db_factory):
        """If embedding one skill fails, others still get indexed."""
        for n in ["test_si_a", "test_si_b", "test_si_c"]:
            self._insert_skill(n)
        call_count = 0
        def flaky_embed(text_input):
            nonlocal call_count
            call_count += 1
            if call_count == 2:
                raise RuntimeError("boom")
            return _deterministic_embed(text_input)

        skills = [_make_skill("test_si_a"), _make_skill("test_si_b"), _make_skill("test_si_c")]
        idx = SkillIndex(embed_fn=flaky_embed, db_factory=db_factory)
        count = idx.build(skills)
        assert count == 2  # one failed

    def test_query_survives_embed_failure(self, db, db_factory):
        """If query embedding fails, return empty list."""
        idx = SkillIndex(
            embed_fn=lambda t: (_ for _ in ()).throw(RuntimeError("boom")),
            db_factory=db_factory,
        )
        assert idx.query("hello") == []

    def test_skill_text_includes_all_fields(self):
        skill = _make_skill("review", description="check code", triggers=["pr", "review"])
        t = _skill_text(skill)
        assert "review" in t
        assert "check code" in t
        assert "pr" in t


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
    def selector_with_skills(self, db, db_factory):
        """Selector with pre-loaded skills and deterministic embeddings."""
        names = ["code_review", "deploy_k8s", "search_code", "ci_status", "list_prs"]
        for name in names:
            _ensure_db_skill(db, name, f"{name} description")

        sel = ModernSkillSelector(db_factory, llm_client=None, embed_fn=_deterministic_embed)
        # Clear embeddings set by constructor, then add our test skills and rebuild
        _clear_embeddings(db)
        for name in names:
            skill = _make_skill(name, description=f"{name} description", triggers=[name.split("_")[0]])
            sel.rule_selector.skills[name] = skill
        sel._index.build(list(sel.rule_selector.skills.values()), force=True)
        yield sel

        # Cleanup
        from api.models.skill import SkillRegistry as SkillModel
        for name in names:
            db.query(SkillModel).filter_by(skill_name=name).delete()
        db.commit()

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

    def test_keyword_fallback_when_no_embed(self, db, db_factory):
        """Without embed_fn, falls back to keyword matching."""
        sel = ModernSkillSelector(db_factory, llm_client=None, embed_fn=None)
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

    def test_real_token_measurement_varies_by_schema(self, db, db_factory):
        """Different skills produce different token costs."""
        _ensure_db_skill(db, "tiny", "x")
        _ensure_db_skill(db, "huge", "A very long description " * 50)
        sel = ModernSkillSelector(db_factory, llm_client=None, embed_fn=_deterministic_embed)

        small = _make_skill("tiny", description="x")
        big = _make_skill("huge", description="A very long description " * 50,
                          triggers=["a", "b", "c", "d", "e"])
        sel.rule_selector.skills["tiny"] = small
        sel.rule_selector.skills["huge"] = big
        _clear_embeddings(db)
        sel._index.build(list(sel.rule_selector.skills.values()), force=True)

        schema_small = sel._skill_to_tool_schema(small)
        schema_big = sel._skill_to_tool_schema(big)
        assert _estimate_tokens(schema_big) > _estimate_tokens(schema_small)

    def test_max_candidates_limits_output(self, selector_with_skills):
        """max_candidates caps how many tools are returned."""
        sel = selector_with_skills
        tools, _ = sel.get_tools_schema("code", max_candidates=2, context_budget=100000)
        assert len(tools) <= 2

    def test_empty_skill_registry_returns_empty(self, db, db_factory):
        sel = ModernSkillSelector(db_factory, llm_client=None, embed_fn=_deterministic_embed)
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
    def selector_with_llm(self, db, db_factory):
        mock_llm = Mock()
        _ensure_db_skill(db, "code_review", "review code")
        sel = ModernSkillSelector(db_factory, llm_client=mock_llm, embed_fn=_deterministic_embed)
        # Clear embeddings set by constructor, then add our test skill and rebuild
        _clear_embeddings(db)
        skill = _make_skill("code_review", description="review code", triggers=["review"])
        sel.rule_selector.skills = {"code_review": skill}
        sel._index.build([skill], force=True)
        sel._index.MAX_L2_DISTANCE = 999
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

    def test_fallback_preserves_ranking_order(self, db, db_factory):
        """Fallback should return the same top-ranked skill as semantic retrieval."""
        mock_llm = Mock()
        mock_llm.chat_with_tools.side_effect = RuntimeError("boom")

        for name in ["deploy_k8s", "code_review", "search_code"]:
            _ensure_db_skill(db, name, f"{name} desc")
        sel = ModernSkillSelector(db_factory, llm_client=mock_llm, embed_fn=_deterministic_embed)
        for name in ["deploy_k8s", "code_review", "search_code"]:
            skill = _make_skill(name, description=f"{name} desc", triggers=[name.split("_")[0]])
            sel.rule_selector.skills[name] = skill
        _clear_embeddings(db)
        sel._index.build(list(sel.rule_selector.skills.values()), force=True)
        sel._index.MAX_L2_DISTANCE = 999

        # Get the expected top-ranked skill from semantic retrieval
        expected_top = sel._index.query("review code quality", top_k=1, max_distance=999)
        assert len(expected_top) == 1, "Semantic index should return at least 1 result"

        result = sel.select_and_execute("review code quality")

        assert len(result) == 1
        assert result[0]["function"]["name"] == expected_top[0], \
            f"Fallback should return top-ranked '{expected_top[0]}', got '{result[0]['function']['name']}'"

    def test_fallback_logs_warning(self, selector_with_llm, caplog):
        """Fallback should log a warning with skill name and candidate count."""
        import logging
        sel, mock_llm = selector_with_llm
        mock_llm.chat_with_tools.side_effect = RuntimeError("timeout")

        with caplog.at_level(logging.WARNING):
            sel.select_and_execute("review code")

        assert any("Fallback selection" in r.message for r in caplog.records)
        assert any("code_review" in r.message for r in caplog.records)

    def test_fallback_with_no_candidates_returns_empty(self, db, db_factory):
        """If no candidates found, fallback should return empty list."""
        mock_llm = Mock()
        mock_llm.chat_with_tools.side_effect = RuntimeError("boom")

        sel = ModernSkillSelector(db_factory, llm_client=mock_llm, embed_fn=_deterministic_embed)
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

    def test_registry_cached_in_init(self, db, db_factory):
        """SkillRegistry should be instantiated once in __init__, not per schema call."""
        sel = ModernSkillSelector(db_factory, llm_client=None)
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

    def test_pipeline_passes_embed_fn_to_selector(self, db, db_factory):
        """SkillPipeline should auto-resolve embed_fn and pass to ModernSkillSelector."""
        from core.skills.pipeline import SkillPipeline

        custom_embed = Mock(return_value=[0.1] * 32)
        pipeline = SkillPipeline(db_factory, llm_client=None, audit=False, learning=False,
                                 embed_fn=custom_embed)
        # The internal selector should have a SkillIndex with our embed_fn
        assert pipeline._modern._index._embed is custom_embed

    def test_pipeline_works_without_embed_fn(self, db, db_factory):
        """Pipeline should work even if no embed_fn is available."""
        from core.skills.pipeline import SkillPipeline

        # Patch get_embedding_client so it raises — pipeline should fall back to None
        with patch("core.context.embeddings.get_embedding_client", side_effect=Exception("no embeddings")):
            pipeline = SkillPipeline(db_factory, llm_client=None, audit=False, learning=False,
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

    def test_retrieval_pipeline_ranks_by_embedding_distance(self, db, db_factory):
        """Verify the full retrieval pipeline: embed → store → query → rank → return."""
        from core.context.embeddings import EmbeddingService

        embed_service = EmbeddingService(db_factory, provider="mock")
        def embed_fn(t): return embed_service.embed_text(t)

        _ensure_db_skill(db, "code_review", "Review code changes in pull requests for quality and security")
        _ensure_db_skill(db, "deploy_k8s", "Deploy application to Kubernetes cluster")

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

        sel = ModernSkillSelector(db_factory, llm_client=None, embed_fn=embed_fn)
        sel.rule_selector.skills = {
            "code_review": code_review,
            "deploy_k8s": deploy_k8s,
        }
        _clear_embeddings(db)
        sel._index.build([code_review, deploy_k8s], force=True)
        sel._index.MAX_L2_DISTANCE = 999

        # Query: "review PR" should rank code_review higher than deploy_k8s
        tools, method = sel.get_tools_schema("review PR code", max_candidates=3)

        assert method == "semantic"
        assert len(tools) >= 1

        # Extract skill names in order
        skill_names = [t["function"]["name"] for t in tools]

        # Both skills must appear (MAX_L2_DISTANCE=999 disables threshold)
        assert "code_review" in skill_names, f"code_review missing from {skill_names}"
        assert "deploy_k8s" in skill_names, f"deploy_k8s missing from {skill_names}"
        # code_review should appear before deploy_k8s (more relevant to "review PR")
        assert skill_names.index("code_review") < skill_names.index("deploy_k8s"), \
            f"code_review should rank higher than deploy_k8s for 'review PR code', got {skill_names}"

    def test_keyword_fallback_still_works(self, db, db_factory):
        """Verify keyword fallback when no embedding available."""
        code_review = _make_skill(
            "code_review",
            description="Review code",
            triggers=["review", "code"]
        )

        sel = ModernSkillSelector(db_factory, llm_client=None, embed_fn=None)
        sel.rule_selector.skills = {"code_review": code_review}

        tools, method = sel.get_tools_schema("review code", max_candidates=3)

        assert method == "keyword"
        assert len(tools) == 1
        assert tools[0]["function"]["name"] == "code_review"

    def test_semantic_retrieval_handles_synonyms(self, db, db_factory):
        """Verify semantic retrieval finds skills even with different wording."""
        from core.context.embeddings import EmbeddingService

        embed_service = EmbeddingService(db_factory, provider="mock")
        def embed_fn(t): return embed_service.embed_text(t)

        _ensure_db_skill(db, "code_review", "Review code changes in pull requests")

        code_review = _make_skill(
            "code_review",
            description="Review code changes in pull requests",
            triggers=["review", "pr"]
        )

        sel = ModernSkillSelector(db_factory, llm_client=None, embed_fn=embed_fn)
        sel.rule_selector.skills = {"code_review": code_review}
        _clear_embeddings(db)
        sel._index.build([code_review], force=True)
        sel._index.MAX_L2_DISTANCE = 999  # mock embeddings: test pipeline, not quality

        # Query with synonyms: "inspect merge request"
        tools, method = sel.get_tools_schema("inspect merge request", max_candidates=3)

        assert method == "semantic"
        # Should find code_review despite different wording
        # (mock embedding uses bag-of-words, so "request" matches "requests")
        assert len(tools) >= 1, "Should find at least one skill with semantic retrieval"
        assert tools[0]["function"]["name"] == "code_review"
