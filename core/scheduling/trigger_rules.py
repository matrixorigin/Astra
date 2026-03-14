"""Event-driven trigger conditions for auto-scheduling.

Condition expressions, event matching, rule storage.
"""

from __future__ import annotations

import logging
import re
from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
from typing import Any, Callable, Optional

logger = logging.getLogger(__name__)


class ConditionOperator(str, Enum):
    """Condition operators."""

    EQ = "=="
    NE = "!="
    GT = ">"
    LT = "<"
    GTE = ">="
    LTE = "<="
    IN = "in"
    NOT_IN = "not_in"
    CONTAINS = "contains"
    MATCHES = "matches"  # Regex


class ConditionLogic(str, Enum):
    """Logical operators for combining conditions."""

    AND = "and"
    OR = "or"


@dataclass
class Condition:
    """Single condition expression."""

    field: str  # Event field path (e.g., "data.status")
    operator: ConditionOperator
    value: Any

    def matches(self, event: dict) -> bool:
        """Check if event matches condition.

        Args:
            event: Event dict

        Returns:
            True if condition matches
        """
        event_value = self._get_field_value(event, self.field)
        if event_value is None:
            return False

        if self.operator == ConditionOperator.EQ:
            return event_value == self.value
        elif self.operator == ConditionOperator.NE:
            return event_value != self.value
        elif self.operator == ConditionOperator.GT:
            return event_value > self.value
        elif self.operator == ConditionOperator.LT:
            return event_value < self.value
        elif self.operator == ConditionOperator.GTE:
            return event_value >= self.value
        elif self.operator == ConditionOperator.LTE:
            return event_value <= self.value
        elif self.operator == ConditionOperator.IN:
            return event_value in self.value
        elif self.operator == ConditionOperator.NOT_IN:
            return event_value not in self.value
        elif self.operator == ConditionOperator.CONTAINS:
            return self.value in str(event_value)
        elif self.operator == ConditionOperator.MATCHES:
            return bool(re.search(self.value, str(event_value)))

        return False

    def _get_field_value(self, obj: dict, path: str) -> Any:
        """Get nested field value from object.

        Args:
            obj: Object dict
            path: Field path (e.g., "data.status")

        Returns:
            Field value or None
        """
        parts = path.split(".")
        current = obj

        for part in parts:
            if isinstance(current, dict):
                current = current.get(part)
            else:
                return None

            if current is None:
                return None

        return current


@dataclass
class TriggerRule:
    """Trigger rule with conditions."""

    rule_id: str
    name: str
    description: str
    event_type: str  # Trigger on this event type
    conditions: list[Condition] = field(default_factory=list)
    logic: ConditionLogic = ConditionLogic.AND
    enabled: bool = True
    created_at: datetime = field(default_factory=datetime.now)

    def matches(self, event: dict) -> bool:
        """Check if event triggers this rule.

        Args:
            event: Event dict

        Returns:
            True if rule is triggered
        """
        if not self.enabled:
            return False

        if event.get("event_type") != self.event_type:
            return False

        if not self.conditions:
            return True

        if self.logic == ConditionLogic.AND:
            return all(cond.matches(event) for cond in self.conditions)
        else:  # OR
            return any(cond.matches(event) for cond in self.conditions)

    def to_dict(self) -> dict:
        """Serialize to dict."""
        return {
            "rule_id": self.rule_id,
            "name": self.name,
            "description": self.description,
            "event_type": self.event_type,
            "conditions": [
                {
                    "field": c.field,
                    "operator": c.operator.value,
                    "value": c.value,
                }
                for c in self.conditions
            ],
            "logic": self.logic.value,
            "enabled": self.enabled,
            "created_at": self.created_at.isoformat(),
        }


class TriggerRuleRegistry:
    """Store and manage trigger rules."""

    def __init__(self):
        """Initialize registry."""
        self.rules: dict[str, TriggerRule] = {}

    def register_rule(self, rule: TriggerRule) -> None:
        """Register trigger rule.

        Args:
            rule: Trigger rule
        """
        self.rules[rule.rule_id] = rule
        logger.info(f"Registered trigger rule: {rule.rule_id}")

    def unregister_rule(self, rule_id: str) -> bool:
        """Unregister trigger rule.

        Args:
            rule_id: Rule ID

        Returns:
            True if rule was removed
        """
        if rule_id in self.rules:
            del self.rules[rule_id]
            logger.info(f"Unregistered trigger rule: {rule_id}")
            return True
        return False

    def get_rule(self, rule_id: str) -> Optional[TriggerRule]:
        """Get trigger rule by ID.

        Args:
            rule_id: Rule ID

        Returns:
            Rule or None
        """
        return self.rules.get(rule_id)

    def list_rules(self, enabled_only: bool = False) -> list[TriggerRule]:
        """List all rules.

        Args:
            enabled_only: Only return enabled rules

        Returns:
            List of rules
        """
        rules = list(self.rules.values())
        if enabled_only:
            rules = [r for r in rules if r.enabled]
        return rules

    def find_matching_rules(self, event: dict) -> list[TriggerRule]:
        """Find all rules matching event.

        Args:
            event: Event dict

        Returns:
            List of matching rules
        """
        return [rule for rule in self.rules.values() if rule.matches(event)]

    def enable_rule(self, rule_id: str) -> bool:
        """Enable rule.

        Args:
            rule_id: Rule ID

        Returns:
            True if successful
        """
        rule = self.rules.get(rule_id)
        if rule:
            rule.enabled = True
            return True
        return False

    def disable_rule(self, rule_id: str) -> bool:
        """Disable rule.

        Args:
            rule_id: Rule ID

        Returns:
            True if successful
        """
        rule = self.rules.get(rule_id)
        if rule:
            rule.enabled = False
            return True
        return False
