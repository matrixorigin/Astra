from tests.conftest import TEST_EMBEDDING_DIM
"""Tests for enhanced hybrid retrieval.

Strategy: ORM chain mocking is brittle, so we test the Python-side logic
(reranking, score merging, error handling, validation) by controlling what
the mock query chain returns, and verify the output dicts.
"""

from unittest.mock import MagicMock

import pytest

from core.context.hybrid_retrieval import HybridRetriever


def _make_chain(rows=None):
    """Return a chainable mock that ends with .all() -> rows."""
    chain = MagicMock()
    chain.join.return_value = chain
    chain.filter.return_value = chain
    chain.add_columns.return_value = chain
    chain.order_by.return_value = chain
    chain.limit.return_value = chain
    chain.all.return_value = rows or []
    chain.first.return_value = None
    return chain


def _make_event_row(event_id, sem=0.3, temp=0.05, chain_id=None):
    """Simulate an ORM row from the vector query."""
    r = MagicMock()
    r.event_id = event_id
    r.session_id = "sess_1"
    r.event_type = "user_query"
    r.content = f"content_{event_id}"
    r.created_at = None
    r.causal_chain_id = chain_id
    r.parent_event_id = None
    r.event_metadata = {}
    r.sem = sem
    r.temp = temp
    return r


def _make_ft_row(event_id, ft_score=999.0):
    """Simulate an ORM row from the fulltext query.

    ft_score=999.0 normalizes to ~1.0 via score/(score+1), so
    keyword_score ≈ weights["keyword"] (default 0.25).
    """
    r = MagicMock()
    r.event_id = event_id
    r.session_id = "sess_1"
    r.event_type = "user_query"
    r.content = f"content_{event_id}"
    r.created_at = None
    r.causal_chain_id = None
    r.parent_event_id = None
    r.event_metadata = {}
    r.ft_score = ft_score
    return r


class TestRetrieveEvents:
    @pytest.fixture
    def mock_db(self):
        return MagicMock()

    @pytest.fixture
    def retriever(self, mock_db):
        return HybridRetriever(lambda: mock_db)

    def test_vector_and_fulltext_scores_merge(self, retriever, mock_db):
        """Event found by both paths gets combined score."""
        vec_chain = _make_chain([_make_event_row("e1", sem=0.30, temp=0.05)])
        ft_chain = _make_chain([_make_ft_row("e1")])
        mock_db.query.side_effect = [vec_chain, ft_chain]

        events = retriever.retrieve_events(
            query_text="test", query_embedding=[0.1] * TEST_EMBEDDING_DIM, session_id="sess_1",
        )
        assert len(events) == 1
        # vector_score = sem + temp + 0 causal = 0.35, keyword_score = 0.25
        assert events[0]["relevance_score"] == pytest.approx(0.60, abs=0.01)
        assert events[0]["event_id"] == "e1"

    def test_causal_bonus_applied(self, retriever, mock_db):
        """Event in same causal chain gets causal weight bonus."""
        vec_chain = _make_chain([_make_event_row("e1", sem=0.30, temp=0.05, chain_id="c1")])
        ft_chain = _make_chain([])
        mock_db.query.side_effect = [vec_chain, ft_chain]

        events = retriever.retrieve_events(
            query_text="test", query_embedding=[0.1] * TEST_EMBEDDING_DIM,
            session_id="sess_1", current_chain_id="c1",
        )
        assert len(events) == 1
        # vector_score = 0.30 + 0.05 + 0.20 (causal) = 0.55
        assert events[0]["relevance_score"] == pytest.approx(0.55, abs=0.01)

    def test_fulltext_only_event(self, retriever, mock_db):
        """Event found only by fulltext gets keyword weight only."""
        vec_chain = _make_chain([])
        ft_chain = _make_chain([_make_ft_row("e2")])
        mock_db.query.side_effect = [vec_chain, ft_chain]

        events = retriever.retrieve_events(
            query_text="test", query_embedding=[0.1] * TEST_EMBEDDING_DIM, session_id="sess_1",
        )
        assert len(events) == 1
        assert events[0]["relevance_score"] == pytest.approx(0.25, abs=0.01)

    def test_ranking_order(self, retriever, mock_db):
        """Events are ranked by combined score descending."""
        vec_chain = _make_chain([
            _make_event_row("e1", sem=0.30, temp=0.05),
            _make_event_row("e2", sem=0.10, temp=0.02),
        ])
        ft_chain = _make_chain([_make_ft_row("e2")])  # e2 also matched by fulltext
        mock_db.query.side_effect = [vec_chain, ft_chain]

        events = retriever.retrieve_events(
            query_text="test", query_embedding=[0.1] * TEST_EMBEDDING_DIM, session_id="sess_1",
        )
        assert len(events) == 2
        # e1: 0.35 vector only; e2: 0.12 + 0.25 keyword = 0.37
        assert events[0]["event_id"] == "e2"
        assert events[1]["event_id"] == "e1"

    def test_vector_failure_falls_back_to_fulltext(self, retriever, mock_db):
        """Vector search exception doesn't prevent fulltext results."""
        call_count = 0

        def side_effect(*args, **kwargs):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                raise RuntimeError("vector down")
            return _make_chain([_make_ft_row("e3")])

        mock_db.query.side_effect = side_effect
        events = retriever.retrieve_events(
            query_text="test", query_embedding=[0.1] * TEST_EMBEDDING_DIM, session_id="sess_1",
        )
        assert len(events) == 1
        assert events[0]["event_id"] == "e3"

    def test_both_fail_returns_empty(self, retriever, mock_db):
        """Both paths failing returns empty list, no exception."""
        mock_db.query.side_effect = RuntimeError("DB down")
        events = retriever.retrieve_events(
            query_text="test", query_embedding=[0.1] * TEST_EMBEDDING_DIM, session_id="sess_1",
        )
        assert events == []

    def test_internal_scores_removed_from_output(self, retriever, mock_db):
        """Output dicts should not contain vector_score or keyword_score."""
        vec_chain = _make_chain([_make_event_row("e1")])
        ft_chain = _make_chain([])
        mock_db.query.side_effect = [vec_chain, ft_chain]

        events = retriever.retrieve_events(
            query_text="test", query_embedding=[0.1] * TEST_EMBEDDING_DIM, session_id="sess_1",
        )
        assert "vector_score" not in events[0]
        assert "keyword_score" not in events[0]
        assert "relevance_score" in events[0]


class TestRetrieveKnowledge:
    @pytest.fixture
    def mock_db(self):
        return MagicMock()

    @pytest.fixture
    def retriever(self, mock_db):
        return HybridRetriever(lambda: mock_db)

    def test_invalid_weights_returns_empty(self, retriever, mock_db):
        """Missing required weight keys returns empty immediately."""
        entries = retriever.retrieve_knowledge(
            query_text="test", query_embedding=[0.1] * TEST_EMBEDDING_DIM,
            user_id="u1", weights={"semantic": 0.5},
        )
        assert entries == []
        assert not mock_db.query.called  # should short-circuit before DB

    def test_vector_failure_returns_empty(self, retriever, mock_db):
        """Vector query failure returns empty list."""
        mock_db.query.side_effect = RuntimeError("DB down")
        entries = retriever.retrieve_knowledge(
            query_text="test", query_embedding=[0.1] * TEST_EMBEDDING_DIM, user_id="u1",
        )
        assert entries == []
