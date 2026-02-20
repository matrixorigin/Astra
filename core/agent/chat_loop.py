"""Chat loop with multi-turn tool use and full message chain."""

import json
import time
from collections.abc import AsyncIterator
from typing import Any

from core.agent.executor import AgentExecutor
from core.agent.planner import Planner
from core.events.event_logger import EventLogger
from core.skills.pipeline import SkillPipeline
from core.skills.learning_signals import SignalType
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
        selector: SkillPipeline,
        executor: AgentExecutor,
        llm_client,
        event_logger: EventLogger,
        context_manager,
        firewall,
        agent_id: str = "dev-agent",
    ):
        """Initialize ChatLoop.
        
        Args:
            selector: Unified skill pipeline
            executor: Skill executor
            llm_client: LLM client
            event_logger: Event logger
            context_manager: Context manager (required for snapshots)
            firewall: Hallucination firewall (required for verification)
            agent_id: ID of the agent running this loop (for multi-agent)
        """
        self.selector = selector
        self._pipeline = selector
        self.executor = executor
        self.llm = llm_client
        self.event_logger = event_logger
        self.context_manager = context_manager
        self.firewall = firewall
        self.agent_id = agent_id

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

        # 2. Build context and save context snapshot (always enabled).
        #    This is a *business-level* snapshot of what the LLM sees (system prompt,
        #    selected events, skills, docs). NOT a MatrixOne database-level snapshot.
        from core.context.manager import TaskType

        ctx = self.context_manager.build_context(
            session_id=session_id, query=user_input, task_type=TaskType.GENERAL
        )
        context_capture_id = self.context_manager.save_snapshot(ctx, session_id, user_event.event_id)
        logger.debug(f"Context snapshot: {context_capture_id}")

        # 3. Build messages with context
        messages = self._build_messages(user_input, context)

        # 4. Get available tools schema (with audit + learning)
        _sel = self._pipeline.get_tools_schema(
            user_input, session_id, max_candidates=max_candidates,
        )
        tools_schema = _sel.tools
        self._last_selection_event_id = _sel.event_id

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
                verification = self.firewall.verify_response(final_content, context_capture_id, mode="warn")
                self.firewall.log_verification(session_id, user_event.event_id, verification, context_capture_id)

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

                # Execute skill with automatic feedback recording
                try:
                    result = self.executor.execute_skill_with_feedback(
                        skill_name=fn_name,
                        params=params,
                        session_id=session_id,
                        parent_event_id=user_event.event_id,
                        selection_event_id=self._last_selection_event_id,
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

        # 2. Build context and save context snapshot (same as non-stream path).
        #    This is a *business-level* snapshot of what the LLM sees (system prompt,
        #    selected events, skills, docs). NOT a MatrixOne database-level snapshot.
        from core.context.manager import TaskType

        ctx = self.context_manager.build_context(
            session_id=session_id, query=user_input, task_type=TaskType.GENERAL
        )
        context_capture_id = self.context_manager.save_snapshot(ctx, session_id, user_event.event_id)
        logger.debug(f"[stream] Context snapshot: {context_capture_id}")

        # 3. Check if planning is needed
        if await _needs_planning(user_input, self.llm):
            async for event in self.run_step_with_planning(
                user_input, session_id, user_id, context, max_candidates,
                context_capture_id=context_capture_id,
            ):
                yield event
            return

        # 4. Build messages with context
        messages = self._build_messages(user_input, context)

        # 5. Get available tools schema (with audit + learning)
        _sel = self._pipeline.get_tools_schema(
            user_input, session_id, max_candidates=max_candidates,
        )
        tools_schema = _sel.tools
        self._last_selection_event_id = _sel.event_id

        # Log RUN_STARTED event
        run_started_event = self.event_logger.create_stream_event(
            user_id=user_id,
            session_id=session_id,
            event_type="stream_run_started",
            content=json.dumps({"query": user_input, "context_capture_id": str(context_capture_id)}),
            parent_event_id=user_event.event_id,
            causal_chain_id=user_event.causal_chain_id,
        )

        yield StreamEvent(
            event_type=StreamEventType.RUN_STARTED,
            data={"query": user_input, "context_capture_id": str(context_capture_id)},
            event_id=run_started_event.event_id,
            causal_chain_id=user_event.causal_chain_id,
            agent_id=self.agent_id,
        )

        if not tools_schema:
            # Plain chat — stream text, accumulate for verification
            full_text = ""
            async for chunk in self.llm.chat_stream(messages, user_id, session_id):
                full_text += chunk
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
            agent_id=self.agent_id,
                )

            # Verify with firewall before delivering
            verification = self.firewall.verify_response(full_text, context_capture_id, mode="warn")
            self.firewall.log_verification(session_id, user_event.event_id, verification, context_capture_id)

            if not verification.safe_to_deliver:
                logger.warning(
                    f"[stream/plain] Firewall: confidence={verification.confidence_score:.2f}, "
                    f"failed={verification.claims_failed}"
                )
                warning = (
                    f"\n\n⚠️ Warning: Low confidence ({verification.confidence_score:.0%}). "
                    f"{verification.claims_failed} unverified claims."
                )
                yield StreamEvent(
                    event_type=StreamEventType.TEXT_DELTA,
                    data={"chunk": warning},
                    event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
            agent_id=self.agent_id,
                )
                full_text += warning

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
                data={"full_text": full_text, "context_capture_id": context_capture_id},
                event_id=text_done_event.event_id,
                causal_chain_id=user_event.causal_chain_id,
            agent_id=self.agent_id,
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
            agent_id=self.agent_id,
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
            agent_id=self.agent_id,
                    )
                elif chunk["type"] == "tool_call":
                    # Accumulate tool calls (streamed in fragments)
                    tool_calls = _merge_tool_call_fragments(tool_calls, [chunk["data"]])

            if not tool_calls:
                # Verify with firewall (same as non-stream path)
                verification = self.firewall.verify_response(full_text, context_capture_id, mode="warn")
                self.firewall.log_verification(session_id, user_event.event_id, verification, context_capture_id)

                if not verification.safe_to_deliver:
                    logger.warning(
                        f"[stream] Firewall: confidence={verification.confidence_score:.2f}, "
                        f"failed={verification.claims_failed}"
                    )
                    full_text += (
                        f"\n\n⚠️ Warning: Low confidence ({verification.confidence_score:.0%}). "
                        f"{verification.claims_failed} unverified claims."
                    )

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
                    data={"full_text": full_text, "context_capture_id": context_capture_id},
                    event_id=text_done_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
            agent_id=self.agent_id,
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
            agent_id=self.agent_id,
                )
                return

            # Execute tools
            messages.append({"role": "assistant", "content": full_text, "tool_calls": tool_calls})
            
            # Check for parallel delegation (multiple delegate_task calls)
            delegation_calls = [tc for tc in tool_calls if tc["function"]["name"] == "delegate_task"]
            
            if len(delegation_calls) > 1:
                # Parallel delegation: fan-out/fan-in
                from core.skills.delegation import DelegateTaskInput
                
                # Emit TOOL_CALL_START for all delegations
                for tc in delegation_calls:
                    tool_start_event = self.event_logger.create_stream_event(
                        user_id=user_id,
                        session_id=session_id,
                        event_type="stream_tool_call_start",
                        content=json.dumps({"tool": "delegate_task", "call_id": tc["id"]}),
                        parent_event_id=user_event.event_id,
                        causal_chain_id=user_event.causal_chain_id,
                    )
                    yield StreamEvent(
                        event_type=StreamEventType.TOOL_CALL_START,
                        data={"tool": "delegate_task", "call_id": tc["id"]},
                        event_id=tool_start_event.event_id,
                        causal_chain_id=user_event.causal_chain_id,
                        agent_id=self.agent_id,
                    )
                
                # Build inputs for parallel execution
                inputs = []
                for tc in delegation_calls:
                    params = json.loads(tc["function"]["arguments"])
                    inputs.append(DelegateTaskInput(
                        agent_id=params.get("agent_id", "unknown"),
                        task=params.get("task", ""),
                        context=params.get("context"),
                        session_id=session_id,
                        user_id=user_id,
                    ))
                
                # Execute parallel streaming
                skill = self.executor.skill_registry.get("delegate_task")
                results = {}  # call_id -> result_text
                agent_to_call = {}  # agent_id -> call_id mapping
                completed_agents = set()  # Track which agents have completed
                execution_times = {}  # call_id -> execution time
                
                # Build agent_id to call_id mapping
                for tc in delegation_calls:
                    params = json.loads(tc["function"]["arguments"])
                    agent_to_call[params.get("agent_id")] = tc["id"]
                
                _t0 = time.monotonic()
                try:
                    async for event in skill.execute_parallel_stream(inputs):
                        yield event
                        
                        # Collect results from TEXT_DONE events (only first one per agent)
                        if event.event_type == StreamEventType.TEXT_DONE:
                            agent_id = event.agent_id
                            call_id = agent_to_call.get(agent_id)
                            if call_id and call_id not in results:  # Only collect first TEXT_DONE
                                result_text = event.data.get("text", "")
                                results[call_id] = result_text
                        # Track completion
                        elif event.event_type == StreamEventType.AGENT_COMPLETED:
                            agent_id = event.agent_id
                            completed_agents.add(agent_id)
                            call_id = agent_to_call.get(agent_id)
                            # Fallback: mark completion even without TEXT_DONE
                            if call_id and call_id not in results:
                                results[call_id] = f"Agent '{agent_id}' completed with no text output"
                        # Track errors
                        elif event.event_type == StreamEventType.RUN_ERROR:
                            agent_id = event.agent_id
                            call_id = agent_to_call.get(agent_id)
                            if call_id and call_id not in results:  # Don't overwrite existing results
                                error_msg = event.data.get("error", "Unknown error")
                                results[call_id] = f"Error: {error_msg}"
                
                except Exception as e:
                    logger.error(f"Error in parallel delegation: {e}", exc_info=True)
                    # Mark all incomplete delegations as failed
                    for tc in delegation_calls:
                        if tc["id"] not in results:
                            results[tc["id"]] = f"Error: Parallel execution failed - {str(e)}"
                finally:
                    # Record feedback for parallel execution
                    _elapsed_ms = (time.monotonic() - _t0) * 1000
                    if self._last_selection_event_id:
                        self._pipeline.record_feedback(
                            self._last_selection_event_id,
                            SignalType.EXECUTION_TIME,
                            {"ms": _elapsed_ms, "skill": "delegate_task", "parallel": True, "count": len(delegation_calls)},
                        )
                
                # Emit TOOL_RESULT for each delegation
                for tc in delegation_calls:
                    result_str = results.get(tc["id"], "No result")
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
                        agent_id=self.agent_id,
                    )
                    messages.append({"role": "tool", "tool_call_id": tc["id"], "content": result_str})
            else:
                # Sequential tool execution (existing logic)
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
                        agent_id=self.agent_id,
                    )

                    # Handle delegation skill specially for multi-agent streaming
                    _t0 = time.monotonic()
                    try:
                        if fn_name == "delegate_task":
                            params = json.loads(tc["function"]["arguments"])
                            delegated_agent_id = params.get("agent_id", "unknown")
                            
                            # Stream delegated agent's events
                            result_text = ""
                            has_output = False
                            async for delegated_event in self.executor.execute_skill_stream(
                                skill_name=fn_name,
                                params=params,
                                session_id=session_id,
                                parent_event_id=user_event.event_id,
                            ):
                                # Forward delegated agent's events with agent_id tagged
                                yield delegated_event
                                
                                # Collect final result
                                if delegated_event.event_type == StreamEventType.TEXT_DONE:
                                    result_text = delegated_event.data.get("text", "")
                                    has_output = True
                            
                            # Use collected result or fallback message with agent_id
                            result_str = result_text if has_output else f"Agent '{delegated_agent_id}' completed with no text output"
                        else:
                            # Execute skill with automatic feedback recording
                            result = self.executor.execute_skill_with_feedback(
                                skill_name=fn_name,
                                params=json.loads(tc["function"]["arguments"]),
                                session_id=session_id,
                                parent_event_id=user_event.event_id,
                                selection_event_id=self._last_selection_event_id,
                            )
                            result_str = (
                                json.dumps(result, default=str) if not isinstance(result, str) else result
                            )
                    except Exception as e:
                        logger.error(f"Parallel tool {fn_name} failed: {e}")
                        result_str = json.dumps({"error": str(e)})
                    finally:
                        # Record feedback for delegate_task streaming (non-delegate skills handled by execute_skill_with_feedback)
                        if fn_name == "delegate_task" and self._last_selection_event_id:
                            _elapsed_ms = (time.monotonic() - _t0) * 1000
                            self._pipeline.record_feedback(
                                self._last_selection_event_id,
                                SignalType.EXECUTION_TIME,
                                {"ms": _elapsed_ms, "skill": fn_name},
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
                        agent_id=self.agent_id,
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
            agent_id=self.agent_id,
            )

        # Verify exhausted-rounds answer with firewall
        verification = self.firewall.verify_response(full_text, context_capture_id, mode="warn")
        self.firewall.log_verification(session_id, user_event.event_id, verification, context_capture_id)

        if not verification.safe_to_deliver:
            logger.warning(
                f"[stream/exhausted] Firewall: confidence={verification.confidence_score:.2f}, "
                f"failed={verification.claims_failed}"
            )
            warning = (
                f"\n\n⚠️ Warning: Low confidence ({verification.confidence_score:.0%}). "
                f"{verification.claims_failed} unverified claims."
            )
            yield StreamEvent(
                event_type=StreamEventType.TEXT_DELTA,
                data={"chunk": warning},
                event_id=user_event.event_id,
                causal_chain_id=user_event.causal_chain_id,
            agent_id=self.agent_id,
            )
            full_text += warning

        self._log_response(
            user_id, session_id, full_text, user_event.event_id, user_event.causal_chain_id
        )
        yield StreamEvent(
            event_type=StreamEventType.RUN_FINISHED,
            data={"context_capture_id": context_capture_id},
            event_id=user_event.event_id,
            causal_chain_id=user_event.causal_chain_id,
            agent_id=self.agent_id,
        )

    async def run_step_with_planning(
        self,
        user_input: str,
        session_id: str,
        user_id: str,
        context: dict[str, Any] | None = None,
        max_candidates: int = 5,
        context_capture_id: str | None = None,
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

                # Execute step through SkillPipeline to enable learning
                skill_name = step.skill_hint
                
                if skill_name:
                    # Get tools schema through pipeline (includes selector/auditor/validator)
                    _sel = self._pipeline.get_tools_schema(
                        query=step.description,
                        session_id=session_id,
                        max_candidates=max_candidates,
                    )
                    tools_schema = _sel.tools
                    selection_event_id = _sel.event_id
                    
                    # Find the tool in schema
                    tool_found = any(t["function"]["name"] == skill_name for t in tools_schema)
                    if not tool_found and tools_schema:
                        # Fallback: use selector's top recommendation
                        skill_name = tools_schema[0]["function"]["name"]
                        logger.info(
                            f"Skill hint '{step.skill_hint}' not in candidates, "
                            f"using selector recommendation: {skill_name}"
                        )
                    
                    if tool_found or tools_schema:
                        # Execute skill with automatic feedback recording
                        result = self.executor.execute_skill_with_feedback(
                            skill_name=skill_name,
                            params={"input": step.description},
                            session_id=session_id,
                            parent_event_id=None,
                            selection_event_id=selection_event_id,
                            extra_feedback_data={"planning_step": step.step_id},
                        )
                    else:
                        result = f"No suitable skill available for: {step.description}"
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
