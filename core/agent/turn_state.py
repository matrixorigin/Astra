"""TurnState, TurnOutcome, PipelineStage protocol, and ExecutionPipeline.

Explicit state for the agent execution loop. TurnState flows through
pipeline stages; each stage reads/mutates state and yields TurnEvents
for streaming + observability.
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from enum import Enum
from typing import TYPE_CHECKING, Any, Protocol, runtime_checkable

if TYPE_CHECKING:
    from collections.abc import AsyncIterator


class TurnStatus(str, Enum):
    """Outcome status of a turn."""

    SUCCESS = "success"
    FAILURE = "failure"
    EXHAUSTED = "exhausted"


@dataclass
class TurnOutcome:
    """Result of a completed turn."""

    status: TurnStatus
    content: str = ""
    failure_reason: str | None = None
    failed_tools: list[str] = field(default_factory=list)
    verification: Any | None = None  # FirewallResult


@dataclass
class TurnEvent:
    """Internal pipeline event — converted to StreamEvent at the boundary."""

    event_type: str  # e.g. "llm_chunk", "tool_start", "tool_result", "stage_complete", "turn_complete"
    data: dict[str, Any] = field(default_factory=dict)


@dataclass
class TurnState:
    """Mutable state threaded through every pipeline stage."""

    messages: list[dict[str, Any]]
    tools_schema: list[dict[str, Any]]
    round: int = 0
    max_rounds: int = 10
    tool_failures: dict[str, list[str]] = field(default_factory=dict)
    blocked_tools: set[str] = field(default_factory=set)
    tokens_consumed: int = 0
    wall_clock_start: float = field(default_factory=time.monotonic)
    wall_clock_timeout: float = 300.0  # seconds
    outcome: TurnOutcome | None = None

    # Metadata carried through the turn (not serialized to wire)
    session_id: str = ""
    user_id: str = ""
    user_input: str = ""
    context_capture_id: str = ""
    user_event: Any = None  # ConversationEvent
    last_skill_name: str | None = None

    # --- wire serialization (edge ↔ cloud) ---

    def to_wire(self) -> dict[str, Any]:
        """Serialize subset for edge-cloud transport."""
        return {
            "blocked_tools": sorted(self.blocked_tools),
            "tool_failures": {k: v[:] for k, v in self.tool_failures.items()},
            "round": self.round,
            "max_rounds": self.max_rounds,
            "outcome": {
                "status": self.outcome.status.value,
                "content": self.outcome.content,
                "failure_reason": self.outcome.failure_reason,
                "failed_tools": self.outcome.failed_tools,
            } if self.outcome else None,
        }

    @classmethod
    def from_wire(cls, data: dict[str, Any], **kwargs: Any) -> TurnState:
        """Deserialize from wire with validation.

        Cloud-side: max_rounds capped, blocked_tools additive-only.
        """
        max_rounds = min(data.get("max_rounds", 10), 20)  # cap at 20
        state = cls(
            messages=kwargs.get("messages", []),
            tools_schema=kwargs.get("tools_schema", []),
            round=max(data.get("round", 0), 0),  # clamp non-negative
            max_rounds=max_rounds,
            blocked_tools=set(data.get("blocked_tools", [])),
            tool_failures={k: v[:] for k, v in data.get("tool_failures", {}).items()},
        )
        if data.get("outcome"):
            o = data["outcome"]
            try:
                status = TurnStatus(o["status"])
            except (ValueError, KeyError):
                status = TurnStatus.FAILURE  # Defensive: unknown status → treat as failure
            state.outcome = TurnOutcome(
                status=status,
                content=o.get("content", ""),
                failure_reason=o.get("failure_reason"),
                failed_tools=o.get("failed_tools", []),
            )
        return state


@runtime_checkable
class PipelineStage(Protocol):
    """A single stage in the execution pipeline."""

    async def __call__(self, state: TurnState) -> AsyncIterator[TurnEvent]: ...


@dataclass
class ExecutionPipeline:
    """Ordered collection of stages: pre_loop → loop_body (repeated) → post_loop."""

    pre_loop: list[PipelineStage] = field(default_factory=list)
    loop_body: list[PipelineStage] = field(default_factory=list)
    post_loop: list[PipelineStage] = field(default_factory=list)
