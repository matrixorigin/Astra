"""Coordination patterns for multi-agent collaboration.

Implements fan-out/fan-in, pipeline, and adversarial review patterns
as described in agents-and-orchestration.md.

Supports streaming for real-time multi-agent progress visualization.
"""

import asyncio
import re
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from typing import Any

from core.events.models import StreamEvent
from core.logging_config import get_logger

logger = get_logger(__name__)


@dataclass
class Task:
    """A task to be executed by an agent."""

    agent_id: str
    description: str
    context: dict[str, Any] | None = None


@dataclass
class Result:
    """Result from an agent execution."""

    agent_id: str
    success: bool
    output: str
    error: str | None = None


# ---------------------------------------------------------------------------
# Structured aggregation result (replaces raw string fan_in)
# ---------------------------------------------------------------------------

@dataclass
class Conflict:
    """A detected conflict between agent results."""

    artifact: str  # target artifact (e.g. file path, function name)
    agents: list[str]  # agents that disagree
    proposals: list[str]  # their competing proposals
    severity: str = "info"  # "info" | "warning" | "blocking"


@dataclass
class AggregatedResult:
    """Structured fan-in output with quality metrics and conflict detection."""

    results: list[Result]
    conflicts: list[Conflict] = field(default_factory=list)
    resolutions: list[dict] = field(default_factory=list)
    success_rate: float = 0.0
    total: int = 0
    succeeded: int = 0
    failed: int = 0

    @property
    def has_conflicts(self) -> bool:
        return len(self.conflicts) > 0

    @property
    def summary(self) -> str:
        """Human-readable summary (backward-compatible with old fan_in str)."""
        parts = []
        if self.succeeded:
            parts.append(f"✅ {self.succeeded}/{self.total} tasks succeeded:")
            for r in self.results:
                if r.success:
                    parts.append(f"  [{r.agent_id}]: {r.output}")
        if self.failed:
            parts.append(f"❌ {self.failed}/{self.total} tasks failed:")
            for r in self.results:
                if not r.success:
                    parts.append(f"  [{r.agent_id}]: {r.error}")
        if self.conflicts:
            parts.append(f"⚠️ {len(self.conflicts)} conflict(s) detected:")
            for c in self.conflicts:
                parts.append(
                    f"  [{c.artifact}] {c.severity}: "
                    f"{', '.join(c.agents)} have competing proposals"
                )
        return "\n".join(parts)


# Artifact patterns: only match tokens with clear artifact structure.
# 1. Path-like: contains '/' or '.' with extension (auth.py, core/utils.py)
# 2. Callable: word followed by () (validate(), check_auth())
# 3. Keyword-prefixed: "file X", "function X", etc.
_PATH_RE = re.compile(r"[`'\"]?([\w/\-]+\.[\w]+)[`'\"]?")
_CALLABLE_RE = re.compile(r"[`'\"]?([\w_]+\(\))[`'\"]?")
_KEYWORD_RE = re.compile(
    r"(?:file|function|class|module|table|endpoint)\s+[`'\"]?([\w./\-]+)[`'\"]?",
    re.IGNORECASE,
)


def _extract_artifacts(text: str) -> set[str]:
    """Extract artifact names (files, functions) from agent output.

    Only matches tokens with structural artifact indicators:
    path separators, file extensions, call parens, or keyword prefixes.
    Plain words like 'error' or 'critical' are NOT matched.
    """
    artifacts: set[str] = set()
    for pattern in (_PATH_RE, _CALLABLE_RE, _KEYWORD_RE):
        for m in pattern.finditer(text):
            token = m.group(1).lower()
            if len(token) > 2:
                artifacts.add(token)
    return artifacts


def detect_conflicts(results: list[Result]) -> list[Conflict]:
    """Detect conflicts: multiple successful agents referencing the same artifact.

    This is structural detection (same artifact mentioned by different agents).
    Semantic conflict analysis (modify vs don't modify) requires LLM and is
    left to the lead agent's synthesis step.
    """
    successful = [r for r in results if r.success]
    if len(successful) < 2:
        return []

    # Map artifact → list of (agent_id, output snippet)
    artifact_agents: dict[str, list[tuple[str, str]]] = {}
    for r in successful:
        for artifact in _extract_artifacts(r.output):
            artifact_agents.setdefault(artifact, []).append((r.agent_id, r.output))

    conflicts = []
    for artifact, agents in artifact_agents.items():
        if len(agents) < 2:
            continue
        agent_ids = [a[0] for a in agents]
        # Only flag if different agents (not same agent mentioned twice)
        if len(set(agent_ids)) < 2:
            continue
        conflicts.append(Conflict(
            artifact=artifact,
            agents=agent_ids,
            proposals=[a[1] for a in agents],
            severity="warning",
        ))

    return sorted(conflicts, key=lambda c: c.artifact)


class CoordinationPatterns:
    """Coordination patterns for multi-agent workflows."""

    def __init__(self, delegation_skill):
        """Initialize with delegation skill for executing tasks.
        
        Args:
            delegation_skill: DelegateTaskSkill instance for task execution
        """
        self.delegate = delegation_skill

    async def fan_out_stream(
        self, tasks: list[Task], session_id: str, user_id: str
    ) -> AsyncIterator[StreamEvent]:
        """Execute tasks in parallel with streaming (fan-out pattern).
        
        Args:
            tasks: List of tasks to execute in parallel
            session_id: Session ID for event logging
            user_id: User ID for event logging
            
        Yields:
            StreamEvent: Multiplexed stream from all agents
        """
        from core.agent.stream_multiplexer import StreamMultiplexer
        from core.skills.delegation import DelegateTaskInput
        
        async def execute_task_stream(task: Task) -> AsyncIterator[StreamEvent]:
            """Execute task and yield stream events."""
            try:
                input_data = DelegateTaskInput(
                    agent_id=task.agent_id,
                    task=task.description,
                    context=str(task.context) if task.context else None,
                    session_id=session_id,
                    user_id=user_id,
                )
                
                # Get stream from delegation
                async for event in self.delegate.execute_stream(input_data):
                    yield event
                    
            except Exception as e:
                logger.error(f"Task stream failed for {task.agent_id}: {e}")
        
        # Create streams for all tasks
        streams = {task.agent_id: execute_task_stream(task) for task in tasks}
        
        # Multiplex streams
        multiplexer = StreamMultiplexer()
        async for event in multiplexer.merge_streams(streams):
            yield event

    async def fan_out(self, tasks: list[Task], session_id: str, user_id: str) -> list[Result]:
        """Execute tasks in parallel (fan-out pattern).
        
        Args:
            tasks: List of tasks to execute in parallel
            session_id: Session ID for event logging
            user_id: User ID for event logging
            
        Returns:
            List of results in same order as tasks
        """
        async def execute_task(task: Task) -> Result:
            try:
                from core.skills.delegation import DelegateTaskInput
                
                input_data = DelegateTaskInput(
                    agent_id=task.agent_id,
                    task=task.description,
                    context=str(task.context) if task.context else None,
                    session_id=session_id,
                    user_id=user_id,
                )
                
                output = await self.delegate.execute(input_data)
                
                return Result(
                    agent_id=task.agent_id,
                    success=output.success,
                    output=output.result,
                    error=None if output.success else output.result,
                )
            except Exception as e:
                logger.error(f"Task execution failed for {task.agent_id}: {e}")
                return Result(
                    agent_id=task.agent_id,
                    success=False,
                    output="",
                    error=str(e),
                )

        # Execute all tasks concurrently
        results = await asyncio.gather(*[execute_task(task) for task in tasks])
        return list(results)

    def fan_in(self, results: list[Result], *, resolve: bool = False, priority_order: list[str] | None = None) -> AggregatedResult:
        """Collect, evaluate quality, detect conflicts, and synthesize results.

        Args:
            results: Agent results to aggregate
            resolve: If True, auto-resolve conflicts using ConflictResolver
            priority_order: Agent priority for authority-based resolution

        Returns structured AggregatedResult with:
        - success_rate: fraction of tasks that succeeded
        - conflicts: detected artifact-level disagreements between agents
        - summary: human-readable synthesis (backward-compatible)
        """
        total = len(results)
        succeeded = sum(1 for r in results if r.success)
        success_rate = succeeded / total if total else 0.0

        conflicts = detect_conflicts(results)
        if conflicts:
            logger.warning(
                "fan_in: %d conflict(s) across %d results", len(conflicts), total,
            )

        # Auto-resolve conflicts if requested
        resolutions: list[dict] | None = None
        if resolve and conflicts:
            try:
                from core.agents.conflict_resolver import ConflictResolver as CR
                from core.agents.conflict_resolver import Proposal
                resolver = CR(db=None)  # stateless resolution, no DB needed
                resolutions = []
                for c in conflicts:
                    proposals = [
                        Proposal(agent_id=aid, action=c.proposals[i][:200], reasoning="")
                        for i, aid in enumerate(c.agents)
                    ]
                    cr_conflict = resolver.detect_conflict(proposals, c.artifact, session_id="")
                    if cr_conflict and priority_order:
                        winner = resolver.resolve_by_authority(cr_conflict, priority_order)
                        resolutions.append({"artifact": c.artifact, "winner": winner.agent_id, "method": "authority"})
                    elif cr_conflict:
                        winner = resolver.resolve_by_evidence(cr_conflict)
                        resolutions.append({"artifact": c.artifact, "winner": winner.agent_id, "method": "evidence"})
            except Exception as e:
                logger.warning("Conflict resolution failed (non-fatal): %s", e)

        agg = AggregatedResult(
            results=results,
            conflicts=conflicts,
            success_rate=success_rate,
            total=total,
            succeeded=succeeded,
            failed=total - succeeded,
        )
        if resolutions:
            agg.resolutions = resolutions
        return agg

    async def pipeline(
        self, steps: list[Task], session_id: str, user_id: str
    ) -> Result:
        """Execute tasks sequentially, passing output to next step.
        
        Args:
            steps: List of tasks to execute in sequence
            session_id: Session ID for event logging
            user_id: User ID for event logging
            
        Returns:
            Final result from last step
        """
        previous_output = ""
        
        for step in steps:
            # Inject previous output into context
            if step.context is None:
                step.context = {}
            step.context["previous_output"] = previous_output
            
            # Execute step
            results = await self.fan_out([step], session_id, user_id)
            result = results[0]
            
            if not result.success:
                # Early termination on failure
                logger.warning(
                    f"Pipeline step '{step.agent_id}' failed: {result.error or result.output}"
                )
                return result
            
            previous_output = result.output
        
        # Return final result
        return Result(
            agent_id="pipeline",
            success=True,
            output=previous_output,
        )

    async def adversarial_review(
        self,
        proposal: str,
        proposer_agent: str,
        reviewer_agent: str,
        session_id: str,
        user_id: str,
        max_rounds: int = 3,
        approval_keywords: list[str] | None = None,
    ) -> Result:
        """Iterative refinement through adversarial review.
        
        Args:
            proposal: Initial proposal to review
            proposer_agent: Agent that generates/revises proposals
            reviewer_agent: Agent that reviews proposals
            session_id: Session ID for event logging
            user_id: User ID for event logging
            max_rounds: Maximum review rounds
            approval_keywords: Keywords indicating approval (default: ["approved", "lgtm"])
            
        Returns:
            Final approved proposal or last revision
        """
        if approval_keywords is None:
            approval_keywords = ["approved", "lgtm"]
        
        current_proposal = proposal
        
        for round_num in range(max_rounds):
            # Review current proposal
            review_task = Task(
                agent_id=reviewer_agent,
                description=f"Review this proposal and provide feedback:\n\n{current_proposal}",
                context={"round": round_num + 1},
            )
            
            review_results = await self.fan_out([review_task], session_id, user_id)
            review = review_results[0]
            
            if not review.success:
                return Result(
                    agent_id="adversarial_review",
                    success=False,
                    output=current_proposal,
                    error=f"Review failed: {review.error}",
                )
            
            # Check if approved using configurable keywords
            review_lower = review.output.lower()
            if any(keyword in review_lower for keyword in approval_keywords):
                return Result(
                    agent_id="adversarial_review",
                    success=True,
                    output=current_proposal,
                )
            
            # Revise based on feedback
            if round_num < max_rounds - 1:  # Don't revise on last round
                revise_task = Task(
                    agent_id=proposer_agent,
                    description=f"Revise your proposal based on this feedback:\n\nFeedback: {review.output}\n\nOriginal: {current_proposal}",
                    context={"round": round_num + 1},
                )
                
                revise_results = await self.fan_out([revise_task], session_id, user_id)
                revision = revise_results[0]
                
                if not revision.success:
                    return Result(
                        agent_id="adversarial_review",
                        success=False,
                        output=current_proposal,
                        error=f"Revision failed: {revision.error}",
                    )
                
                current_proposal = revision.output
        
        # Max rounds exhausted
        return Result(
            agent_id="adversarial_review",
            success=True,
            output=current_proposal,
            error=f"Max rounds ({max_rounds}) reached without approval",
        )
