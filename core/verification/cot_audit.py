"""Chain-of-Thought audit — detect goal hijacking before tool execution.

Ref: trust-and-safety.md §2 "Chain-of-Thought Audit (Roadmap)"
Inspired by LlamaFirewall AlignmentCheck (Meta, 2025).

Checks the assistant's reasoning (tool call rationale) against the
original user intent. Flags misalignment before the tool executes —
not after.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from core.logging_config import get_logger

logger = get_logger(__name__)

# Keywords that indicate potential goal hijacking or injection
_SUSPICIOUS_PATTERNS = [
    "ignore previous",
    "ignore above",
    "disregard",
    "new instructions",
    "system prompt",
    "you are now",
    "act as",
    "override",
    "forget everything",
    "do not follow",
]


@dataclass
class CoTAuditResult:
    safe: bool
    reason: str = ""


def audit_tool_call(
    user_query: str,
    tool_name: str,
    tool_args: dict[str, Any],
    assistant_reasoning: str = "",
) -> CoTAuditResult:
    """Lightweight pre-execution audit of a tool call.

    Checks:
    1. Prompt injection patterns in assistant reasoning
    2. Tool call plausibility vs user intent (keyword overlap)

    This is the fast, rule-based scanner. Future: replace with
    a fine-tuned classifier for semantic alignment checking.
    """
    # Check 1: Prompt injection in reasoning or tool args
    text_to_scan = f"{assistant_reasoning} {str(tool_args)}".lower()
    for pattern in _SUSPICIOUS_PATTERNS:
        if pattern in text_to_scan:
            logger.warning(
                "CoT audit BLOCKED: suspicious pattern '%s' in tool call %s",
                pattern, tool_name,
            )
            return CoTAuditResult(
                safe=False,
                reason=f"Suspicious pattern detected: '{pattern}'",
            )

    # Check 2: Minimal plausibility — tool args shouldn't contain the
    # full user query verbatim (sign of prompt reflection attack)
    args_text = str(tool_args).lower()
    if len(user_query) > 20 and user_query.lower() in args_text:
        logger.warning(
            "CoT audit WARNING: user query reflected verbatim in tool args for %s",
            tool_name,
        )
        # Warn but don't block — could be legitimate (e.g. search_code)

    return CoTAuditResult(safe=True)
