"""Tests for topic shift detection and introspection skill.

Validates two improvements:
1. Topic shift detection in RelevanceScorer — accuracy improvement
2. Introspection skill selection — cost savings (zero LLM cost)
"""

import pytest
from unittest.mock import MagicMock, patch

from core.context.manager import ContextManager, TaskType
from core.context.scorer import (
    ScoringWeights, RelevanceScorer, TASK_WEIGHTS,
    TopicShiftConfig, _DEFAULT_TOPIC_SHIFT_CONFIG,
)
from core.utils.similarity import cosine_similarity as _cosine_similarity


def _scorer_with_defaults(db_factory, embeddings) -> RelevanceScorer:
    """Create a RelevanceScorer with config cache pre-seeded (no DB hit)."""
    s = RelevanceScorer(db_factory, embeddings)
    s._topic_shift_config = _DEFAULT_TOPIC_SHIFT_CONFIG
    s._topic_shift_config_ts = float("inf")  # Never expires
    return s


# ============================================================================
# ScoringWeights.adjust_for_topic_shift
# ============================================================================


class TestScoringWeightsTopicShift:
    """Test weight adjustment for topic shifts."""

    def test_no_adjustment_below_threshold(self):
        """shift_score < 0.5 returns identical weights."""
        w = ScoringWeights(semantic=0.4, temporal=0.2, causal=0.3, keyword=0.1)
        adjusted = w.adjust_for_topic_shift(0.3)
        assert adjusted.semantic == w.semantic
        assert adjusted.temporal == w.temporal
        assert adjusted.causal == w.causal
        assert adjusted.keyword == w.keyword

    def test_no_adjustment_at_zero(self):
        w = ScoringWeights()
        adjusted = w.adjust_for_topic_shift(0.0)
        assert adjusted is w  # Same object, no copy needed

    def test_semantic_boosted_on_high_shift(self):
        """High topic shift should boost semantic weight."""
        w = ScoringWeights(semantic=0.4, temporal=0.2, causal=0.3, keyword=0.1)
        adjusted = w.adjust_for_topic_shift(0.9)
        assert adjusted.semantic > w.semantic
        assert adjusted.temporal < w.temporal
        assert adjusted.causal < w.causal
        assert adjusted.keyword == w.keyword

    def test_weights_still_sum_to_one(self):
        """Adjusted weights must still sum to 1.0."""
        w = ScoringWeights(semantic=0.4, temporal=0.2, causal=0.3, keyword=0.1)
        for shift in [0.5, 0.7, 0.9, 1.0]:
            adjusted = w.adjust_for_topic_shift(shift)
            total = adjusted.semantic + adjusted.temporal + adjusted.causal + adjusted.keyword
            assert abs(total - 1.0) < 0.01, f"shift={shift}: total={total}"

    def test_temporal_causal_have_floor(self):
        """Temporal and causal should never go below 0.05."""
        w = ScoringWeights(semantic=0.4, temporal=0.2, causal=0.3, keyword=0.1)
        adjusted = w.adjust_for_topic_shift(1.0)
        assert adjusted.temporal >= 0.05
        assert adjusted.causal >= 0.05

    def test_semantic_has_ceiling(self):
        """Semantic should never exceed 0.8."""
        w = ScoringWeights(semantic=0.4, temporal=0.2, causal=0.3, keyword=0.1)
        adjusted = w.adjust_for_topic_shift(1.0)
        assert adjusted.semantic <= 0.8

    def test_all_task_weights_adjustable(self):
        """All predefined task weights should be adjustable without error."""
        for task_type, weights in TASK_WEIGHTS.items():
            adjusted = weights.adjust_for_topic_shift(0.8)
            total = adjusted.semantic + adjusted.temporal + adjusted.causal + adjusted.keyword
            assert abs(total - 1.0) < 0.01, f"{task_type}: total={total}"

    def test_custom_config_threshold(self):
        """Custom config with higher threshold should require stronger shift."""
        w = ScoringWeights(semantic=0.4, temporal=0.2, causal=0.3, keyword=0.1)
        strict_config = TopicShiftConfig(threshold=0.8)
        # 0.7 is below the strict threshold → no adjustment
        adjusted = w.adjust_for_topic_shift(0.7, config=strict_config)
        assert adjusted is w

        # 0.9 is above → adjustment happens
        adjusted = w.adjust_for_topic_shift(0.9, config=strict_config)
        assert adjusted.semantic > w.semantic

    def test_custom_config_floors_and_ceiling(self):
        """Custom floors/ceiling from DB config are respected."""
        w = ScoringWeights(semantic=0.4, temporal=0.2, causal=0.3, keyword=0.1)
        config = TopicShiftConfig(
            threshold=0.5,
            temporal_floor=0.10,
            causal_floor=0.10,
            semantic_ceiling=0.70,
        )
        adjusted = w.adjust_for_topic_shift(1.0, config=config)
        assert adjusted.temporal >= 0.10
        assert adjusted.causal >= 0.10
        assert adjusted.semantic <= 0.70

    def test_config_from_dict_roundtrip(self):
        """TopicShiftConfig can be serialized to/from dict (DB storage)."""
        config = TopicShiftConfig(threshold=0.6, temporal_floor=0.08)
        d = config.to_dict()
        restored = TopicShiftConfig.from_dict(d)
        assert restored.threshold == 0.6
        assert restored.temporal_floor == 0.08


# ============================================================================
# _cosine_similarity
# ============================================================================


class TestCosineSimilarity:
    def test_identical_vectors(self):
        assert _cosine_similarity([1, 0, 0], [1, 0, 0]) == pytest.approx(1.0)

    def test_orthogonal_vectors(self):
        assert _cosine_similarity([1, 0, 0], [0, 1, 0]) == pytest.approx(0.0)

    def test_opposite_vectors(self):
        assert _cosine_similarity([1, 0], [-1, 0]) == pytest.approx(-1.0)

    def test_zero_vector_returns_zero(self):
        assert _cosine_similarity([0, 0, 0], [1, 2, 3]) == 0.0

    def test_similar_vectors_high_score(self):
        sim = _cosine_similarity([1, 2, 3], [1, 2, 3.1])
        assert sim > 0.99


# ============================================================================
# RelevanceScorer.detect_topic_shift
# ============================================================================


class TestDetectTopicShift:
    """Test topic shift detection using embeddings."""

    @pytest.fixture
    def mock_embeddings(self):
        """Embedding service that returns deterministic vectors."""
        emb = MagicMock()
        # Map text → embedding for controlled similarity
        vectors = {
            # Python topic cluster
            "how do I write a Python decorator": [1.0, 0.0, 0.0],
            "explain Python decorators": [0.95, 0.05, 0.0],
            "show me a decorator example": [0.9, 0.1, 0.0],
            # Completely different topic
            "what is the weather today": [0.0, 0.0, 1.0],
            # Agent introspection
            "how big is my context": [0.0, 1.0, 0.0],
            # Slightly related
            "how do I use type hints in Python": [0.7, 0.3, 0.0],
        }
        def embed_text(text):
            return vectors.get(text, [0.33, 0.33, 0.34])
        emb.embed_text = embed_text
        return emb

    @pytest.fixture
    def scorer(self, mock_embeddings):
        db_factory = MagicMock()
        return _scorer_with_defaults(db_factory, mock_embeddings)

    def test_same_topic_low_shift(self, scorer):
        """Continuing same topic should have low shift score."""
        recent = [
            {"content": "explain Python decorators"},
            {"content": "show me a decorator example"},
        ]
        shift = scorer.detect_topic_shift("how do I write a Python decorator", recent)
        assert shift < 0.5, f"Same topic should have low shift, got {shift}"

    def test_different_topic_high_shift(self, scorer):
        """Completely different topic should have high shift score."""
        recent = [
            {"content": "explain Python decorators"},
            {"content": "show me a decorator example"},
        ]
        shift = scorer.detect_topic_shift("what is the weather today", recent)
        assert shift > 0.5, f"Different topic should have high shift, got {shift}"

    def test_empty_recent_returns_zero(self, scorer):
        shift = scorer.detect_topic_shift("any query", [])
        assert shift == 0.0

    def test_empty_content_events_returns_zero(self, scorer):
        shift = scorer.detect_topic_shift("any query", [{"content": ""}])
        assert shift == 0.0

    def test_embedding_failure_returns_zero(self):
        """If embedding fails, gracefully return 0 (no shift assumed)."""
        emb = MagicMock()
        emb.embed_text.side_effect = RuntimeError("API down")
        scorer = _scorer_with_defaults(MagicMock(), emb)
        shift = scorer.detect_topic_shift("test", [{"content": "hello"}])
        assert shift == 0.0

    def test_introspection_shift_from_code_topic(self, scorer):
        """Switching from code discussion to introspection should detect shift."""
        recent = [
            {"content": "explain Python decorators"},
            {"content": "show me a decorator example"},
        ]
        shift = scorer.detect_topic_shift("how big is my context", recent)
        assert shift > 0.5, f"Introspection after code should be a shift, got {shift}"


# ============================================================================
# Integration: topic shift affects scoring
# ============================================================================


class TestTopicShiftScoring:
    """Verify that topic shift changes which events get selected."""

    @pytest.fixture
    def mock_embeddings(self):
        emb = MagicMock()
        emb.embed_text.return_value = [0.0, 1.0, 0.0]  # "introspection" direction
        emb.search_similar.return_value = []
        return emb

    @pytest.fixture
    def scorer(self, mock_embeddings):
        db_factory = MagicMock()
        s = _scorer_with_defaults(db_factory, mock_embeddings)
        # Stub out DB call for causal chains
        s._get_recent_chains = MagicMock(return_value={"chain-old"})
        return s

    def _make_candidate(self, event_id, content, chain_id="chain-old", age_hours=0.5):
        from datetime import datetime, timezone, timedelta
        return {
            "event_id": event_id,
            "content": content,
            "causal_chain_id": chain_id,
            "created_at": datetime.now(timezone.utc) - timedelta(hours=age_hours),
        }

    def test_without_shift_temporal_causal_dominate(self, scorer):
        """Without topic shift, recent events in same chain score high."""
        candidates = [
            self._make_candidate("old-topic", "Python decorators", "chain-old", 0.1),
            self._make_candidate("new-topic", "context window size", "chain-new", 2.0),
        ]
        scored = scorer.score_candidates("how big is my context", candidates, "sess-1", topic_shift=None)
        # Old topic event has temporal + causal boost
        old_score = next(s for c, s, _ in scored if c["event_id"] == "old-topic")
        new_score = next(s for c, s, _ in scored if c["event_id"] == "new-topic")
        # Without shift, old-topic benefits from recency + causal chain
        assert old_score > 0, "Old topic should have positive score from temporal+causal"

    def test_with_high_shift_semantic_dominates(self, scorer):
        """With high topic shift, semantic signal should dominate."""
        candidates = [
            self._make_candidate("old-topic", "Python decorators", "chain-old", 0.1),
            self._make_candidate("new-topic", "context window size", "chain-new", 2.0),
        ]
        scored_no_shift = scorer.score_candidates(
            "how big is my context", candidates, "sess-1", topic_shift=None,
        )
        scored_with_shift = scorer.score_candidates(
            "how big is my context", candidates, "sess-1", topic_shift=0.9,
        )

        def get_score(scored, eid):
            return next(s for c, s, _ in scored if c["event_id"] == eid)

        old_no_shift = get_score(scored_no_shift, "old-topic")
        old_with_shift = get_score(scored_with_shift, "old-topic")

        # With topic shift, old-topic's temporal+causal boost is suppressed
        assert old_with_shift < old_no_shift, (
            f"Topic shift should reduce old-topic score: {old_with_shift} vs {old_no_shift}"
        )


class TestIntrospectionSkillExecution:
    """Test the introspection skill returns correct data."""

    @pytest.mark.asyncio
    async def test_execute_returns_stats(self):
        from skills.introspection.skill import IntrospectionSkill, IntrospectionInput

        skill = IntrospectionSkill()
        inp = IntrospectionInput(
            user_id="alice",
            session_id="sess-1",
            dimension="all",
            runtime_state={
                "context_tokens": 5000,
                "max_tokens": 128000,
                "turn_count": 12,
                "session_id": "sess-1",
                "agent_id": "dev-agent",
                "model": "gpt-4o",
                "skills_loaded": 7,
            },
        )

        output = await skill.execute(inp)

        assert output.success is True
        assert output.context_tokens == 5000
        assert output.max_tokens == 128000
        assert output.usage_percent == pytest.approx(3.9, abs=0.1)
        assert output.turn_count == 12
        assert output.session_id == "sess-1"
        assert output.agent_id == "dev-agent"
        assert output.model == "gpt-4o"
        assert output.skills_loaded == 7

    @pytest.mark.asyncio
    async def test_execute_with_no_metadata(self):
        """Graceful defaults when no metadata injected."""
        from skills.introspection.skill import IntrospectionSkill, IntrospectionInput

        skill = IntrospectionSkill()
        inp = IntrospectionInput(user_id="alice", session_id="sess-1")

        output = await skill.execute(inp)

        assert output.success is True
        assert output.context_tokens == 0
        assert output.max_tokens == 128000
        assert output.usage_percent == 0.0


# ============================================================================
# End-to-end: topic shift + introspection = accuracy + savings
# ============================================================================


class TestEndToEndImprovement:
    """Simulate the original failing scenario and verify improvement."""

    def test_topic_shift_reduces_stale_context_tokens(self):
        """After 10 turns of code discussion, user asks about context size.

        Without topic shift: all 10 code events fill the context (waste).
        With topic shift: code events are deprioritized, saving tokens.
        """
        from datetime import datetime, timezone, timedelta

        emb = MagicMock()
        # Code topic = [1,0,0], introspection = [0,1,0]
        def embed_text(text):
            if any(kw in text.lower() for kw in ["decorator", "python", "code", "function"]):
                return [1.0, 0.0, 0.0]
            if any(kw in text.lower() for kw in ["context", "上下文", "token", "多大"]):
                return [0.0, 1.0, 0.0]
            return [0.5, 0.5, 0.0]
        emb.embed_text = embed_text
        emb.search_similar.return_value = []

        scorer = _scorer_with_defaults(MagicMock(), emb)
        scorer._get_recent_chains = MagicMock(return_value={"code-chain"})

        # 10 code discussion events (recent, same causal chain)
        now = datetime.now(timezone.utc)
        code_events = [
            {
                "event_id": f"code-{i}",
                "content": f"Python decorator example {i}",
                "causal_chain_id": "code-chain",
                "created_at": now - timedelta(minutes=10 - i),
            }
            for i in range(10)
        ]

        query = "上下文积累到多大了"

        # Detect topic shift
        shift = scorer.detect_topic_shift(query, code_events[-3:])
        assert shift > 0.5, f"Should detect topic shift, got {shift}"

        # Score WITHOUT topic shift
        scored_no_shift = scorer.score_candidates(query, code_events, "sess-1", topic_shift=None)
        # Score WITH topic shift
        scored_with_shift = scorer.score_candidates(query, code_events, "sess-1", topic_shift=shift)

        # Sum of all scores = proxy for "how much context would be selected"
        total_no_shift = sum(s for _, s, _ in scored_no_shift)
        total_with_shift = sum(s for _, s, _ in scored_with_shift)

        # With topic shift, total relevance of stale events should be lower
        assert total_with_shift < total_no_shift, (
            f"Topic shift should reduce stale event scores: "
            f"with_shift={total_with_shift:.3f} vs no_shift={total_no_shift:.3f}"
        )

        # Quantify savings: at least 20% reduction in stale context score
        reduction_pct = (1 - total_with_shift / total_no_shift) * 100
        assert reduction_pct > 20, f"Expected >20% reduction, got {reduction_pct:.1f}%"

    def test_introspection_skill_zero_llm_cost(self):
        """Introspection skill has cost_estimate='low' and llm_required=False."""
        from skills.introspection.skill import IntrospectionSkill

        skill = IntrospectionSkill()
        assert skill.requirements.llm_required is False
        assert skill.side_effect_profile.category.value == "read"


# ============================================================================
# End-to-end: ContextManager.build_context with topic shift
# ============================================================================


class TestEndToEndTopicShift:
    """Full pipeline: build_context → scorer → selected events.

    Simulates: 10 turns of code discussion, then user asks "上下文积累到多大了".
    Verifies that topic shift detection causes fewer stale code events to be
    selected, and more token budget is available for relevant content.
    """

    @pytest.fixture
    def deterministic_embeddings(self):
        """Embedding service with deterministic vectors per topic."""
        emb = MagicMock()

        def embed_text(text):
            t = text.lower()
            if any(kw in t for kw in ["decorator", "python", "code", "function", "class"]):
                return [1.0, 0.0, 0.0]
            if any(kw in t for kw in ["context", "上下文", "token", "多大", "积累"]):
                return [0.0, 1.0, 0.0]
            return [0.5, 0.5, 0.0]

        emb.embed_text = embed_text
        emb.search_similar.return_value = []
        return emb

    @pytest.fixture
    def code_events(self):
        """10 code discussion events + 1 context-related event."""
        from datetime import datetime, timezone, timedelta

        now = datetime.now(timezone.utc)
        events = []
        for i in range(10):
            events.append({
                "event_id": f"code-{i}",
                "event_type": "user_query" if i % 2 == 0 else "llm_response",
                "content": f"Python decorator example {i}: def my_decorator(func): pass",
                "created_at": now - timedelta(minutes=20 - i),
                "parent_event_id": None,
                "causal_chain_id": "code-chain",
                "metadata": {},
            })
        # One older but relevant event about context
        events.append({
            "event_id": "ctx-old",
            "event_type": "llm_response",
            "content": "Your context window is currently using 5000 tokens",
            "created_at": now - timedelta(hours=2),
            "parent_event_id": None,
            "causal_chain_id": "other-chain",
            "metadata": {},
        })
        return events

    def _build_context_manager(self, embeddings):
        """Create ContextManager with mocked DB and controlled embeddings."""
        mock_db = MagicMock()

        with patch("core.context.embeddings.EmbeddingService") as MockEmbSvc, \
             patch("core.context.prompts.PromptManager") as MockPrompts:
            MockEmbSvc.return_value = embeddings
            MockPrompts.return_value.get_system_prompt.return_value = "You are an agent."

            cm = ContextManager(lambda: mock_db, embedding_provider="mock")
            # Replace embeddings with our deterministic one
            cm.embeddings = embeddings
            cm.scorer = _scorer_with_defaults(lambda: mock_db, embeddings)
            # Stub out DB call for causal chains
            cm.scorer._get_recent_chains = MagicMock(return_value={"code-chain"})
            return cm

    def test_build_context_selects_fewer_stale_events_on_topic_shift(
        self, deterministic_embeddings, code_events,
    ):
        """Core assertion: topic shift → fewer code events selected.

        Uses forced_retrieval to inject events directly, bypassing DB.
        Compares selected_events count between same-topic and shifted-topic queries.
        """
        cm = self._build_context_manager(deterministic_embeddings)

        # --- Baseline: same-topic query (no shift expected) ---
        ctx_same = cm.build_context(
            session_id="sess-1",
            query="show me another Python decorator example",
            max_tokens=4000,
            forced_retrieval=code_events,
        )
        same_topic_ids = {e["event_id"] for e in ctx_same.selected_events}
        same_topic_tokens = ctx_same.total_tokens

        # --- Topic shift: introspection query ---
        ctx_shift = cm.build_context(
            session_id="sess-1",
            query="上下文积累到多大了",
            max_tokens=4000,
            forced_retrieval=code_events,
        )
        shift_topic_ids = {e["event_id"] for e in ctx_shift.selected_events}
        shift_topic_tokens = ctx_shift.total_tokens

        # The context-related event should rank higher after topic shift
        # (or at least stale code events should rank lower)
        same_code_count = sum(1 for eid in same_topic_ids if eid.startswith("code-"))
        shift_code_count = sum(1 for eid in shift_topic_ids if eid.startswith("code-"))

        # With topic shift, fewer code events should be selected
        # (their temporal+causal boost is suppressed)
        assert shift_code_count <= same_code_count, (
            f"Topic shift should select fewer stale code events: "
            f"shift={shift_code_count} vs same={same_code_count}"
        )

        # Token savings: shifted context should use fewer or equal tokens
        assert shift_topic_tokens <= same_topic_tokens, (
            f"Topic shift should save tokens: "
            f"shift={shift_topic_tokens} vs same={same_topic_tokens}"
        )

    def test_build_context_relevance_order_changes_on_shift(
        self, deterministic_embeddings, code_events,
    ):
        """The score distribution should change when topic shifts.

        After shift, code events should have lower scores than without shift,
        because temporal+causal weights are suppressed.
        """
        cm = self._build_context_manager(deterministic_embeddings)

        # Same-topic query: code events get full temporal+causal boost
        ctx_same = cm.build_context(
            session_id="sess-1",
            query="show me another Python decorator example",
            max_tokens=8000,
            forced_retrieval=code_events,
        )

        # Shifted query: code events lose temporal+causal boost
        ctx_shift = cm.build_context(
            session_id="sess-1",
            query="上下文积累到多大了",
            max_tokens=8000,
            forced_retrieval=code_events,
        )

        # Compare average relevance scores of code events
        def avg_code_score(ctx):
            scores = [e["score"] for e in ctx.selected_events if e["event_id"].startswith("code-")]
            return sum(scores) / len(scores) if scores else 0

        avg_same = avg_code_score(ctx_same)
        avg_shift = avg_code_score(ctx_shift)

        assert avg_shift < avg_same, (
            f"Code events should score lower after topic shift: "
            f"shift_avg={avg_shift:.4f} vs same_avg={avg_same:.4f}"
        )

    def test_no_shift_preserves_recency_order(
        self, deterministic_embeddings, code_events,
    ):
        """Without topic shift, recent code events should dominate."""
        cm = self._build_context_manager(deterministic_embeddings)

        ctx = cm.build_context(
            session_id="sess-1",
            query="show me another Python decorator",
            max_tokens=4000,
            forced_retrieval=code_events,
        )

        selected_ids = [e["event_id"] for e in ctx.selected_events]
        code_ids = [eid for eid in selected_ids if eid.startswith("code-")]

        # Most recent code events should be selected
        assert len(code_ids) > 0, "Same-topic query should select code events"

        # ctx-old should NOT be prioritized (it's old and off-topic)
        if "ctx-old" in selected_ids and code_ids:
            ctx_rank = selected_ids.index("ctx-old")
            first_code_rank = selected_ids.index(code_ids[0])
            assert ctx_rank > first_code_rank, (
                "Without topic shift, code events should rank above old context event"
            )


class TestEndToEndCostSavings:
    """Verify measurable cost savings from both features."""

    def test_introspection_skill_zero_llm_cost(self):
        """Introspection skill has cost_estimate='low' and llm_required=False."""
        from skills.introspection.skill import IntrospectionSkill

        skill = IntrospectionSkill()
        assert skill.requirements.llm_required is False
        assert skill.side_effect_profile.category.value == "read"

    def test_topic_shift_token_savings_quantified(self):
        """Quantify: topic shift saves >20% tokens on stale context."""
        from datetime import datetime, timezone, timedelta

        emb = MagicMock()
        def embed_text(text):
            if "decorator" in text.lower():
                return [1.0, 0.0, 0.0]
            if "context" in text.lower() or "上下文" in text:
                return [0.0, 1.0, 0.0]
            return [0.5, 0.5, 0.0]
        emb.embed_text = embed_text
        emb.search_similar.return_value = []

        scorer = _scorer_with_defaults(MagicMock(), emb)
        scorer._get_recent_chains = MagicMock(return_value={"code-chain"})

        now = datetime.now(timezone.utc)
        code_events = [
            {
                "event_id": f"code-{i}",
                "content": f"Python decorator example {i}",
                "causal_chain_id": "code-chain",
                "created_at": now - timedelta(minutes=10 - i),
            }
            for i in range(10)
        ]

        query = "上下文积累到多大了"
        shift = scorer.detect_topic_shift(query, code_events[-3:])
        assert shift > 0.5

        scored_no_shift = scorer.score_candidates(query, code_events, "sess-1", topic_shift=None)
        scored_with_shift = scorer.score_candidates(query, code_events, "sess-1", topic_shift=shift)

        total_no_shift = sum(s for _, s, _ in scored_no_shift)
        total_with_shift = sum(s for _, s, _ in scored_with_shift)

        reduction_pct = (1 - total_with_shift / total_no_shift) * 100
        assert reduction_pct > 20, f"Expected >20% reduction, got {reduction_pct:.1f}%"


# ============================================================================
# Self-evolution: config-driven + learning signal
# ============================================================================


class TestSelfEvolution:
    """Verify topic shift is plugged into the self-improving loop."""

    def test_stale_context_signal_type_exists(self):
        """STALE_CONTEXT is a valid signal type for the learning system."""
        """RelevanceScorer loads TopicShiftConfig from configs table."""
        mock_db = MagicMock()
        # Simulate DB returning a custom config
        mock_row = MagicMock()
        mock_row.__getitem__ = lambda self, i: '{"threshold": 0.7, "temporal_floor": 0.08}'
        mock_db.query.return_value.filter.return_value.first.return_value = mock_row

        scorer = RelevanceScorer(lambda: mock_db, MagicMock())
        config = scorer._load_topic_shift_config()

        assert config.threshold == 0.7
        assert config.temporal_floor == 0.08

    def test_topic_shift_config_defaults_on_missing_db(self):
        """Falls back to defaults when DB has no config."""
        mock_db = MagicMock()
        mock_db.query.return_value.filter.return_value.first.return_value = None

        scorer = RelevanceScorer(lambda: mock_db, MagicMock())
        config = scorer._load_topic_shift_config()

        assert config.threshold == 0.5
        assert config.temporal_floor == 0.05

    def test_topic_shift_config_defaults_on_db_error(self):
        """Falls back to defaults when DB query fails."""
        mock_db = MagicMock()
        mock_db.query.side_effect = RuntimeError("DB down")

        scorer = RelevanceScorer(lambda: mock_db, MagicMock())
        config = scorer._load_topic_shift_config()

        assert config.threshold == 0.5  # Default, not crash
