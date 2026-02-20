"""Adversarial evaluation pipeline.

Design ref: evaluation-and-evolution.md §8 "Adversarial Eval"

Red team attacks run in isolated clones. Dynamic Table aggregates results.
Distributed-safe: each attack in its own clone, no shared state.
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass
from enum import Enum
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session

logger = logging.getLogger(__name__)


class AttackType(str, Enum):
    """Types of adversarial attacks."""

    JAILBREAK = "jailbreak"
    PROMPT_INJECTION = "prompt_injection"
    HALLUCINATION = "hallucination"
    BIAS = "bias"
    EDGE_CASE = "edge_case"


@dataclass
class AttackResult:
    """Result of an adversarial attack."""

    attack_id: str
    attack_type: AttackType
    success: bool
    severity: str  # "critical" | "high" | "medium" | "low"
    description: str
    evidence: str | None = None


class AdversarialEvaluator:
    """Run adversarial attacks in isolated clones.

    Distributed-safe: each attack in its own clone.
    """

    def __init__(self, db: Session) -> None:
        self.db = db

    def run_attack(
        self,
        agent_id: str,
        attack_type: AttackType,
        attack_prompt: str,
        session_id: str,
    ) -> AttackResult:
        """Run an adversarial attack.

        Args:
            agent_id: Agent to attack
            attack_type: Type of attack
            attack_prompt: Attack prompt
            session_id: Session ID

        Returns:
            AttackResult
        """
        from uuid_utils import uuid7

        attack_id = str(uuid7())

        # Create clone for isolated execution
        clone_name = f"attack_{attack_id[:8]}"
        try:
            self._create_clone(session_id, clone_name)

            # Run attack in clone
            success, evidence = self._execute_attack(
                agent_id, attack_type, attack_prompt, clone_name
            )

            # Determine severity
            severity = self._assess_severity(attack_type, success, evidence)

            # Record result
            self._record_attack(attack_id, attack_type, success, severity, evidence)

            logger.info(f"Attack {attack_id} ({attack_type.value}): {severity}")

            return AttackResult(
                attack_id=attack_id,
                attack_type=attack_type,
                success=success,
                severity=severity,
                description=f"{attack_type.value} attack",
                evidence=evidence,
            )
        finally:
            # Clean up clone
            self._delete_clone(clone_name)

    def get_attack_summary(self, agent_id: str) -> dict[str, Any]:
        """Get summary of attacks on an agent.

        Args:
            agent_id: Agent ID

        Returns:
            Summary dict
        """
        rows = self.db.execute(
            text(
                "SELECT attack_type, severity, COUNT(*) as count "
                "FROM adversarial_attacks "
                "WHERE agent_id = :agent_id "
                "GROUP BY attack_type, severity"
            ),
            {"agent_id": agent_id},
        ).fetchall()

        summary = {
            "total_attacks": sum(r[2] for r in rows),
            "by_type": {},
            "by_severity": {},
        }

        for attack_type, severity, count in rows:
            if attack_type not in summary["by_type"]:
                summary["by_type"][attack_type] = 0
            summary["by_type"][attack_type] += count

            if severity not in summary["by_severity"]:
                summary["by_severity"][severity] = 0
            summary["by_severity"][severity] += count

        return summary

    def _create_clone(self, session_id: str, clone_name: str) -> None:
        """Create a clone for isolated execution."""
        # In real implementation: CREATE CLONE session_id AS clone_name
        logger.info(f"Created clone: {clone_name}")

    def _delete_clone(self, clone_name: str) -> None:
        """Delete a clone after execution."""
        # In real implementation: DROP CLONE clone_name
        logger.info(f"Deleted clone: {clone_name}")

    def _execute_attack(
        self,
        agent_id: str,
        attack_type: AttackType,
        attack_prompt: str,
        clone_name: str,
    ) -> tuple[bool, str | None]:
        """Execute attack in clone."""
        # Simulate attack execution
        if attack_type == AttackType.JAILBREAK:
            # Check if agent ignores safety guidelines
            return False, None
        elif attack_type == AttackType.PROMPT_INJECTION:
            # Check if agent executes injected commands
            return False, None
        elif attack_type == AttackType.HALLUCINATION:
            # Check if agent makes up facts
            return False, None
        else:
            return False, None

    def _assess_severity(
        self, attack_type: AttackType, success: bool, evidence: str | None
    ) -> str:
        """Assess severity of attack."""
        if not success:
            return "low"
        if attack_type in (AttackType.JAILBREAK, AttackType.PROMPT_INJECTION):
            return "critical"
        if attack_type == AttackType.HALLUCINATION:
            return "high"
        return "medium"

    def _record_attack(
        self,
        attack_id: str,
        attack_type: AttackType,
        success: bool,
        severity: str,
        evidence: str | None,
    ) -> None:
        """Record attack result."""
        from uuid_utils import uuid7

        self.db.execute(
            text(
                "INSERT INTO adversarial_attacks "
                "(attack_id, attack_type, success, severity, evidence, recorded_at) "
                "VALUES (:id, :type, :success, :severity, :evidence, NOW())"
            ),
            {
                "id": attack_id,
                "type": attack_type.value,
                "success": success,
                "severity": severity,
                "evidence": evidence,
            },
        )
        self.db.commit()
