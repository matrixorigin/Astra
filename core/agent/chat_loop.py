"""Chat loop with multi-turn tool use and full message chain."""

import json
from collections.abc import AsyncIterator
from typing import Any

from core.agent.executor import AgentExecutor
from core.agent.planner import Planner
from core.agent.selector import AgentSkillSelector
from core.events.event_logger import EventLogger
from core.events.models import StreamEvent, StreamEventType
from core.llm.models import LLMMessage
from core.logging_config import get_logger

logger = get_logger(__name__)

MAX_TOOL_ROUNDS = 10


async def _needs_planning(user_input: str, llm_client) -> bool:
    """Check if user input needs planning using LLM judgment.
    
    Uses a lightweight LLM call to determine if the task requires multi-step planning.
    """
    prompt = f"""Analyze if this task requires multi-step planning.

Task: {user_input}

Answer with ONLY "yes" or "no".

Answer "yes" if the task:
- Has multiple distinct steps
- Requires sequential execution
- Involves complex dependencies
- Needs coordination between different actions

Answer "no" if the task:
- Is a simple query or question
- Can be done in one step
- Is just asking for information

Answer:"""

    try:
        response = llm_client.chat(
            messages=[{"role": "user", "content": prompt}],
            temperature=0.0,  # Deterministic
            max_tokens=10,
        )
        answer = response.strip().lower()
        return answer.startswith("yes")
    except Exception as e:
        logger.warning(f"Planning check failed: {e}, defaulting to no planning")
        return False


def _merge_tool_call_fragments(fragments: list[dict], new_fragments: list[dict]) -> list[dict]:
    """Merge tool call fragments from streaming responses.

    OpenAI streams tool_calls in fragments. This accumulates them.
    """
    merged = {fc["id"]: fc.copy() for fc in fragments}

    for new_fc in new_fragments:
        fc_id = new_fc["id"]
        if fc_id not in merged:
            merged[fc_id] = new_fc.copy()
        else:
            # Merge function arguments
            if "function" in new_fc and "arguments" in new_fc["function"]:
                if "function" in merged[fc_id]:
                    merged[fc_id]["function"]["arguments"] += new_fc["function"]["arguments"]
                else:
                    merged[fc_id]["function"] = new_fc["function"].copy()

    return list(merged.values())


class ChatLoop:
    """Manages the conversation loop with multi-turn tool use.

    Supports the full OpenAI function calling protocol:
    [user] → [assistant+tool_calls] → [tool results] → ... → [assistant final]
    """

    def __init__(
        self,
        selector: AgentSkillSelector,
        executor: AgentExecutor,
        llm_client,
        event_logger: EventLogger,
        context_manager,
        firewall,
    ):
        """Initialize ChatLoop.
        
        Args:
            selector: Skill selector
            executor: Skill executor
            llm_client: LLM client
            event_logger: Event logger
            context_manager: Context manager (required for snapshots)
            firewall: Hallucination firewall (required for verification)
        """
        self.selector = selector
        self.executor = executor
        self.llm = llm_client
        self.event_logger = event_logger
        self.context_manager = context_manager
        self.firewall = firewall

    async def run_step(
        self,
        user_input: str,
        session_id: str,
        user_id: str,
        context: dict[str, Any] | None = None,
        max_candidates: int = 5,
    ) -> str:
        """Run a full conversation step with multi-turn tool use.

        The LLM can call tools multiple times before producing a final answer.
        The complete message chain is preserved so the LLM retains its
        chain-of-thought across tool calls.
        
        Note: For complex tasks requiring planning, use run_step_stream() instead.
        """
        # 1. Log user query event
        user_event = self.event_logger.create_user_query(
            user_id=user_id,
            session_id=session_id,
            content=user_input,
        )

        # 2. Build context and save snapshot (always enabled)
        from core.context.manager import TaskType

        ctx = self.context_manager.build_context(
            session_id=session_id, query=user_input, task_type=TaskType.GENERAL
        )
        snapshot_id = self.context_manager.save_snapshot(ctx, session_id, user_event.event_id)
        logger.debug(f"Context snapshot: {snapshot_id}")

        # 3. Build messages with context
        messages = self._build_messages(user_input, context)

        # 4. Get available tools schema
        tools_schema = self.selector.selector.get_tools_schema(
            query=user_input, max_candidates=max_candidates
        )

        if not tools_schema:
            # No tools available — plain chat
            response = self.llm.chat(
                messages=[
                    LLMMessage(role=m["role"], content=m.get("content", "")) for m in messages
                ],
                user_id=user_id,
                session_id=session_id,
            )
            self._log_response(
                user_id,
                session_id,
                response.content or "",
                user_event.event_id,
                user_event.causal_chain_id,
            )
            return response.content or ""

        # 6. Multi-turn tool use loop
        for _round in range(MAX_TOOL_ROUNDS):
            llm_result = self.llm.chat_with_tools(
                messages=messages,
                tools=tools_schema,
                tool_choice="auto",
            )

            tool_calls = llm_result.get("tool_calls", [])

            if not tool_calls:
                # LLM produced a final text answer — verify and deliver
                final_content = llm_result.get("content", "")

                # Always verify with firewall
                verification = self.firewall.verify_response(final_content, snapshot_id, mode="warn")
                self.firewall.log_verification(session_id, user_event.event_id, verification)

                if not verification.safe_to_deliver:
                    logger.warning(
                        f"Firewall: confidence={verification.confidence_score:.2f}, "
                        f"failed={verification.claims_failed}"
                    )
                    final_content = (
                        f"{final_content}\n\n⚠️ Warning: Low confidence ({verification.confidence_score:.0%}). "
                        f"{verification.claims_failed} unverified claims."
                    )

                self._log_response(
                    user_id,
                    session_id,
                    final_content,
                    user_event.event_id,
                    user_event.causal_chain_id,
                )
                return final_content or ""

            # Append the assistant message (with tool_calls) to the chain
            assistant_msg: dict[str, Any] = {
                "role": "assistant",
                "content": llm_result.get("content") or "",
            }
            assistant_msg["tool_calls"] = tool_calls
            messages.append(assistant_msg)

            # Execute each tool and append results
            for tc in tool_calls:
                fn_name = tc["function"]["name"]
                raw_args = tc["function"]["arguments"]
                tc_id = tc.get("id", fn_name)

                # Log tool start
                self.event_logger.create_stream_event(
                    user_id=user_id,
                    session_id=session_id,
                    event_type="stream_tool_call_start",
                    content=json.dumps({"tool": fn_name, "call_id": tc_id}),
                    parent_event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                )

                params = json.loads(raw_args) if isinstance(raw_args, str) else raw_args

                try:
                    result = self.executor.execute_skill(
                        skill_name=fn_name,
                        params=params,
                        session_id=session_id,
                        parent_event_id=user_event.event_id,
                    )
                    result_str = (
                        json.dumps(result, default=str) if not isinstance(result, str) else result
                    )
                except Exception as e:
                    logger.error(f"Skill {fn_name} failed: {e}")
                    result_str = json.dumps({"error": str(e)})

                # Log tool result
                metadata = {
                    "call_id": tc_id,
                    "skill_result": result,  # Store structured result for Replay
                    "skill_name": fn_name,
                    "skill_params": params
                }
                
                self.event_logger.create_stream_event(
                    user_id=user_id,
                    session_id=session_id,
                    event_type="stream_tool_result",
                    content=json.dumps({"call_id": tc_id, "result": result_str[:500]}),
                    parent_event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                    metadata=metadata
                )

                # Append tool result in OpenAI protocol format
                messages.append(
                    {
                        "role": "tool",
                        "tool_call_id": tc_id,
                        "content": result_str,
                    }
                )

        # Exhausted rounds — ask LLM for a final answer without tools
        messages.append(
            {
                "role": "system",
                "content": "Please provide your final answer based on the tool results above.",
            }
        )
        response = self.llm.chat(
            messages=[LLMMessage(role=m["role"], content=m.get("content", "")) for m in messages],
            user_id=user_id,
            session_id=session_id,
        )
        self._log_response(
            user_id,
            session_id,
            response.content or "",
            user_event.event_id,
            user_event.causal_chain_id,
        )
        return response.content or ""

    async def run_step_stream(
        self,
        user_input: str,
        session_id: str,
        user_id: str,
        context: dict[str, Any] | None = None,
        max_candidates: int = 5,
    ) -> AsyncIterator[StreamEvent]:
        """Stream events as the agent processes a request.

        Yields StreamEvent objects for real-time output.
        """
        # 1. Log user query event
        user_event = self.event_logger.create_user_query(
            user_id=user_id,
            session_id=session_id,
            content=user_input,
        )

        # 2. Check if planning is needed
        if await _needs_planning(user_input, self.llm):
            # Use planning for complex tasks - stream events directly
            async for event in self.run_step_with_planning(
                user_input, session_id, user_id, context, max_candidates
            ):
                yield event
            return

        # 3. Build messages with context
        messages = self._build_messages(user_input, context)

        # 4. Get available tools schema
        tools_schema = self.selector.selector.get_tools_schema(
            query=user_input, max_candidates=max_candidates
        )

        # Log RUN_STARTED event
        run_started_event = self.event_logger.create_stream_event(
            user_id=user_id,
            session_id=session_id,
            event_type="stream_run_started",
            content=json.dumps({"query": user_input}),
            parent_event_id=user_event.event_id,
            causal_chain_id=user_event.causal_chain_id,
        )

        yield StreamEvent(
            event_type=StreamEventType.RUN_STARTED,
            data={"query": user_input},
            event_id=run_started_event.event_id,
            causal_chain_id=user_event.causal_chain_id,
        )

        if not tools_schema:
            # Plain chat — stream text
            async for chunk in self.llm.chat_stream(messages, user_id, session_id):
                text_event = self.event_logger.create_stream_event(
                    user_id=user_id,
                    session_id=session_id,
                    event_type="stream_text_delta",
                    content=json.dumps({"chunk": chunk}),
                    parent_event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                )
                yield StreamEvent(
                    event_type=StreamEventType.TEXT_DELTA,
                    data={"chunk": chunk},
                    event_id=text_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                )

            text_done_event = self.event_logger.create_stream_event(
                user_id=user_id,
                session_id=session_id,
                event_type="stream_text_done",
                content="{}",
                parent_event_id=user_event.event_id,
                causal_chain_id=user_event.causal_chain_id,
            )
            yield StreamEvent(
                event_type=StreamEventType.TEXT_DONE,
                data={},
                event_id=text_done_event.event_id,
                causal_chain_id=user_event.causal_chain_id,
            )

            run_finished_event = self.event_logger.create_stream_event(
                user_id=user_id,
                session_id=session_id,
                event_type="stream_run_finished",
                content="{}",
                parent_event_id=user_event.event_id,
                causal_chain_id=user_event.causal_chain_id,
            )
            yield StreamEvent(
                event_type=StreamEventType.RUN_FINISHED,
                data={},
                event_id=run_finished_event.event_id,
                causal_chain_id=user_event.causal_chain_id,
            )
            return

        # Multi-turn tool use loop with streaming
        for _round in range(MAX_TOOL_ROUNDS):
            full_text = ""
            tool_calls: list[dict] = []

            async for chunk in self.llm.chat_with_tools_stream(messages, tools_schema):
                if chunk["type"] == "text":
                    full_text += chunk["content"]
                    text_event = self.event_logger.create_stream_event(
                        user_id=user_id,
                        session_id=session_id,
                        event_type="stream_text_delta",
                        content=json.dumps({"chunk": chunk["content"]}),
                        parent_event_id=user_event.event_id,
                        causal_chain_id=user_event.causal_chain_id,
                    )
                    yield StreamEvent(
                        event_type=StreamEventType.TEXT_DELTA,
                        data={"chunk": chunk["content"]},
                        event_id=text_event.event_id,
                        causal_chain_id=user_event.causal_chain_id,
                    )
                elif chunk["type"] == "tool_call":
                    # Accumulate tool calls (streamed in fragments)
                    tool_calls = _merge_tool_call_fragments(tool_calls, [chunk["data"]])

            if not tool_calls:
                text_done_event = self.event_logger.create_stream_event(
                    user_id=user_id,
                    session_id=session_id,
                    event_type="stream_text_done",
                    content=json.dumps({"full_text": full_text}),
                    parent_event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                )
                yield StreamEvent(
                    event_type=StreamEventType.TEXT_DONE,
                    data={"full_text": full_text},
                    event_id=text_done_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                )
                self._log_response(
                    user_id, session_id, full_text, user_event.event_id, user_event.causal_chain_id
                )

                run_finished_event = self.event_logger.create_stream_event(
                    user_id=user_id,
                    session_id=session_id,
                    event_type="stream_run_finished",
                    content="{}",
                    parent_event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                )
                yield StreamEvent(
                    event_type=StreamEventType.RUN_FINISHED,
                    data={},
                    event_id=run_finished_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                )
                return

            # Execute tools
            messages.append({"role": "assistant", "content": full_text, "tool_calls": tool_calls})
            for tc in tool_calls:
                fn_name = tc["function"]["name"]
                tool_start_event = self.event_logger.create_stream_event(
                    user_id=user_id,
                    session_id=session_id,
                    event_type="stream_tool_call_start",
                    content=json.dumps({"tool": fn_name, "call_id": tc["id"]}),
                    parent_event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                )
                yield StreamEvent(
                    event_type=StreamEventType.TOOL_CALL_START,
                    data={"tool": fn_name, "call_id": tc["id"]},
                    event_id=tool_start_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                )

                # Handle delegation skill specially for multi-agent
                if fn_name == "delegate_task":
                    params = json.loads(tc["function"]["arguments"])
                    agent_delegated_event = self.event_logger.create_stream_event(
                        user_id=user_id,
                        session_id=session_id,
                        event_type="stream_agent_delegated",
                        content=json.dumps({"agent": params.get("agent_id", "unknown")}),
                        parent_event_id=user_event.event_id,
                        causal_chain_id=user_event.causal_chain_id,
                    )
                    yield StreamEvent(
                        event_type=StreamEventType.AGENT_DELEGATED,
                        data={"agent": params.get("agent_id", "unknown")},
                        event_id=agent_delegated_event.event_id,
                        causal_chain_id=user_event.causal_chain_id,
                    )
                    # Execute delegation and stream results
                    result = self.executor.execute_skill(
                        skill_name=fn_name,
                        params=params,
                        session_id=session_id,
                        parent_event_id=user_event.event_id,
                    )
                    # For delegation, the result is a string from the delegated agent
                    # In a full implementation, we would stream the delegated agent's events
                    result_str = str(result) if not isinstance(result, str) else result
                else:
                    result = self.executor.execute_skill(
                        skill_name=fn_name,
                        params=json.loads(tc["function"]["arguments"]),
                        session_id=session_id,
                        parent_event_id=user_event.event_id,
                    )
                    result_str = (
                        json.dumps(result, default=str) if not isinstance(result, str) else result
                    )

                tool_result_event = self.event_logger.create_stream_event(
                    user_id=user_id,
                    session_id=session_id,
                    event_type="stream_tool_result",
                    content=json.dumps({"call_id": tc["id"], "result": result_str[:500]}),
                    parent_event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                )
                yield StreamEvent(
                    event_type=StreamEventType.TOOL_RESULT,
                    data={"call_id": tc["id"], "result": result_str[:500]},
                    event_id=tool_result_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                )
                messages.append({"role": "tool", "tool_call_id": tc["id"], "content": result_str})

        # Exhausted rounds — ask LLM for a final answer without tools
        messages.append(
            {
                "role": "system",
                "content": "Please provide your final answer based on the tool results above.",
            }
        )
        full_text = ""
        async for chunk in self.llm.chat_stream(messages, user_id, session_id):
            full_text += chunk
            yield StreamEvent(
                event_type=StreamEventType.TEXT_DELTA,
                data={"chunk": chunk},
                event_id=user_event.event_id,
                causal_chain_id=user_event.causal_chain_id,
            )
        self._log_response(
            user_id, session_id, full_text, user_event.event_id, user_event.causal_chain_id
        )
        yield StreamEvent(
            event_type=StreamEventType.RUN_FINISHED,
            data={},
            event_id=user_event.event_id,
            causal_chain_id=user_event.causal_chain_id,
        )

    async def run_step_with_planning(
        self,
        user_input: str,
        session_id: str,
        user_id: str,
        context: dict[str, Any] | None = None,
        max_candidates: int = 5,
    ) -> AsyncIterator[StreamEvent]:
        """PAOR: Plan → Act → Observe → Reflect loop.

        For complex tasks that need multi-step planning.
        """
        planner = Planner(self.llm)
        constraints = planner.constraints

        # P: Plan
        plan = await planner.create_plan(goal=user_input, context=str(context))

        # Check constraints
        is_valid, error_msg = planner.check_constraints(plan)
        if not is_valid:
            yield StreamEvent(
                event_type=StreamEventType.RUN_ERROR,
                data={"error": error_msg},
            )
            return

        # Log plan created event
        self.event_logger.create_stream_event(
            user_id=user_id,
            session_id=session_id,
            event_type="stream_plan_created",
            content=plan.model_dump_json(),
        )

        yield StreamEvent(
            event_type=StreamEventType.PLAN_CREATED,
            data={"plan": plan.model_dump()},
        )

        max_revisions = self.llm.config.get("max_revisions", 3)
        for _rev in range(max_revisions):
            step_results = []

            # A: Act — execute ready steps
            next_steps = planner.get_next_steps(plan)
            if not next_steps:
                # All steps completed
                break

            # Check step count constraint
            if len(plan.steps) > constraints.max_steps:
                yield StreamEvent(
                    event_type=StreamEventType.RUN_ERROR,
                    data={
                        "error": f"Step count {len(plan.steps)} exceeds max {constraints.max_steps}"
                    },
                )
                return

            for step in next_steps:
                step.status = "in_progress"  # type: ignore
                yield StreamEvent(
                    event_type=StreamEventType.PLAN_STEP_START,
                    data={"step": step.step_id},
                )

                # Execute step using existing skill execution
                skill_name = step.skill_hint
                if skill_name:
                    result = self.executor.execute_skill(
                        skill_name=skill_name,
                        params={"input": step.description},
                        session_id=session_id,
                        parent_event_id=None,
                    )
                else:
                    # Use plain chat for step execution
                    result = "Step executed"

                step.status = "completed"  # type: ignore
                step.result = str(result)
                step_results.append({"step_id": step.step_id, "result": result})

                yield StreamEvent(
                    event_type=StreamEventType.PLAN_STEP_DONE,
                    data={"step": step.step_id, "result": str(result)},
                )

            # O: Observe — check if all done
            all_completed = all(s.status == "completed" for s in plan.steps)
            if all_completed:
                break

            # R: Reflect — should we revise?
            _assessment, revised_plan = await planner.reflect(plan, step_results)
            if revised_plan is not None:
                plan = revised_plan
                yield StreamEvent(
                    event_type=StreamEventType.PLAN_REVISED,
                    data={"plan": plan.model_dump()},
                )

        # Final synthesis
        yield StreamEvent(
            event_type=StreamEventType.TEXT_DELTA,
            data={"chunk": "Planning complete. Executing final synthesis..."},
        )
        yield StreamEvent(
            event_type=StreamEventType.RUN_FINISHED,
            data={},
        )

    # ------------------------------------------------------------------
    # Helpers
    # ------------------------------------------------------------------

    def _build_messages(
        self, user_input: str, context: dict[str, Any] | None
    ) -> list[dict[str, Any]]:
        """Build the initial messages list, injecting context if available."""
        messages: list[dict[str, Any]] = []

        system_parts = [
            "You are a development assistant. Use the available tools to help the user."
        ]

        if context:
            if context.get("system_prompt"):
                system_parts = [context["system_prompt"]]
            if context.get("selected_events"):
                history_lines = []
                for ev in context["selected_events"]:
                    role = "User" if ev.get("event_type") == "user_query" else "Agent"
                    history_lines.append(f"{role}: {ev.get('content', '')}")
                if history_lines:
                    system_parts.append("Recent conversation:\n" + "\n".join(history_lines))

        messages.append({"role": "system", "content": "\n\n".join(system_parts)})
        messages.append({"role": "user", "content": user_input})
        return messages

    def _log_response(
        self,
        user_id: str,
        session_id: str,
        content: str,
        parent_event_id: str,
        causal_chain_id: str | None,
    ) -> None:
        """Log the final agent response as an event."""
        self.event_logger.create_llm_response(
            user_id=user_id,
            session_id=session_id,
            content=content,
            agent_id="dev-agent",
            agent_version="0.1.0",
            parent_event_id=parent_event_id,
            causal_chain_id=causal_chain_id or "",
            llm_model_used=self.llm.config.get("model", "unknown"),
        )
