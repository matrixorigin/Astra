"""Multi-agent conflict resolution and consensus.

Design ref: agents-and-orchestration.md §6 "Multi-Agent Conflict Resolution"

Detects conflicts when multiple agents produce incompatible results for the same target.
Resolves via configurable strategies: authority, evidence-based, synthesis, tournament, escalation.
All conflicts and resolutions are auditable events.

Distributed-safe: stateless, all state in events.
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass
from enum import Enum
from typing import Any

from sqlalchemy.orm import Session
from core.db_consumer import DbConsumer, DbFactory

logger = logging.getLogger(__name__)


class ResolutionStrategy(str, Enum):
    """Conflict resolution strategies."""

    AUTHORITY = "authority"
    EVIDENCE_BASED = "evidence_based"
    SYNTHESIS = "synthesis"
    TOURNAMENT = "tournament"
    ESCALATION = "escalation"


class VoteType(str, Enum):
    """Vote types for consensus."""

    APPROVE = "approve"
    OBJECT = "object"


@dataclass
class Proposal:
    """A proposal from an agent."""

    agent_id: str
    action: str
    reasoning: str
    evidence: dict[str, Any] | None = None
    priority: int = 0  # Higher = more important


@dataclass
class Conflict:
    """A detected conflict between proposals."""

    conflict_id: str
    target: str  # What they're conflicting about
    proposals: list[Proposal]
    detected_at: str
    resolution_strategy: ResolutionStrategy | None = None
    resolution: str | None = None


class ConflictResolver(DbConsumer):
    """Detect and resolve conflicts between agent proposals.

    Distributed-safe: all operations are event-based.
    """

    def __init__(self, db_factory: DbFactory, event_logger=None) -> None:
        super().__init__(db_factory)
        self.event_logger = event_logger

    def detect_conflict(
        self,
        proposals: list[Proposal],
        target: str,
        session_id: str,
    ) -> Conflict | None:
        """Detect if proposals conflict.

        Simple heuristic: if proposals have contradictory actions on same target.

        Args:
            proposals: List of proposals
            target: What they're about (e.g., "function_X")
            session_id: Session ID

        Returns:
            Conflict object if detected, None otherwise
        """
        if len(proposals) < 2:
            return None

        # Check for contradictory actions
        actions = set(p.action for p in proposals)
        if len(actions) == 1:
            return None  # All agree

        # Conflict detected
        from uuid_utils import uuid7
        from datetime import datetime, timezone

        conflict_id = str(uuid7())
        conflict = Conflict(
            conflict_id=conflict_id,
            target=target,
            proposals=proposals,
            detected_at=datetime.now(timezone.utc).isoformat(),
        )

        # Log conflict event
        if self.event_logger:
            self.event_logger.create_event(
                user_id="system",
                session_id=session_id,
                event_type="conflict_detected",
                content=f"Conflict on {target}: {len(proposals)} proposals",
                metadata={
                    "conflict_id": conflict_id,
                    "target": target,
                    "proposals": [
                        {
                            "agent_id": p.agent_id,
                            "action": p.action,
                            "reasoning": p.reasoning,
                        }
                        for p in proposals
                    ],
                },
            )

        logger.info(f"Conflict detected: {conflict_id} on {target}")
        return conflict

    def resolve_by_authority(
        self,
        conflict: Conflict,
        priority_order: list[str],
    ) -> Proposal:
        """Resolve by authority: highest-priority agent wins.

        Args:
            conflict: The conflict
            priority_order: List of agent IDs in priority order

        Returns:
            Winning proposal
        """
        for agent_id in priority_order:
            for proposal in conflict.proposals:
                if proposal.agent_id == agent_id:
                    logger.info(f"Conflict {conflict.conflict_id} resolved by authority: {agent_id}")
                    return proposal

        # Fallback: first proposal
        return conflict.proposals[0]

    def resolve_by_evidence(
        self,
        conflict: Conflict,
        scoring_fn=None,
    ) -> Proposal:
        """Resolve by evidence: score proposals and pick highest.

        Args:
            conflict: The conflict
            scoring_fn: Optional callable(proposal) -> score. Default: use evidence quality.

        Returns:
            Winning proposal
        """
        if scoring_fn:
            scored = [(p, scoring_fn(p)) for p in conflict.proposals]
        else:
            # Simple: proposals with evidence score higher
            scored = [
                (p, len(p.evidence or {}) if p.evidence else 0)
                for p in conflict.proposals
            ]

        winner = max(scored, key=lambda x: x[1])[0]
        logger.info(f"Conflict {conflict.conflict_id} resolved by evidence: {winner.agent_id}")
        return winner

    def request_consensus(
        self,
        proposal: Proposal,
        team_members: list[str],
        session_id: str,
    ) -> dict[str, VoteType]:
        """Request consensus votes from team members.

        Args:
            proposal: Proposal to vote on
            team_members: List of agent IDs to vote
            session_id: Session ID

        Returns:
            Dict of agent_id -> vote
        """
        votes: dict[str, VoteType] = {}

        # In real implementation, this would send messages and collect responses.
        # For now, return empty (caller would populate via agent responses).
        for agent_id in team_members:
            if self.event_logger:
                self.event_logger.create_event(
                    user_id="system",
                    session_id=session_id,
                    event_type="consensus_vote_requested",
                    content=f"Vote on: {proposal.action}",
                    metadata={
                        "proposal_agent": proposal.agent_id,
                        "voter": agent_id,
                        "action": proposal.action,
                    },
                )

        return votes

    def record_vote(
        self,
        voter_id: str,
        proposal_agent: str,
        vote: VoteType,
        reason: str | None = None,
        session_id: str | None = None,
    ) -> None:
        """Record a consensus vote.

        Args:
            voter_id: Agent voting
            proposal_agent: Agent who made the proposal
            vote: APPROVE or OBJECT
            reason: Optional reason for objection
            session_id: Session ID
        """
        if self.event_logger and session_id:
            self.event_logger.create_event(
                user_id="system",
                session_id=session_id,
                event_type="consensus_vote",
                content=f"{voter_id} votes {vote.value}",
                metadata={
                    "voter": voter_id,
                    "proposal_agent": proposal_agent,
                    "vote": vote.value,
                    "reason": reason,
                },
            )

        logger.info(f"Vote recorded: {voter_id} → {vote.value}")
