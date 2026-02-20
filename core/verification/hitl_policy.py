"""Human-in-the-Loop policy engine — policy-driven supervision.

Ref: trust-and-safety.md §9

Policies are data (stored in DB), not code. The engine evaluates all active
policies against a proposed action and returns the most restrictive required
supervision action.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session

from core.logging_config import get_logger

logger = get_logger(__name__)


class SupervisionAction(str, Enum):
    NONE = "none"
    OBSERVE_ONLY = "observe_only"
    APPROVE_REJECT = "approve_reject"
    REVIEW_AND_EDIT = "review_and_edit"
    TAKEOVER = "takeover"

    @property
    def severity(self) -> int:
        return _ACTION_SEVERITY[self]


_ACTION_SEVERITY = {
    SupervisionAction.NONE: 0,
    SupervisionAction.OBSERVE_ONLY: 1,
    SupervisionAction.APPROVE_REJECT: 2,
    SupervisionAction.REVIEW_AND_EDIT: 3,
    SupervisionAction.TAKEOVER: 4,
}


@dataclass
class SupervisionTrigger:
    cost_exceeds: float | None = None
    confidence_below: float | None = None
    affects_resources: list[str] | None = None
    plan_depth_exceeds: int | None = None
    novel_skill_use: bool = False
    escalated_by_agent: bool = False


@dataclass
class SupervisionPolicy:
    name: str
    trigger: SupervisionTrigger
    action: SupervisionAction
    scope: str = "global"  # global / tenant / agent
    scope_id: str | None = None
    enabled: bool = True


@dataclass
class ActionContext:
    """Context for a proposed agent action, evaluated against policies."""
    estimated_cost: float = 0.0
    confidence: float = 1.0
    resources: list[str] = field(default_factory=list)
    plan_depth: int = 0
    skill_name: str | None = None
    is_novel_skill: bool = False
    agent_escalated: bool = False
    agent_id: str | None = None
    tenant_id: str | None = None


@dataclass
class PolicyDecision:
    action: SupervisionAction
    triggered_policies: list[str]
    reason: str


class HITLPolicyEngine:
    """Evaluates supervision policies against proposed actions."""

    def __init__(self, db: Session | None = None):
        self.db = db
        self._policies: list[SupervisionPolicy] = []

    def load_policies(self, agent_id: str | None = None, tenant_id: str | None = None):
        """Load active policies from DB."""
        if not self.db:
            return
        try:
            rows = self.db.execute(text("""
                SELECT name, trigger_config, action, scope, scope_id, enabled
                FROM supervision_policies
                WHERE enabled = TRUE
                  AND (scope = 'global'
                       OR (scope = 'tenant' AND scope_id = :tenant_id)
                       OR (scope = 'agent' AND scope_id = :agent_id))
                ORDER BY name
            """), {"tenant_id": tenant_id or "", "agent_id": agent_id or ""}).fetchall()

            self._policies = []
            for row in rows:
                trigger_data = row[1] if isinstance(row[1], dict) else {}
                self._policies.append(SupervisionPolicy(
                    name=row[0],
                    trigger=SupervisionTrigger(
                        cost_exceeds=trigger_data.get("cost_exceeds"),
                        confidence_below=trigger_data.get("confidence_below"),
                        affects_resources=trigger_data.get("affects_resources"),
                        plan_depth_exceeds=trigger_data.get("plan_depth_exceeds"),
                        novel_skill_use=trigger_data.get("novel_skill_use", False),
                        escalated_by_agent=trigger_data.get("escalated_by_agent", False),
                    ),
                    action=SupervisionAction(row[2]),
                    scope=row[3],
                    scope_id=row[4],
                    enabled=bool(row[5]),
                ))
        except Exception as e:
            logger.warning("Failed to load supervision policies: %s", e)

    def add_policy(self, policy: SupervisionPolicy):
        """Add policy programmatically (for testing or bootstrap)."""
        self._policies.append(policy)

    def evaluate(self, ctx: ActionContext) -> PolicyDecision:
        """Evaluate all active policies. Returns most restrictive action."""
        triggered: list[tuple[SupervisionPolicy, str]] = []

        for policy in self._policies:
            if not policy.enabled:
                continue
            reason = self._check_trigger(policy.trigger, ctx)
            if reason:
                triggered.append((policy, reason))

        if not triggered:
            return PolicyDecision(
                action=SupervisionAction.NONE,
                triggered_policies=[],
                reason="no_policy_triggered",
            )

        # Most restrictive wins
        triggered.sort(key=lambda t: t[0].action.severity, reverse=True)
        winner = triggered[0]
        reasons = "; ".join(f"{p.name}: {r}" for p, r in triggered)

        decision = PolicyDecision(
            action=winner[0].action,
            triggered_policies=[p.name for p, _ in triggered],
            reason=reasons,
        )

        self._record(ctx, decision)
        return decision

    @staticmethod
    def _check_trigger(trigger: SupervisionTrigger, ctx: ActionContext) -> str | None:
        """Check if trigger matches context. Returns reason or None."""
        if trigger.cost_exceeds is not None and ctx.estimated_cost > trigger.cost_exceeds:
            return f"cost {ctx.estimated_cost:.2f} > {trigger.cost_exceeds:.2f}"

        if trigger.confidence_below is not None and ctx.confidence < trigger.confidence_below:
            return f"confidence {ctx.confidence:.2f} < {trigger.confidence_below:.2f}"

        if trigger.affects_resources:
            overlap = set(trigger.affects_resources) & set(ctx.resources)
            if overlap:
                return f"affects {overlap}"

        if trigger.plan_depth_exceeds is not None and ctx.plan_depth > trigger.plan_depth_exceeds:
            return f"plan_depth {ctx.plan_depth} > {trigger.plan_depth_exceeds}"

        if trigger.novel_skill_use and ctx.is_novel_skill:
            return f"novel skill: {ctx.skill_name}"

        if trigger.escalated_by_agent and ctx.agent_escalated:
            return "agent requested escalation"

        return None

    def _record(self, ctx: ActionContext, decision: PolicyDecision):
        """Record policy evaluation as auditable event."""
        if not self.db:
            return
        try:
            import json
            from uuid_extensions import uuid7
            self.db.execute(text("""
                INSERT INTO conversation_events
                    (event_id, session_id, user_id, agent_id, event_type,
                     content, causal_chain_id, created_at)
                VALUES
                    (:eid, 'system_hitl', :uid, :aid, 'hitl_policy_evaluation',
                     :content, :chain, NOW())
            """), {
                "eid": str(uuid7()),
                "uid": "system",
                "aid": ctx.agent_id or "system",
                "content": json.dumps({
                    "action": decision.action.value,
                    "triggered": decision.triggered_policies,
                    "reason": decision.reason,
                    "confidence": ctx.confidence,
                    "cost": ctx.estimated_cost,
                }),
                "chain": f"hitl_{ctx.agent_id or 'system'}",
            })
            self.db.commit()
        except Exception as e:
            logger.debug("Failed to record HITL decision: %s", e)
