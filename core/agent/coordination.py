"""Coordination patterns for multi-agent collaboration.

Implements fan-out/fan-in, pipeline, and adversarial review patterns
as described in agents-and-orchestration.md.

Supports streaming for real-time multi-agent progress visualization.
"""

import asyncio
from collections.abc import AsyncIterator
from dataclasses import dataclass
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

    def fan_in(self, results: list[Result]) -> str:
        """Collect and synthesize results (fan-in pattern).
        
        Args:
            results: List of results from parallel execution
            
        Returns:
            Synthesized summary of all results
        """
        successful = [r for r in results if r.success]
        failed = [r for r in results if not r.success]
        
        summary_parts = []
        
        if successful:
            summary_parts.append(f"✅ {len(successful)} tasks completed successfully:")
            for r in successful:
                summary_parts.append(f"  [{r.agent_id}]: {r.output[:200]}...")
        
        if failed:
            summary_parts.append(f"\n❌ {len(failed)} tasks failed:")
            for r in failed:
                summary_parts.append(f"  [{r.agent_id}]: {r.error}")
        
        return "\n".join(summary_parts)

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
