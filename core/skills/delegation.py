"""Delegation skill for multi-agent collaboration."""

import asyncio

from core.logging_config import get_logger
from core.skills.base import (
    AccessScope,
    RepoType,
    SideEffectCategory,
    SideEffectProfile,
    Skill,
    SkillInput,
    SkillOutput,
    SkillRequirement,
)

logger = get_logger(__name__)


class DelegateTaskInput(SkillInput):
    """Input for delegation skill.
    
    Context Structure:
        When delegating, the context is passed to the delegated agent as:
        {
            "system_prompt": str,           # Agent's system prompt
            "agent_id": str,                # Delegated agent's ID
            "delegation_context": str,      # Optional: context from parent (if provided)
        }
        
        The delegated agent can access parent context via context["delegation_context"].
    """

    agent_id: str
    task: str
    context: str | None = None
    timeout: float | None = None  # Timeout in seconds, None = no timeout


class DelegateTaskOutput(SkillOutput):
    """Output for delegation skill."""

    result: str
    agent_id: str
    events_produced: int


class DelegateTaskSkill(Skill):
    """Delegate a task to another agent.

    This skill enables orchestrator agents to delegate work to specialist agents.
    The delegation is logged as an event for auditability.
    """

    name: str = "delegate_task"
    version: str = "1.0.0"
    description: str = "Delegate a task to another agent for execution"
    requirements: SkillRequirement = SkillRequirement(
        repo_types=[RepoType.CODE, RepoType.DOCS], min_access=AccessScope.READ, llm_required=True
    )
    side_effect_profile = SideEffectProfile(category=SideEffectCategory.READ)

    def __init__(self, agent_registry, chat_loop_factory):
        """Initialize delegation skill.

        Args:
            agent_registry: AgentRegistry instance for looking up agent profiles
            chat_loop_factory: Factory function that creates ChatLoop instances
        """
        self.registry = agent_registry
        self.make_loop = chat_loop_factory

    def validate_input(self, input_data: dict) -> DelegateTaskInput:
        """Validate and parse input data."""
        # Extract session_id and user_id from input or use defaults
        session_id = input_data.get("session_id", "default_session")
        user_id = input_data.get("user_id", "default_user")

        return DelegateTaskInput(
            agent_id=input_data.get("agent_id", ""),
            task=input_data.get("task", ""),
            context=input_data.get("context"),
            session_id=session_id,
            user_id=user_id,
        )

    async def execute(self, input: DelegateTaskInput) -> DelegateTaskOutput:
        """Execute the delegation.

        Args:
            input: DelegateTaskInput with agent_id, task, and optional context

        Returns:
            DelegateTaskOutput with result from delegated agent
        """
        profile = self.registry.get(input.agent_id)

        if not profile:
            return DelegateTaskOutput(
                success=False,
                result=f"Error: Agent '{input.agent_id}' not found",
                agent_id=input.agent_id,
                events_produced=0,
            )

        # Create a new ChatLoop for the delegated agent with its agent_id
        loop = self.make_loop(
            system_prompt=profile.system_prompt,
            agent_id=input.agent_id,
        )

        # Build context with agent profile
        context = {
            "system_prompt": profile.system_prompt,
            "agent_id": input.agent_id,
        }
        if input.context:
            context["delegation_context"] = input.context

        # Execute the task
        result = await loop.run_step(
            user_input=input.task,
            session_id=input.session_id,
            user_id=input.user_id,
            context=context,
        )

        return DelegateTaskOutput(
            success=True,
            result=result,
            agent_id=input.agent_id,
            events_produced=0,  # Will be counted by the loop
        )
    
    async def execute_stream(self, input: DelegateTaskInput):
        """Execute delegation with streaming output.
        
        Args:
            input: DelegateTaskInput with agent_id, task, and optional context
            
        Yields:
            StreamEvent: Stream events from delegated agent with agent_id tagged
        """
        from core.events.models import StreamEvent, StreamEventType
        
        profile = self.registry.get(input.agent_id)

        if not profile:
            # Yield error event
            yield StreamEvent(
                event_type=StreamEventType.RUN_ERROR,
                data={"error": f"Agent '{input.agent_id}' not found"},
                agent_id=input.agent_id,
            )
            return

        # Yield delegation start marker
        yield StreamEvent(
            event_type=StreamEventType.AGENT_DELEGATED,
            data={"agent_id": input.agent_id, "task": input.task},
            agent_id=input.agent_id,
        )

        try:
            # Create a new ChatLoop for the delegated agent with its agent_id
            loop = self.make_loop(
                system_prompt=profile.system_prompt,
                agent_id=input.agent_id,
            )

            # Build context with agent profile
            context = {
                "system_prompt": profile.system_prompt,
                "agent_id": input.agent_id,
            }
            if input.context:
                context["delegation_context"] = input.context

            # Stream the task execution with optional timeout
            stream = loop.run_step_stream(
                user_input=input.task,
                session_id=input.session_id,
                user_id=input.user_id,
                context=context,
            )
            
            # Apply timeout using asyncio.timeout() context manager (Python 3.11+)
            if input.timeout:
                try:
                    async with asyncio.timeout(input.timeout):
                        async for event in stream:
                            event.agent_id = input.agent_id
                            yield event
                except TimeoutError:
                    logger.error(f"Delegation to agent '{input.agent_id}' timed out after {input.timeout}s")
                    yield StreamEvent(
                        event_type=StreamEventType.RUN_ERROR,
                        data={"error": f"Timeout after {input.timeout}s", "agent_id": input.agent_id},
                        agent_id=input.agent_id,
                    )
                    return
            else:
                # No timeout - stream normally
                async for event in stream:
                    event.agent_id = input.agent_id
                    yield event
        
        except asyncio.CancelledError:
            logger.warning(f"Delegation to agent '{input.agent_id}' was cancelled")
            yield StreamEvent(
                event_type=StreamEventType.RUN_ERROR,
                data={"error": "Cancelled", "agent_id": input.agent_id},
                agent_id=input.agent_id,
            )
            raise  # Re-raise to propagate cancellation
        except Exception as e:
            logger.error(f"Error in delegation to agent '{input.agent_id}': {e}", exc_info=True)
            yield StreamEvent(
                event_type=StreamEventType.RUN_ERROR,
                data={"error": str(e), "agent_id": input.agent_id},
                agent_id=input.agent_id,
            )
        
        # Yield delegation completion marker
        yield StreamEvent(
            event_type=StreamEventType.AGENT_COMPLETED,
            data={"agent_id": input.agent_id},
            agent_id=input.agent_id,
        )
    
    async def execute_parallel(
        self, inputs: list[DelegateTaskInput]
    ) -> list[DelegateTaskOutput]:
        """Execute multiple delegations in parallel.
        
        Args:
            inputs: List of delegation inputs
            
        Returns:
            List of outputs in same order as inputs
        """
        tasks = [self.execute(inp) for inp in inputs]
        results = await asyncio.gather(*tasks, return_exceptions=True)
        
        # Convert exceptions to error outputs
        outputs = []
        for i, result in enumerate(results):
            if isinstance(result, Exception):
                outputs.append(
                    DelegateTaskOutput(
                        success=False,
                        result=f"Error: {str(result)}",
                        agent_id=inputs[i].agent_id,
                        events_produced=0,
                    )
                )
            else:
                outputs.append(result)
        
        return outputs
    
    async def execute_parallel_stream(self, inputs: list[DelegateTaskInput]):
        """Execute multiple delegations in parallel with streaming.
        
        Fan-out: Start all delegations in parallel
        Stream: Multiplex events from all agents
        Fan-in: Collect all results when complete
        
        Args:
            inputs: List of delegation inputs
            
        Yields:
            StreamEvent: Multiplexed stream events from all agents
        """
        from core.events.models import StreamEvent, StreamEventType
        
        if not inputs:
            return
        
        # Track results and completion
        results = {}
        errors = set()  # Track which delegations had errors
        completed_count = [0]  # Use list for closure
        
        # Create all streams
        streams = [self.execute_stream(inp) for inp in inputs]
        
        # Use asyncio.Queue for event multiplexing
        queue = asyncio.Queue()
        
        async def consume_stream(idx, stream):
            """Consume a single stream and put events in queue."""
            try:
                async for event in stream:
                    await queue.put((idx, event))
                    
                    # Track errors
                    if event.event_type == StreamEventType.RUN_ERROR:
                        errors.add(idx)
                        results[idx] = event.data.get("error", "Unknown error")
                    # Track completion - collect any final text
                    elif event.event_type == StreamEventType.TEXT_DONE:
                        if idx not in errors:  # Only if no error
                            results[idx] = event.data.get("text", "")
                    elif event.event_type == StreamEventType.AGENT_COMPLETED:
                        # Ensure we mark as completed even without TEXT_DONE
                        if idx not in results:
                            results[idx] = ""  # Empty result but completed
                        
            except Exception as e:
                logger.error(f"Error in delegation stream {idx} (agent={inputs[idx].agent_id}): {e}", exc_info=True)
                errors.add(idx)
                await queue.put((idx, StreamEvent(
                    event_type=StreamEventType.RUN_ERROR,
                    data={"error": str(e), "agent_id": inputs[idx].agent_id},
                    agent_id=inputs[idx].agent_id,
                )))
                # Mark as failed
                results[idx] = f"Error: {str(e)}"
            finally:
                completed_count[0] += 1
                if completed_count[0] == len(inputs):
                    await queue.put(None)  # Sentinel
        
        # Start all consumers
        tasks = [asyncio.create_task(consume_stream(i, s)) for i, s in enumerate(streams)]
        
        # Yield events from queue
        while True:
            item = await queue.get()
            if item is None:  # Sentinel
                break
            idx, event = item
            yield event
        
        # Wait for all tasks to complete
        results_with_exceptions = await asyncio.gather(*tasks, return_exceptions=True)
        
        # Log any exceptions that weren't caught
        for i, result in enumerate(results_with_exceptions):
            if isinstance(result, Exception):
                logger.error(f"Unhandled exception in delegation {i} (agent={inputs[i].agent_id}): {result}", exc_info=result)
        
        # Fan-in: Yield aggregated results
        aggregated = {
            "delegations": [
                {
                    "agent_id": inputs[i].agent_id,
                    "task": inputs[i].task,
                    "result": results.get(i, ""),
                    "success": i in results and i not in errors,
                }
                for i in range(len(inputs))
            ],
            "total": len(inputs),
            "successful": sum(1 for i in range(len(inputs)) if i in results and i not in errors),
            "failed": len(errors),
        }
        
        yield StreamEvent(
            event_type=StreamEventType.AGENT_PROGRESS,
            data={"aggregated_results": aggregated},
            agent_id="orchestrator",
        )
