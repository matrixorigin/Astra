"""Tests for multi-agent conflict resolution."""

from unittest.mock import Mock

import pytest

from core.agents.conflict_resolver import (
    Conflict,
    ConflictResolver,
    Proposal,
    ResolutionStrategy,
    VoteType,
)


def _mock_db():
    return Mock()


class TestProposal:
    def test_proposal_creation(self):
        p = Proposal(
            agent_id="code_agent",
            action="refactor_function_X",
            reasoning="Improve readability",
            priority=1,
        )
        assert p.agent_id == "code_agent"
        assert p.action == "refactor_function_X"
        assert p.priority == 1

    def test_proposal_with_evidence(self):
        p = Proposal(
            agent_id="perf_agent",
            action="keep_function_X",
            reasoning="Hot path optimization",
            evidence={"benchmark": "10% faster"},
            priority=2,
        )
        assert p.evidence["benchmark"] == "10% faster"


class TestConflict:
    def test_conflict_creation(self):
        proposals = [
            Proposal("agent_a", "modify", "reason_a"),
            Proposal("agent_b", "keep", "reason_b"),
        ]
        conflict = Conflict(
            conflict_id="c1",
            target="function_X",
            proposals=proposals,
            detected_at="2026-01-01T00:00:00Z",
        )
        assert conflict.conflict_id == "c1"
        assert len(conflict.proposals) == 2


class TestConflictResolver:
    def test_detect_conflict_no_conflict(self):
        db = _mock_db()
        resolver = ConflictResolver(lambda: db)

        proposals = [
            Proposal("agent_a", "refactor", "improve"),
            Proposal("agent_b", "refactor", "improve"),
        ]
        conflict = resolver.detect_conflict(proposals, "function_X", "sess-1")
        assert conflict is None

    def test_detect_conflict_found(self):
        db = _mock_db()
        event_logger = Mock()
        resolver = ConflictResolver(lambda: db, event_logger=event_logger)

        proposals = [
            Proposal("code_agent", "refactor", "improve readability"),
            Proposal("perf_agent", "keep", "hot path"),
            Proposal("security_agent", "rewrite", "vulnerability"),
        ]
        conflict = resolver.detect_conflict(proposals, "function_X", "sess-1")

        assert conflict is not None
        assert conflict.target == "function_X"
        assert len(conflict.proposals) == 3
        event_logger.create_event.assert_called_once()
        call_kwargs = event_logger.create_event.call_args[1]
        assert call_kwargs["event_type"] == "conflict_detected"

    def test_detect_conflict_single_proposal(self):
        db = _mock_db()
        resolver = ConflictResolver(lambda: db)

        proposals = [Proposal("agent_a", "refactor", "improve")]
        conflict = resolver.detect_conflict(proposals, "function_X", "sess-1")
        assert conflict is None

    def test_resolve_by_authority(self):
        db = _mock_db()
        resolver = ConflictResolver(lambda: db)

        proposals = [
            Proposal("code_agent", "refactor", "improve", priority=1),
            Proposal("perf_agent", "keep", "optimize", priority=2),
            Proposal("security_agent", "rewrite", "fix vuln", priority=3),
        ]
        conflict = Conflict(
            conflict_id="c1",
            target="function_X",
            proposals=proposals,
            detected_at="2026-01-01T00:00:00Z",
        )

        # Security > code > perf
        priority_order = ["security_agent", "code_agent", "perf_agent"]
        winner = resolver.resolve_by_authority(conflict, priority_order)

        assert winner.agent_id == "security_agent"

    def test_resolve_by_authority_fallback(self):
        db = _mock_db()
        resolver = ConflictResolver(lambda: db)

        proposals = [
            Proposal("agent_a", "action_a", "reason_a"),
            Proposal("agent_b", "action_b", "reason_b"),
        ]
        conflict = Conflict(
            conflict_id="c1",
            target="target",
            proposals=proposals,
            detected_at="2026-01-01T00:00:00Z",
        )

        # Neither in priority order
        priority_order = ["agent_c", "agent_d"]
        winner = resolver.resolve_by_authority(conflict, priority_order)

        # Should return first proposal
        assert winner.agent_id == "agent_a"

    def test_resolve_by_evidence_default_scoring(self):
        db = _mock_db()
        resolver = ConflictResolver(lambda: db)

        proposals = [
            Proposal("agent_a", "action_a", "reason_a", evidence=None),
            Proposal(
                "agent_b",
                "action_b",
                "reason_b",
                evidence={"test": "passed", "perf": "10% faster"},
            ),
        ]
        conflict = Conflict(
            conflict_id="c1",
            target="target",
            proposals=proposals,
            detected_at="2026-01-01T00:00:00Z",
        )

        winner = resolver.resolve_by_evidence(conflict)
        assert winner.agent_id == "agent_b"  # Has evidence

    def test_resolve_by_evidence_custom_scoring(self):
        db = _mock_db()
        resolver = ConflictResolver(lambda: db)

        proposals = [
            Proposal("agent_a", "action_a", "reason_a"),
            Proposal("agent_b", "action_b", "reason_b"),
        ]
        conflict = Conflict(
            conflict_id="c1",
            target="target",
            proposals=proposals,
            detected_at="2026-01-01T00:00:00Z",
        )

        def score_fn(p):
            return 10 if p.agent_id == "agent_a" else 5

        winner = resolver.resolve_by_evidence(conflict, scoring_fn=score_fn)
        assert winner.agent_id == "agent_a"

    def test_request_consensus(self):
        db = _mock_db()
        event_logger = Mock()
        resolver = ConflictResolver(lambda: db, event_logger=event_logger)

        proposal = Proposal("lead", "deploy_to_prod", "ready")
        team = ["code_agent", "security_agent", "ops_agent"]

        votes = resolver.request_consensus(proposal, team, "sess-1")

        assert isinstance(votes, dict)
        assert event_logger.create_event.call_count == 3

    def test_record_vote(self):
        db = _mock_db()
        event_logger = Mock()
        resolver = ConflictResolver(lambda: db, event_logger=event_logger)

        resolver.record_vote(
            voter_id="code_agent",
            proposal_agent="lead",
            vote=VoteType.APPROVE,
            reason=None,
            session_id="sess-1",
        )

        event_logger.create_event.assert_called_once()
        call_kwargs = event_logger.create_event.call_args[1]
        assert call_kwargs["event_type"] == "consensus_vote"
        assert call_kwargs["metadata"]["vote"] == "approve"

    def test_record_vote_with_objection(self):
        db = _mock_db()
        event_logger = Mock()
        resolver = ConflictResolver(lambda: db, event_logger=event_logger)

        resolver.record_vote(
            voter_id="security_agent",
            proposal_agent="lead",
            vote=VoteType.OBJECT,
            reason="Security risk in deployment",
            session_id="sess-1",
        )

        call_kwargs = event_logger.create_event.call_args[1]
        assert call_kwargs["metadata"]["vote"] == "object"
        assert call_kwargs["metadata"]["reason"] == "Security risk in deployment"


class TestResolutionStrategy:
    def test_strategy_enum(self):
        assert ResolutionStrategy.AUTHORITY.value == "authority"
        assert ResolutionStrategy.EVIDENCE_BASED.value == "evidence_based"
        assert ResolutionStrategy.SYNTHESIS.value == "synthesis"
        assert ResolutionStrategy.TOURNAMENT.value == "tournament"
        assert ResolutionStrategy.ESCALATION.value == "escalation"


class TestVoteType:
    def test_vote_enum(self):
        assert VoteType.APPROVE.value == "approve"
        assert VoteType.OBJECT.value == "object"
