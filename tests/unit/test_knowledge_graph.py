"""Tests for Knowledge Graph (entity-relationship layer)."""

from unittest.mock import MagicMock, patch

from skills.knowledge.api import add_relation, expand_with_graph, get_neighbors


def _make_row(**kwargs):
    m = MagicMock()
    for k, v in kwargs.items():
        setattr(m, k, v)
    return m


# ── add_relation ─────────────────────────────────────────────────

class TestAddRelation:
    def test_success(self):
        db = MagicMock()
        rid = add_relation(db, "e1", "related_to", "e2", weight=0.8)
        assert rid is not None
        db.commit.assert_called_once()

    def test_failure_returns_none(self):
        db = MagicMock()
        db.execute.side_effect = RuntimeError("db error")
        rid = add_relation(db, "e1", "related_to", "e2")
        assert rid is None
        db.rollback.assert_called_once()

    def test_idempotent_via_on_duplicate_key(self):
        """Calling twice with same (s, p, o) should not raise."""
        db = MagicMock()
        # First call: SELECT returns None → INSERT
        db.execute.return_value.fetchone.return_value = None
        add_relation(db, "e1", "related_to", "e2")
        # Second call: SELECT returns existing → UPDATE
        db.execute.return_value.fetchone.return_value = _make_row(relation_id="r1")
        add_relation(db, "e1", "related_to", "e2", weight=0.5)
        assert db.commit.call_count == 2


# ── get_neighbors ────────────────────────────────────────────────

class TestGetNeighbors:
    def test_both_directions(self):
        db = MagicMock()
        rows = [
            _make_row(neighbor_id="e2", predicate="related_to", weight=0.9, dir="outgoing"),
            _make_row(neighbor_id="e3", predicate="depends_on", weight=0.5, dir="incoming"),
        ]
        db.execute.return_value.fetchall.return_value = rows
        result = get_neighbors(db, "e1")
        assert len(result) == 2
        assert result[0]["neighbor_id"] == "e2"
        assert result[1]["direction"] == "incoming"

    def test_empty_result(self):
        db = MagicMock()
        db.execute.return_value.fetchall.return_value = []
        assert get_neighbors(db, "e1") == []

    def test_db_error_returns_empty(self):
        db = MagicMock()
        db.execute.side_effect = RuntimeError("fail")
        assert get_neighbors(db, "e1") == []

    def test_predicate_filter(self):
        """Predicate filter uses dynamic placeholders, not tuple binding."""
        db = MagicMock()
        db.execute.return_value.fetchall.return_value = [
            _make_row(neighbor_id="e2", predicate="depends_on", weight=1.0, dir="outgoing"),
        ]
        result = get_neighbors(db, "e1", predicates=["depends_on", "related_to"])
        assert len(result) == 1
        # Verify the SQL was called (no tuple binding error)
        db.execute.assert_called_once()


# ── expand_with_graph ────────────────────────────────────────────

class TestExpandWithGraph:
    def test_empty_seeds(self):
        db = MagicMock()
        assert expand_with_graph(db, []) == []

    def test_returns_neighbor_ids(self):
        db = MagicMock()
        rows = [
            _make_row(neighbor_id="e3", total_weight=1.5),
            _make_row(neighbor_id="e4", total_weight=0.8),
        ]
        db.execute.return_value.fetchall.return_value = rows
        result = expand_with_graph(db, ["e1", "e2"])
        assert result == ["e3", "e4"]

    def test_db_error_returns_empty(self):
        db = MagicMock()
        db.execute.side_effect = RuntimeError("fail")
        assert expand_with_graph(db, ["e1"]) == []


# ── HybridRetrieval integration ──────────────────────────────────

class TestHybridRetrievalGraphExpansion:
    def test_graph_expansion_wired_in_retrieve_knowledge(self):
        """Verify expand_with_graph is called from retrieve_knowledge."""
        import inspect
        from core.context.hybrid_retrieval import HybridRetriever
        source = inspect.getsource(HybridRetriever.retrieve_knowledge)
        assert "expand_with_graph" in source

    def test_graph_expansion_failure_non_fatal(self):
        """Graph expansion failure should not break retrieval."""
        from core.context.hybrid_retrieval import HybridRetriever

        hr = HybridRetriever.__new__(HybridRetriever)
        _mock_db = MagicMock()
        hr._db_factory = lambda: _mock_db

        # Simulate: main query returns 1 entry, graph expansion raises
        main_row = _make_row(
            entry_id="e1", category="fact", key_name="k", value="v",
            confidence=0.9, trust_tier="T1", created_at=None, last_validated_at=None,
            relevance_score=0.8,
        )
        call_count = [0]
        def side_effect(*args, **kwargs):
            call_count[0] += 1
            if call_count[0] == 1:
                # Main query
                result = MagicMock()
                result.__iter__ = MagicMock(return_value=iter([main_row]))
                return result
            # Graph expansion or access tracking
            raise RuntimeError("graph boom")

        _mock_db.execute = MagicMock(side_effect=side_effect)

        with patch("skills.knowledge.api.update_access_tracking"):
            entries = hr.retrieve_knowledge(
                query_text="test", query_embedding=[0.1] * 1536,
                user_id="u1", limit=5,
            )
        assert len(entries) >= 1
        assert entries[0]["entry_id"] == "e1"
