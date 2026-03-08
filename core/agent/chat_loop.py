"""Chat loop with multi-turn tool use and full message chain."""

import asyncio
import json
import time
from collections.abc import AsyncIterator
from typing import Any

from core.agent.executor import AgentExecutor
from core.agent.planner import Planner, PlanStatus, restore_plan_from_events
from core.events.event_logger import EventLogger
from core.events.models import StreamEvent, StreamEventType
from core.llm.models import LLMMessage
from core.logging_config import get_logger
from core.skills.prefilter import ConversationState

logger = get_logger(__name__)

_UNSET = object()  # Sentinel for "not yet checked"

MAX_TOOL_ROUNDS = 10

# Hard ceiling on any single tool result in the message chain (~3K tokens).
# Acts as a safety net when mo-trustmem and budget tracker are both unavailable.
MAX_SINGLE_TOOL_RESULT_CHARS = 12000
TOOL_TIMEOUT_SECONDS = 120

# Scratchpad tool schemas — injected alongside skill tools when scratchpad is enabled
_SCRATCHPAD_TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "scratchpad_write",
            "description": "Write a note to working memory. Use for plans, hypotheses, findings, todos, decisions.",
            "parameters": {
                "type": "object",
                "properties": {
                    "note_type": {"type": "string", "enum": ["plan", "hypothesis", "finding", "todo", "decision"]},
                    "content": {"type": "string", "description": "Note content"},
                },
                "required": ["note_type", "content"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "scratchpad_read",
            "description": "Read active notes from working memory.",
            "parameters": {
                "type": "object",
                "properties": {
                    "note_type": {"type": "string", "enum": ["plan", "hypothesis", "finding", "todo", "decision"], "description": "Filter by type (optional)"},
                },
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "scratchpad_close",
            "description": "Close/complete a note in working memory.",
            "parameters": {
                "type": "object",
                "properties": {
                    "note_id": {"type": "string"},
                    "status": {"type": "string", "enum": ["completed", "superseded"], "default": "completed"},
                },
                "required": ["note_id"],
            },
        },
    },
]



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
        selector,  # ToolRegistry instance
        executor: AgentExecutor,
        llm_client,
        event_logger: EventLogger,
        context_manager,
        firewall,
        agent_id: str = "dev-agent",
        scratchpad=None,
        firewall_mode: str = "warn",
        hitl_policy=None,
    ):
        """Initialize ChatLoop.
        
        Args:
            selector: ToolRegistry for tool selection
            executor: Skill executor
            llm_client: LLM client
            event_logger: Event logger
            context_manager: Context manager (required for snapshots)
            firewall: Hallucination firewall (required for verification)
            agent_id: ID of the agent running this loop (for multi-agent)
            scratchpad: AgentScratchpad instance for working memory (optional)
            firewall_mode: 'warn' (annotate) or 'block' (fail-closed). Default: 'warn'.
            hitl_policy: HITLPolicyEngine instance for human-in-the-loop supervision (optional)
        """
        self.selector = selector
        self._tool_registry = selector
        self.executor = executor
        self.llm = llm_client
        self.event_logger = event_logger
        self.context_manager = context_manager
        self.firewall = firewall
        self.firewall_mode = firewall_mode if firewall_mode in ("warn", "block") else "warn"
        self.agent_id = agent_id
        self.scratchpad = scratchpad
        self.hitl_policy = hitl_policy
        self.observer = None  # Set via set_observer()
        self.mcp_bridge = None  # Set via set_mcp_bridge()
        self._few_shot = None  # Initialized lazily on first use
        self._escalated_model: str | None | object = _UNSET  # SLO escalation cache
        self._memory_service: Any = None  # MemoryService facade
        self._budget_manager = None  # Global context budget manager
        # Circuit breaker state (per-turn, reset on each run_step call)
        self._tool_failures: dict[str, list[str]] = {}
        self._blocked_tools: set[str] = set()
        # Stall detector: tracks per-round tool call signatures to detect
        # repeated unsuccessful search patterns (e.g. same query rephrased).
        self._round_tool_sigs: list[set[str]] = []
        self._round_text_lens: list[int] = []
        try:
            from core.context.few_shot import FewShotRetriever
            if hasattr(llm_client, '_db_factory'):
                self._few_shot = FewShotRetriever(llm_client._db_factory)
        except Exception:
            pass
        # Initialize memory service for tool output handling
        try:
            from core.memory.tabular.service import MemoryService
            if hasattr(event_logger, '_db_factory'):
                self._memory_service = MemoryService(event_logger._db_factory)
        except Exception:
            pass
        # Initialize global context budget manager
        try:
            from core.context.budget_manager import ContextBudgetManager
            max_tokens = llm_client.config.get("max_context_tokens", 128000) if hasattr(llm_client, 'config') else 128000
            self._budget_manager = ContextBudgetManager(max_context_tokens=max_tokens)
        except Exception:
            pass

    def _extract_params_from_query(self, query: str) -> dict[str, Any]:
        """Extract common parameters from query using simple patterns.
        
        Extracts:
        - repo: "owner/repo" or bare project name
        - limit/count: numeric values
        - state: open/closed/all
        """
        import re
        params: dict[str, Any] = {}
        q = query.lower()
        
        # Extract repo: "owner/repo" pattern
        repo_match = re.search(r'\b([a-zA-Z0-9_-]+/[a-zA-Z0-9_.-]+)\b', query)
        if repo_match:
            params["repo"] = repo_match.group(1)
        else:
            # Bare project name after "for", "in", "of"
            bare_match = re.search(r'\b(?:for|in|of)\s+([a-zA-Z0-9_-]+)\b', q)
            if bare_match:
                params["repo"] = bare_match.group(1)
        
        # Extract limit/count
        limit_match = re.search(r'\b(?:top|last|recent|limit)\s*(\d+)\b', q)
        if limit_match:
            params["limit"] = int(limit_match.group(1))
        
        # Extract state
        if "closed" in q:
            params["state"] = "closed"
        elif "all" in q and ("issue" in q or "pr" in q):
            params["state"] = "all"
        
        return params

    def _merge_context(self, ctx, context: dict[str, Any] | None) -> dict[str, Any]:
        """Merge ContextManager output into request context.

        ContextManager.build_context() returns a Context dataclass with fields like
        system_prompt, selected_events, code_context, documentation. This method
        merges those into the request context dict so _build_messages() can use them.

        Args:
            ctx: Context dataclass from ContextManager.build_context()
            context: Optional request-level context dict

        Returns:
            Merged context dict with all fields available for _build_messages()
        """
        merged = dict(context or {})
        # ctx may be a Context dataclass or a dict (in tests); use getattr for safety.
        # setdefault: request-level context takes priority over ContextManager output.
        merged.setdefault("system_prompt", getattr(ctx, "system_prompt", None))
        merged.setdefault("selected_events", getattr(ctx, "selected_events", None))
        merged.setdefault("token_budget", getattr(ctx, "token_budget", None))
        if getattr(ctx, "code_context", None):
            merged.setdefault("code_context", ctx.code_context)
        if getattr(ctx, "documentation", None):
            merged.setdefault("documentation", ctx.documentation)
        return merged

    async def run_step(
        self,
        user_input: str,
        session_id: str,
        user_id: str,
        context: dict[str, Any] | None = None,
        max_candidates: int = 5,
    ) -> str:
        """Run a full conversation step with multi-turn tool use.

        This is a convenience wrapper around run_step_stream() that collects
        all events and returns the final text content.

        The LLM can call tools multiple times before producing a final answer.
        The complete message chain is preserved so the LLM retains its
        chain-of-thought across tool calls.
        """
        final_content = ""
        async for event in self.run_step_stream(
            user_input=user_input,
            session_id=session_id,
            user_id=user_id,
            context=context,
            max_candidates=max_candidates,
        ):
            # Collect final text from TEXT_DONE or RUN_FINISHED events
            if event.event_type == StreamEventType.TEXT_DONE:
                final_content = event.data.get("full_text", final_content)
            elif event.event_type == StreamEventType.RUN_FINISHED:
                # RUN_FINISHED may contain final content if TEXT_DONE wasn't emitted
                if not final_content and "content" in event.data:
                    final_content = event.data["content"]
        return final_content

    async def _execute_single_tool(
        self,
        tc: dict,
        fn_name: str,
        user_id: str,
        session_id: str,
        user_input: str,
        full_text: str,
        user_event: Any,
        messages: list[dict],
    ) -> AsyncIterator[StreamEvent]:
        """Execute a single tool call — extracted to share between parallel-delegation and sequential paths."""
        # Circuit breaker check
        if self._is_tool_blocked(fn_name):
            result_str = json.dumps({"error": f"Tool {fn_name} is blocked by circuit breaker"})
            yield StreamEvent(
                event_type=StreamEventType.TOOL_RESULT,
                data={"call_id": tc["id"], "result": result_str, "blocked": True},
                event_id=user_event.event_id,
                causal_chain_id=user_event.causal_chain_id,
                agent_id=self.agent_id,
            )
            messages.append({"role": "tool", "tool_call_id": tc["id"], "content": result_str})
            return

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

        _t0 = time.monotonic()
        result_str = ""
        try:
            params = json.loads(tc["function"]["arguments"])

            from core.agent.async_tools import get_async_tool_registry
            _async_registry = get_async_tool_registry()

            from core.verification.cot_audit import audit_tool_call
            audit = audit_tool_call(
                user_query=user_input,
                tool_name=fn_name,
                tool_args=params,
                assistant_reasoning=full_text,
                llm_client=self.llm,
            )
            if not audit.safe:
                logger.warning("CoT audit blocked tool %s: %s", fn_name, audit.reason)
                result_str = json.dumps({"error": f"Blocked by CoT audit: {audit.reason}"})
            else:
                hitl_ok, hitl_msg = self._evaluate_hitl(fn_name, params)
                if not hitl_ok:
                    result_str = hitl_msg
                elif _async_registry.is_async_tool(fn_name):
                    result = await _async_registry.execute(fn_name, params, run_id=getattr(self, '_current_run_id', None))
                    result_str = json.dumps(result, default=str)
                    if self.hitl_policy:
                        self.hitl_policy.record_outcome(fn_name, success=True)
                    if result.get("wait_for"):
                        yield StreamEvent(
                            event_type=StreamEventType.TOOL_RESULT,
                            data={"call_id": tc["id"], "result": result_str[:500], "wait_for": result["wait_for"]},
                            event_id=user_event.event_id,
                            causal_chain_id=user_event.causal_chain_id,
                            agent_id=self.agent_id,
                        )
                        messages.append({"role": "tool", "tool_call_id": tc["id"], "content": result_str})
                        return
                elif fn_name == "delegate_task":
                    delegated_agent_id = params.get("agent_id", "unknown")
                    result_text = ""
                    has_output = False
                    async for delegated_event in self.executor.execute_skill_stream(
                        skill_name=fn_name,
                        params=params,
                        session_id=session_id,
                        parent_event_id=user_event.event_id,
                    ):
                        yield delegated_event
                        if delegated_event.event_type == StreamEventType.TEXT_DONE:
                            result_text = delegated_event.data.get("full_text", "")
                            has_output = True
                    result_str = result_text if has_output else f"Agent '{delegated_agent_id}' completed with no text output"
                    if self.hitl_policy:
                        self.hitl_policy.record_outcome(fn_name, success=True)
                else:
                    if fn_name.startswith("scratchpad_") and self.scratchpad:
                        result = self._handle_scratchpad_tool(fn_name, params, session_id, user_id)
                    elif self.mcp_bridge and self.mcp_bridge.is_mcp_tool(fn_name):
                        result = await asyncio.wait_for(
                            self.mcp_bridge.call_tool(fn_name, params), timeout=TOOL_TIMEOUT_SECONDS,
                        )
                    else:
                        result = await asyncio.wait_for(
                            asyncio.to_thread(
                                self.executor.execute_skill_with_feedback,
                                skill_name=fn_name,
                                params=params,
                                session_id=session_id,
                                parent_event_id=user_event.event_id,
                                selection_event_id=None,
                            ),
                            timeout=TOOL_TIMEOUT_SECONDS,
                        )
                    result_str = json.dumps(result, default=str) if not isinstance(result, str) else result
                    self._record_tool_success(fn_name)
                    if self.hitl_policy:
                        self.hitl_policy.record_outcome(fn_name, success=True)
        except Exception as e:
            logger.error(f"Tool {fn_name} failed: {e}")
            result_str = json.dumps({"error": str(e)})
            self._record_tool_failure(fn_name, str(e))
            if self.hitl_policy:
                self.hitl_policy.record_outcome(fn_name, success=False)
        finally:
            pass

        _tool_elapsed_ms = (time.monotonic() - _t0) * 1000
        _result_size_bytes = len(result_str.encode("utf-8", errors="replace")) if result_str else 0
        _result_size_tokens = len(result_str.split()) if result_str else 0  # rough estimate

        tool_result_event = self.event_logger.create_stream_event(
            user_id=user_id,
            session_id=session_id,
            event_type="stream_tool_result",
            content=json.dumps({"call_id": tc["id"], "result": result_str[:500]}),
            parent_event_id=user_event.event_id,
            causal_chain_id=user_event.causal_chain_id,
            metadata={
                "duration_ms": _tool_elapsed_ms,
                "result_size_bytes": _result_size_bytes,
                "result_size_tokens": _result_size_tokens,
            },
        )
        yield StreamEvent(
            event_type=StreamEventType.TOOL_RESULT,
            data={"call_id": tc["id"], "result": result_str[:500]},
            event_id=tool_result_event.event_id,
            causal_chain_id=user_event.causal_chain_id,
            agent_id=self.agent_id,
        )
        # Process tool output unconditionally: large results → summarize (+ store if memory available)
        from core.agent.tool_output_handler import process_tool_output
        if not hasattr(self, '_turn_budget') or self._turn_budget is None:
            from core.context.budget_manager import TurnBudgetTracker
            self._turn_budget = TurnBudgetTracker(max_tool_output_tokens=30000)

        remaining = self._turn_budget.remaining
        result_str = process_tool_output(
            output=result_str,
            tool_name=fn_name,
            session_id=session_id,
            user_id=user_id,
            memory_service=getattr(self, '_memory_service', None),
            turn_event_id=user_event.event_id,
            remaining_tokens=remaining,
        )
        self._turn_budget.record(len(result_str))
        messages.append({"role": "tool", "tool_call_id": tc["id"], "content": result_str})

        # If skill returned success=False, inject a hard stop to prevent LLM from
        # retrying with different params or using bash/curl to work around the failure.
        try:
            _parsed = json.loads(result_str) if isinstance(result_str, str) else result
            if isinstance(_parsed, dict):
                if _parsed.get("success") is False:
                    messages.append({
                        "role": "system",
                        "content": (
                            "The skill returned success=False. "
                            "STOP. Do NOT call any more tools. Do NOT retry with different parameters. "
                            "Do NOT use bash, curl, grep, or any other tool to work around this. "
                            "Report the error directly to the user and ask them to clarify."
                        ),
                    })
                # Skill-provided authoritative guidance — injected as system
                # message so the LLM treats it as a directive, not a suggestion.
                # Also emit user_message as a text event so the CLI can display
                # it directly if the LLM returns empty after seeing the guidance.
                elif _parsed.get("guidance"):
                    messages.append({
                        "role": "system",
                        "content": _parsed["guidance"],
                    })
                    _user_msg = _parsed.get("user_message")
                    if _user_msg:
                        yield StreamEvent(
                            event_type=StreamEventType.TEXT_DELTA,
                            data={"chunk": f"\n\n{_user_msg}"},
                            event_id=tool_result_event.event_id,
                            causal_chain_id=user_event.causal_chain_id,
                            agent_id=self.agent_id,
                        )
        except Exception:
            pass

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
        # Set user context on executor for framework field injection
        self.executor._current_user_id = user_id

        # 1. Log user query event
        user_event = self.event_logger.create_user_query(
            user_id=user_id,
            session_id=session_id,
            content=user_input,
        )
        self.event_logger.flush_critical()  # Must be visible for build_context

        # 2. Build context and save context snapshot (same as non-stream path).
        #    This is a *business-level* snapshot of what the LLM sees (system prompt,
        #    selected events, skills, docs). NOT a MatrixOne database-level snapshot.

        ctx = self.context_manager.build_context(
            session_id=session_id, query=user_input,
        )
        context_capture_id = self.context_manager.save_snapshot(ctx, session_id, user_event.event_id)
        logger.debug(f"[stream] Context snapshot: {context_capture_id}")

        # 3. Planning: only when explicitly requested via context.
        if (context or {}).get("planning"):
            async for event in self.run_step_with_planning(
                user_input, session_id, user_id, context, max_candidates,
                context_capture_id=context_capture_id,
                parent_user_event=user_event,
            ):
                yield event
            return

        # 4. Build messages with context
        merged_ctx = self._merge_context(ctx, context)
        messages = self._build_messages(user_input, merged_ctx, session_id=session_id, user_id=user_id)

        # 4.5. Extract parameters from query and add as hint
        extracted_params = self._extract_params_from_query(user_input)
        if extracted_params:
            hint = "Extracted from query: " + ", ".join(f"{k}={v}" for k, v in extracted_params.items())
            # Append hint to user message
            if messages and messages[-1].get("role") == "user":
                messages[-1]["content"] += f"\n\n[{hint}]"
            logger.debug("Parameter extraction: %s", extracted_params)

        # 5. Get available tools schema
        tools_schema = self._tool_registry.select(user_input, messages)

        if self.scratchpad:
            tools_schema = list(tools_schema) + _SCRATCHPAD_TOOLS

        # Append MCP tools when bridge is connected
        if self.mcp_bridge and self.mcp_bridge.tool_count > 0:
            mcp_tools = await self.mcp_bridge.get_tools_schema()
            tools_schema = list(tools_schema) + mcp_tools

        # Append async tools (submit_job, etc.) — always available
        from core.agent.async_tools import get_async_tool_registry
        _async_registry = get_async_tool_registry()
        tools_schema = list(tools_schema) + _async_registry.get_schemas()

        # Filter tools by agent's allowed_tools (for child agent isolation)
        allowed = (context or {}).get("allowed_tools")
        if allowed:
            allowed_set = set(allowed)
            tools_schema = [t for t in tools_schema
                           if t.get("function", {}).get("name") in allowed_set]

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
            # Plain chat — stream text with sentence-level verification
            from core.verification.streaming_verifier import StreamingVerifier
            sv = StreamingVerifier(firewall=self.firewall, context_capture_id=context_capture_id, llm_client=self.llm)

            # Use model from context if provided, otherwise use SLO escalation
            from core.llm.model_resolver import resolve_model
            model = resolve_model(
                request_model=(context or {}).get("model"),
                slo_escalation_model=self._check_slo_escalation(session_id),
            )

            async for chunk_msg in self.llm.chat_stream(
                messages, user_id, session_id, model=model,
            ):
                if chunk_msg["type"] == "reasoning":
                    # Emit reasoning event for CoT audit trail
                    yield StreamEvent(
                        event_type=StreamEventType.REASONING_MESSAGE_CONTENT,
                        data={"content": chunk_msg["content"]},
                        event_id=user_event.event_id,
                        causal_chain_id=user_event.causal_chain_id,
                        agent_id=self.agent_id,
                    )
                    continue
                chunk = chunk_msg["content"]
                warnings = sv.check(chunk)
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
                for warning in warnings:
                    sv.full_text += warning
                    yield StreamEvent(
                        event_type=StreamEventType.TEXT_DELTA,
                        data={"chunk": warning},
                        event_id=text_event.event_id,
                        causal_chain_id=user_event.causal_chain_id,
                        agent_id=self.agent_id,
                    )

            # Flush remaining buffer + pending sentences
            for warning in sv.flush():
                sv.full_text += warning
                yield StreamEvent(
                    event_type=StreamEventType.TEXT_DELTA,
                    data={"chunk": warning},
                    event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                    agent_id=self.agent_id,
                )

            full_text = sv.full_text

            # Post-stream: full response-level verification for audit record
            verification = self.firewall.verify_response(full_text, context_capture_id, mode=self.firewall_mode)
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
                user_id, session_id, full_text, user_event.event_id, user_event.causal_chain_id,
                messages=messages, firewall_result=verification,
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
        last_skill_name: str | None = None
        # Reset turn budget tracker and circuit breaker for this turn
        self._turn_budget = None
        self._reset_breaker()

        # Unified intent routing — single pass for tool filtering + task type + intent
        # Edge path uses Tier 0 only (<1ms, no LLM call) for tool filtering and task type.
        # The full Tier 0→1 cascade runs in the cloud path (api/routers/chat.py).
        from core.context.intent_routing import Tier0Engine, ToolFilter, LOCAL_TOOLS, RoutingDecision, RoutingResult, INTENT_PLANS, _FALLBACK_PLAN
        # Tier0Engine is stateless — reuse a single instance across calls
        if not hasattr(self, '_tier0'):
            self._tier0 = Tier0Engine()
        _tier0 = self._tier0
        _tool_filter, _max_rounds = _tier0.classify_tool_filter(user_input)
        _task_type = _tier0.classify_task_type(user_input)
        _tier0_result = _tier0.classify(user_input)
        _routing = RoutingDecision(
            plan=INTENT_PLANS.get(_tier0_result.intent, _FALLBACK_PLAN) if _tier0_result.intent else _FALLBACK_PLAN,
            routing_result=_tier0_result,
            tool_filter=_tool_filter,
            max_tool_rounds=_max_rounds,
            task_type=_task_type,
        )

        _effective_max_rounds = _routing.max_tool_rounds
        # Filter tools by routing decision. This runs in the streaming path
        # (run_step_stream). The pipeline path (execute_turn → RouteStage) has
        # its own filtering — the two paths are mutually exclusive, not redundant.
        # If tools_schema becomes empty after filtering, the existing
        # `if not tools_schema:` branch above handles it (plain-chat path).
        if _routing.tool_filter == ToolFilter.LOCAL_BLOCKED and tools_schema:
            tools_schema = [
                t for t in tools_schema
                if t.get("function", {}).get("name") not in LOCAL_TOOLS
            ]
            logger.info("Intent router: LOCAL_BLOCKED — filtered to %d tools, max %d rounds",
                        len(tools_schema), _effective_max_rounds)

        # Log unified routing_decision event (replaces the old stream_intent_classification
        # event — contains all three classification dimensions in one event).
        self.event_logger.create_stream_event(
            user_id=user_id,
            session_id=session_id,
            event_type="routing_decision",
            content=json.dumps({
                "intent": _routing.routing_result.intent,
                "confidence": _routing.routing_result.confidence,
                "tier": _routing.routing_result.tier,
                "tool_filter": _routing.tool_filter.value,
                "max_tool_rounds": _routing.max_tool_rounds,
                "task_type": _routing.task_type.value,
                "threshold_used": _routing.threshold_used,
            }),
            parent_event_id=user_event.event_id,
            causal_chain_id=user_event.causal_chain_id,
        )

        for _round in range(_effective_max_rounds):

            # Compact if approaching context limit
            from core.context.compaction import compact, compact_history_messages, needs_compaction
            max_tokens = self.llm.config.get("max_context_tokens", 128000)

            # Lightweight pre-pass: shrink old tool results
            messages = compact_history_messages(messages)

            if isinstance(max_tokens, int) and needs_compaction(messages, max_tokens):
                def llm_summarize(text: str) -> str:
                    try:
                        result = self.llm.chat(
                            messages=[{"role": "user", "content": f"Summarize concisely:\n{text}"}],
                            user_id=user_id,
                            task_hint="compaction",
                        )
                        return result.content or text[:1000]
                    except Exception:
                        return text[:1000]

                messages = compact(messages, max_tokens, llm_summarize=llm_summarize)

            full_text = ""
            tool_calls: list[dict] = []
            reasoning_content_parts: list[str] = []

            # Use model from context if provided, otherwise use SLO escalation
            from core.llm.model_resolver import resolve_model
            model = resolve_model(
                request_model=(context or {}).get("model"),
                slo_escalation_model=self._check_slo_escalation(session_id),
            )

            async for chunk in self.llm.chat_with_tools_stream(
                messages, tools_schema, model=model,
            ):
                if chunk["type"] == "reasoning":
                    reasoning_content_parts.append(chunk["content"])
                    yield StreamEvent(
                        event_type=StreamEventType.REASONING_MESSAGE_CONTENT,
                        data={"content": chunk["content"]},
                        event_id=user_event.event_id,
                        causal_chain_id=user_event.causal_chain_id,
                        agent_id=self.agent_id,
                    )
                elif chunk["type"] == "text":
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
                verification = self.firewall.verify_response(full_text, context_capture_id, mode=self.firewall_mode, skill_name=last_skill_name)
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
                    user_id, session_id, full_text, user_event.event_id, user_event.causal_chain_id,
                    messages=messages, firewall_result=verification,
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
            asst_msg: dict = {"role": "assistant", "content": full_text, "tool_calls": tool_calls}
            if reasoning_content_parts:
                asst_msg["reasoning_content"] = "".join(reasoning_content_parts)
            messages.append(asst_msg)

            # Check for parallel delegation (multiple delegate_task calls)
            delegation_calls = [tc for tc in tool_calls if tc["function"]["name"] == "delegate_task"]

            if len(delegation_calls) > 1:
                last_skill_name = "delegate_task"
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
                                result_text = event.data.get("full_text", "")
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
                            results[tc["id"]] = f"Error: Parallel execution failed - {e!s}"
                finally:
                    pass

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

                # Execute remaining non-delegation tool calls sequentially
                other_calls = [tc for tc in tool_calls if tc["function"]["name"] != "delegate_task"]
                for tc in other_calls:
                    fn_name = tc["function"]["name"]
                    last_skill_name = fn_name
                    async for evt in self._execute_single_tool(
                        tc, fn_name, user_id, session_id, user_input, full_text,
                        user_event, messages,
                    ):
                        yield evt
                        if evt.data.get("wait_for"):
                            return
            else:
                # Sequential tool execution
                for tc in tool_calls:
                    fn_name = tc["function"]["name"]
                    last_skill_name = fn_name
                    async for evt in self._execute_single_tool(
                        tc, fn_name, user_id, session_id, user_input, full_text,
                        user_event, messages,
                    ):
                        yield evt
                        if evt.data.get("wait_for"):
                            return

            # Task 1.2: Check if all tools are blocked after this round
            if self._all_tools_blocked(tools_schema):
                failure_report = self._build_failure_report()
                yield StreamEvent(
                    event_type=StreamEventType.TEXT_DELTA,
                    data={"chunk": failure_report},
                    event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                    agent_id=self.agent_id,
                )
                # Emit TEXT_DONE so run_step() wrapper can capture the content
                yield StreamEvent(
                    event_type=StreamEventType.TEXT_DONE,
                    data={"full_text": failure_report},
                    event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                    agent_id=self.agent_id,
                )
                self._log_response(
                    user_id, session_id, failure_report,
                    user_event.event_id, user_event.causal_chain_id,
                    messages=messages,
                    firewall_result=None,
                )
                yield StreamEvent(
                    event_type=StreamEventType.RUN_FINISHED,
                    data={"failure": "all_tools_blocked"},
                    event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                    agent_id=self.agent_id,
                )
                return

            # Stall detection: if the agent keeps calling the same tools
            # across consecutive rounds, nudge it to stop and answer directly.
            self._record_round_tools(tool_calls, full_text)
            if self._detect_stall():
                logger.info("Stall detected at round %d — nudging LLM to conclude", _round)
                messages.append({
                    "role": "system",
                    "content": (
                        "You have already tried similar tool calls multiple times without "
                        "making progress. Stop calling tools and give the user your best "
                        "answer based on what you have so far. If you could not find what "
                        "the user asked for, say so directly."
                    ),
                })
                # Remove tools for the next LLM call so it must produce text
                tools_schema = []

        # Exhausted rounds — ask LLM for a final answer without tools
        messages.append(
            {
                "role": "system",
                "content": "Please provide your final answer based on the tool results above.",
            }
        )
        from core.verification.streaming_verifier import StreamingVerifier
        sv = StreamingVerifier(firewall=self.firewall, context_capture_id=context_capture_id, llm_client=self.llm)

        async for chunk_msg in self.llm.chat_stream(
            messages, user_id, session_id, model=self._check_slo_escalation(session_id),
        ):
            if chunk_msg["type"] == "reasoning":
                yield StreamEvent(
                    event_type=StreamEventType.REASONING_MESSAGE_CONTENT,
                    data={"content": chunk_msg["content"]},
                    event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                    agent_id=self.agent_id,
                )
                continue
            chunk = chunk_msg["content"]
            warnings = sv.check(chunk)
            yield StreamEvent(
                event_type=StreamEventType.TEXT_DELTA,
                data={"chunk": chunk},
                event_id=user_event.event_id,
                causal_chain_id=user_event.causal_chain_id,
                agent_id=self.agent_id,
            )
            for warning in warnings:
                sv.full_text += warning
                yield StreamEvent(
                    event_type=StreamEventType.TEXT_DELTA,
                    data={"chunk": warning},
                    event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                    agent_id=self.agent_id,
                )

        for warning in sv.flush():
            sv.full_text += warning
            yield StreamEvent(
                event_type=StreamEventType.TEXT_DELTA,
                data={"chunk": warning},
                event_id=user_event.event_id,
                causal_chain_id=user_event.causal_chain_id,
                agent_id=self.agent_id,
            )

        full_text = sv.full_text

        # Verify exhausted-rounds answer with firewall
        verification = self.firewall.verify_response(full_text, context_capture_id, mode=self.firewall_mode, skill_name=last_skill_name)
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
            user_id, session_id, full_text, user_event.event_id, user_event.causal_chain_id,
            messages=messages, firewall_result=verification,
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
        parent_user_event=None,
    ) -> AsyncIterator[StreamEvent]:
        """PAOR: Plan → Act → Observe → Reflect loop.

        For complex tasks that need multi-step planning.
        """
        # ── Audit binding: user event + context snapshot ──────────
        if parent_user_event is not None:
            user_event = parent_user_event
        else:
            user_event = self.event_logger.create_user_query(
                user_id=user_id,
                session_id=session_id,
                content=user_input,
            )
            self.event_logger.flush_critical()  # Must be visible for build_context

        if not context_capture_id:
            ctx = self.context_manager.build_context(
                session_id=session_id, query=user_input,
            )
            context_capture_id = self.context_manager.save_snapshot(
                ctx, session_id, user_event.event_id,
            )
            logger.debug(f"[planning] Context snapshot: {context_capture_id}")

        # ── RUN_STARTED (aligned with stream path) ───────────────
        _event_id = str(user_event.event_id)
        _chain_id = str(user_event.causal_chain_id or "")

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
            event_id=str(run_started_event.event_id),
            causal_chain_id=_chain_id,
            agent_id=self.agent_id,
        )

        _db = self.event_logger._db_factory()
        planner = Planner(
            self.llm,
            event_logger=self.event_logger,
            db=_db,
        )
        constraints = planner.constraints

        # P: Plan — try cross-session restore first, then create new
        try:
            plan = restore_plan_from_events(_db, user_input)
        except Exception as e:
            logger.warning("[planning] Failed to restore plan: %s", e)
            plan = None
        _resumed = plan is not None

        # Skip restore if plan has no pending steps (all completed)
        if _resumed and all(s.status == PlanStatus.COMPLETED for s in plan.steps):
            logger.info("[planning] Plan %s already completed, creating new", plan.plan_id)
            plan = None
            _resumed = False

        if _resumed:
            logger.info("[planning] Resumed plan %s from events", plan.plan_id)
        else:
            plan = await planner.create_plan(
                goal=user_input,
                context=str(context),
                user_id=user_id,
                session_id=session_id,
                parent_event_id=user_event.event_id,
                causal_chain_id=user_event.causal_chain_id,
            )

        # Check constraints
        is_valid, error_msg = planner.check_constraints(plan)
        if not is_valid:
            planner.log_plan_failed(
                plan, user_id, session_id, error_msg or "constraint violation",
                parent_event_id=user_event.event_id,
                causal_chain_id=user_event.causal_chain_id,
            )
            yield StreamEvent(
                event_type=StreamEventType.RUN_ERROR,
                data={"error": error_msg},
            )
            return

        # Log plan created event (stream event for UI)
        self.event_logger.create_stream_event(
            user_id=user_id,
            session_id=session_id,
            event_type="stream_plan_created",
            content=plan.model_dump_json(),
            parent_event_id=user_event.event_id,
            causal_chain_id=user_event.causal_chain_id,
        )

        yield StreamEvent(
            event_type=StreamEventType.PLAN_CREATED,
            data={"plan": plan.model_dump(), "resumed": _resumed},
        )

        for _rev in range(constraints.max_revisions):
            step_results = []

            # A: Act — execute ready steps
            next_steps = planner.get_next_steps(plan)
            if not next_steps:
                # All steps completed
                break

            # Check step count constraint
            if len(plan.steps) > constraints.max_steps:
                planner.log_plan_failed(
                    plan, user_id, session_id,
                    f"Step count {len(plan.steps)} exceeds max {constraints.max_steps}",
                    parent_event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                )
                yield StreamEvent(
                    event_type=StreamEventType.RUN_ERROR,
                    data={
                        "error": f"Step count {len(plan.steps)} exceeds max {constraints.max_steps}"
                    },
                )
                return

            for step in next_steps:
                step.status = "in_progress"  # type: ignore
                planner.log_step_start(
                    step, plan.plan_id, user_id, session_id,
                    parent_event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                )
                yield StreamEvent(
                    event_type=StreamEventType.PLAN_STEP_START,
                    data={"step": step.step_id},
                )

                # Execute step — tool selection handled by LLM native FC
                skill_name = step.skill_hint

                if skill_name:
                    tools_schema = []  # LLM selects tools via native FC
                    selection_event_id = None

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
                        # CoT audit: check for goal hijacking before execution
                        from core.verification.cot_audit import audit_tool_call
                        # Use structured skill_params if available, fallback to description
                        exec_params = step.skill_params if isinstance(step.skill_params, dict) else {"input": step.description}
                        audit = audit_tool_call(
                            user_query=user_input,
                            tool_name=skill_name,
                            tool_args=exec_params,
                            assistant_reasoning=step.description,
                            llm_client=self.llm,
                        )
                        if not audit.safe:
                            logger.warning("CoT audit blocked planning skill %s: %s", skill_name, audit.reason)
                            result = f"Blocked by CoT audit: {audit.reason}"
                            step.status = "blocked"  # type: ignore
                        # HITL policy check
                        elif not (hitl_ret := self._evaluate_hitl(skill_name, exec_params))[0]:
                            result = hitl_ret[1]
                            step.status = "blocked"  # type: ignore
                        # Execute skill with automatic feedback recording
                        elif self.mcp_bridge and self.mcp_bridge.is_mcp_tool(skill_name):
                            result = await self.mcp_bridge.call_tool(
                                skill_name, exec_params,
                            )
                        else:
                            result = self.executor.execute_skill_with_feedback(
                                skill_name=skill_name,
                                params=exec_params,
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

                # Record HITL outcome for adaptive supervision decay
                if skill_name and self.hitl_policy and step.status != "blocked":
                    self.hitl_policy.record_outcome(skill_name, success=True)

                step.status = step.status if step.status == "blocked" else "completed"  # type: ignore
                step.result = str(result)
                step_results.append({"step_id": step.step_id, "result": result})

                planner.log_step_done(
                    step, plan.plan_id, user_id, session_id,
                    parent_event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                )
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
                planner.log_plan_revised(
                    revised_plan, user_id, session_id,
                    parent_event_id=user_event.event_id,
                    causal_chain_id=user_event.causal_chain_id,
                )
                plan = revised_plan
                yield StreamEvent(
                    event_type=StreamEventType.PLAN_REVISED,
                    data={"plan": plan.model_dump()},
                )

        # Log plan completion
        planner.log_plan_completed(
            plan, user_id, session_id,
            summary=f"Completed {sum(1 for s in plan.steps if s.status == PlanStatus.COMPLETED)}/{len(plan.steps)} steps",
            parent_event_id=user_event.event_id,
            causal_chain_id=user_event.causal_chain_id,
        )

        # Final synthesis — LLM summarises step results, then firewall verifies
        results_summary = "\n".join(
            f"- Step {r['step_id']}: {str(r['result'])[:500]}" for r in step_results
        )
        synth_messages = [
            {"role": "system", "content": "Summarise the results of the executed plan steps into a coherent answer for the user."},
            {"role": "user", "content": f"Original request: {user_input}\n\nPlan results:\n{results_summary}"},
        ]
        from core.llm.model_resolver import resolve_model
        model = resolve_model(
            request_model=(context or {}).get("model"),
            slo_escalation_model=self._check_slo_escalation(session_id),
        )
        final_text = ""
        async for chunk_msg in self.llm.chat_stream(synth_messages, user_id, session_id, model=model):
            if chunk_msg["type"] == "reasoning":
                continue
            chunk = chunk_msg["content"]
            final_text += chunk
            yield StreamEvent(
                event_type=StreamEventType.TEXT_DELTA,
                data={"chunk": chunk},
                event_id=_event_id,
                causal_chain_id=_chain_id,
                agent_id=self.agent_id,
            )

        verification = self.firewall.verify_response(final_text, context_capture_id, mode=self.firewall_mode)
        self.firewall.log_verification(
            session_id, user_event.event_id, verification, context_capture_id,
        )
        if not verification.safe_to_deliver:
            logger.warning(
                "[planning] Firewall: confidence=%.2f, failed=%s",
                verification.confidence_score, verification.claims_failed,
            )
            warning = (
                f"\n\n⚠️ Warning: Low confidence ({verification.confidence_score:.0%}). "
                f"{verification.claims_failed} unverified claims."
            )
            yield StreamEvent(
                event_type=StreamEventType.TEXT_DELTA,
                data={"chunk": warning},
                event_id=_event_id,
                causal_chain_id=_chain_id,
                agent_id=self.agent_id,
            )
            final_text += warning

        self._log_response(
            user_id, session_id, final_text,
            user_event.event_id, user_event.causal_chain_id,
            firewall_result=verification,
        )

        yield StreamEvent(
            event_type=StreamEventType.RUN_FINISHED,
            data={"context_capture_id": context_capture_id},
            event_id=_event_id,
            causal_chain_id=_chain_id,
            agent_id=self.agent_id,
        )

    # ------------------------------------------------------------------
    # Pipeline integration (Task 0.4)
    # ------------------------------------------------------------------

    def _make_llm_call(self, model: str | None = None):
        """Create an async LLM call adapter for the pipeline engine."""
        async def llm_call(messages, tools, **kw):
            from core.context.compaction import compact, compact_history_messages, needs_compaction
            max_tokens = self.llm.config.get("max_context_tokens", 128000)

            def _summarize(text: str) -> str:
                try:
                    result = self.llm.chat(
                        messages=[{"role": "user", "content": f"Summarize concisely:\n{text}"}],
                        user_id="pipeline",
                        task_hint="compaction",
                    )
                    return result.content or text[:1000]
                except Exception:
                    return text[:1000]

            messages = compact_history_messages(messages)
            if isinstance(max_tokens, int) and needs_compaction(messages, max_tokens):
                messages = compact(messages, max_tokens, llm_summarize=_summarize)

            if not tools:
                response = self.llm.chat(
                    messages=[LLMMessage(role=m["role"], content=m.get("content", "")) for m in messages],
                    user_id="pipeline",
                    model=model,
                )
                return {"content": response.content or "", "tool_calls": []}

            try:
                return self.llm.chat_with_tools(
                    messages=messages, tools=tools, tool_choice="auto", model=model,
                )
            except Exception as e:
                if "context length" in str(e).lower() or "token" in str(e).lower():
                    logger.warning("Context length exceeded, forcing compaction")
                    messages = compact(messages, max_tokens // 2, llm_summarize=_summarize)
                    return self.llm.chat_with_tools(
                        messages=messages, tools=tools, tool_choice="auto", model=model,
                    )
                raise
        return llm_call

    def _make_tool_execute(self, session_id: str, user_id: str, user_input: str, user_event):
        """Create an async tool execute adapter for the pipeline engine."""
        async def tool_execute(fn_name: str, params: dict, **kw):
            from core.verification.cot_audit import audit_tool_call
            audit = audit_tool_call(
                user_query=user_input,
                tool_name=fn_name,
                tool_args=params,
                assistant_reasoning="",
                llm_client=self.llm,
            )
            if not audit.safe:
                raise RuntimeError(f"Blocked by CoT audit: {audit.reason}")

            hitl_ok, hitl_msg = self._evaluate_hitl(fn_name, params)
            if not hitl_ok:
                raise RuntimeError(hitl_msg)

            from core.agent.async_tools import get_async_tool_registry
            _async_registry = get_async_tool_registry()

            if _async_registry.is_async_tool(fn_name):
                return await _async_registry.execute(fn_name, params, run_id=getattr(self, '_current_run_id', None))
            elif fn_name.startswith("scratchpad_") and self.scratchpad:
                return self._handle_scratchpad_tool(fn_name, params, session_id, user_id)
            elif self.mcp_bridge and self.mcp_bridge.is_mcp_tool(fn_name):
                return await asyncio.wait_for(
                    self.mcp_bridge.call_tool(fn_name, params), timeout=TOOL_TIMEOUT_SECONDS,
                )
            else:
                return await asyncio.wait_for(
                    asyncio.to_thread(
                        self.executor.execute_skill_with_feedback,
                        skill_name=fn_name,
                        params=params,
                        session_id=session_id,
                        parent_event_id=user_event.event_id,
                        selection_event_id=None,
                    ),
                    timeout=TOOL_TIMEOUT_SECONDS,
                )
        return tool_execute

    async def _execute_turn_pipeline(
        self,
        state,
        model: str | None = None,
        classify_intent=None,
    ):
        """Bridge between ChatLoop and the pipeline engine.

        Yields TurnEvent objects from the pipeline.
        """
        from core.agent.pipeline_stages import execute_turn

        async def final_answer_call(messages, **kw):
            response = self.llm.chat(
                messages=[LLMMessage(role=m["role"], content=m.get("content", "")) for m in messages],
                user_id=state.user_id,
                model=model,
            )
            return response.content or ""

        async for event in execute_turn(
            state,
            llm_call=self._make_llm_call(model),
            tool_execute=self._make_tool_execute(
                state.session_id, state.user_id, state.user_input, state.user_event,
            ),
            classify_intent=classify_intent,
            final_answer_call=final_answer_call,
        ):
            yield event

    # ------------------------------------------------------------------
    # ------------------------------------------------------------------
    # Circuit breaker helpers (Task 1.1)
    # ------------------------------------------------------------------

    def _reset_breaker(self) -> None:
        """Reset per-turn breaker state and load persisted state (1 SELECT).

        Cross-turn failure accumulation: persisted consecutive_failures are seeded
        into _tool_failures so _should_break() considers failures from prior turns.
        Example: 2 failures in turn 1 + 1 failure in turn 2 = 3 total → breaker trips.
        """
        self._tool_failures = {}
        self._blocked_tools = set()
        self._round_tool_sigs = []
        self._round_text_lens = []
        self._breaker_records: dict[str, Any] = {}  # Actually dict[str, BreakerRecord]
        try:
            if hasattr(self.event_logger, '_db_factory'):
                from core.agent.breaker_store import load_breaker_state
                db = self.event_logger._db_factory()
                try:
                    user_id = getattr(self, '_current_user_id', None)
                    if user_id:
                        self._breaker_records = load_breaker_state(db, user_id)
                        for name, rec in self._breaker_records.items():
                            if rec.in_cooldown:
                                self._blocked_tools.add(name)
                                logger.info("Breaker cooldown active for %s until %s", name, rec.cooldown_until)
                            # Seed in-memory failures from persisted count so cross-turn
                            # accumulation works: _should_break sees prior failures.
                            # Use unique placeholders to avoid false "similar error" matches —
                            # only the "3 any failures" threshold should consider these.
                            if rec.consecutive_failures > 0:
                                self._tool_failures[name] = [
                                    f"[prior-turn-failure-{i + 1}-{name}]"
                                    for i in range(rec.consecutive_failures)
                                ]
                finally:
                    db.close()
        except Exception as e:
            logger.debug("Breaker state load failed (non-fatal): %s", e)

    def _flush_breaker(self) -> None:
        """Persist dirty breaker records at turn end (1 batch transaction)."""
        try:
            records = getattr(self, '_breaker_records', {})
            if not any(r.dirty for r in records.values()):
                return
            if hasattr(self.event_logger, '_db_factory'):
                from core.agent.breaker_store import flush_breaker_state
                db = self.event_logger._db_factory()
                try:
                    flush_breaker_state(db, records)
                finally:
                    db.close()
        except Exception as e:
            logger.debug("Breaker flush failed (non-fatal): %s", e)

    def _is_tool_blocked(self, fn_name: str) -> bool:
        return fn_name in getattr(self, '_blocked_tools', set())

    def _record_tool_failure(self, fn_name: str, error_msg: str) -> None:
        """Record a tool failure and check breaker. In-memory only — flushed at turn end.

        Every failure is persisted (not just breaker-tripping ones) so cross-turn
        accumulation works: 2 failures in turn 1 + 1 in turn 2 = 3 total → trips.
        """
        self._tool_failures.setdefault(fn_name, []).append(error_msg)
        # Always persist failure count — not just on breaker trip
        records = getattr(self, '_breaker_records', {})
        if fn_name not in records:
            from core.agent.breaker_store import BreakerRecord
            records[fn_name] = BreakerRecord(
                user_id=getattr(self, '_current_user_id', "") or "",
                tool_name=fn_name,
            )
        records[fn_name].record_failure()
        from core.agent.pipeline_stages import _should_break
        if _should_break(self._tool_failures[fn_name]):
            self._blocked_tools.add(fn_name)
            logger.warning("Circuit breaker tripped for tool %s", fn_name)

    def _record_tool_success(self, fn_name: str) -> None:
        """Clear failure history on success. In-memory only — flushed at turn end."""
        self._tool_failures.pop(fn_name, None)
        records = getattr(self, '_breaker_records', {})
        if fn_name in records:
            records[fn_name].record_success()

    def _all_tools_blocked(self, tools_schema: list[dict]) -> bool:
        """Check if all available tools are blocked."""
        active = [
            t for t in tools_schema
            if t.get("function", {}).get("name") not in getattr(self, '_blocked_tools', set())
        ]
        return len(active) == 0 and len(tools_schema) > 0

    def _build_failure_report(self) -> str:
        """Build user-facing failure report from breaker state."""
        lines = ["I was unable to complete the task. The following tools encountered repeated errors:"]
        for tool, errors in getattr(self, '_tool_failures', {}).items():
            if tool in getattr(self, '_blocked_tools', set()):
                last_err = errors[-1] if errors else "unknown error"
                lines.append(f"- **{tool}**: {last_err}")
        lines.append("\nPlease check the tool configuration or try a different approach.")
        return "\n".join(lines)

    # Stall detection
    # ------------------------------------------------------------------
    # Detects when the agent repeatedly calls the same tool(s) with the
    # same arguments across consecutive rounds — a sign it's stuck in a
    # search loop that won't converge.  We compare *normalised call
    # signatures* (tool name + sorted argument keys+values) so that
    # genuinely different queries (e.g. grep "foo" vs grep "bar") are
    # not flagged, while identical retries are.

    _STALL_WINDOW = 3  # consecutive rounds to compare
    _NO_PROGRESS_WINDOW = 3  # rounds with no meaningful text → stall
    _MIN_PROGRESS_CHARS = 20  # text shorter than this counts as "no progress"

    def _record_round_tools(self, tool_calls: list[dict], text: str = "") -> None:
        """Record normalised call signatures and text length for this round."""
        sigs: set[str] = set()
        for tc in tool_calls:
            name = tc["function"]["name"]
            args = tc["function"].get("arguments", "")
            sigs.add(f"{name}:{args}")
        self._round_tool_sigs.append(sigs)
        self._round_text_lens.append(len(text.strip()))

    def _detect_stall(self) -> bool:
        """Return True if the agent is stuck.

        Two complementary heuristics:
        1. Identical call signatures for ``_STALL_WINDOW`` consecutive rounds.
        2. No meaningful text output (< ``_MIN_PROGRESS_CHARS``) for
           ``_NO_PROGRESS_WINDOW`` consecutive rounds — catches "different tool
           each round" thrashing.
        """
        sigs = self._round_tool_sigs
        window = self._STALL_WINDOW
        if len(sigs) >= window:
            recent = sigs[-window:]
            if all(s == recent[0] for s in recent[1:]):
                return True

        lens = self._round_text_lens
        np_window = self._NO_PROGRESS_WINDOW
        if len(lens) >= np_window:
            threshold = self._MIN_PROGRESS_CHARS
            if all(length < threshold for length in lens[-np_window:]):
                return True

        return False

    # Helpers
    # ------------------------------------------------------------------

    def _handle_scratchpad_tool(
        self, fn_name: str, params: dict, session_id: str, user_id: str,
    ) -> dict:
        """Execute a scratchpad tool call locally."""
        if fn_name == "scratchpad_write":
            note_id = self.scratchpad.create_note(
                session_id=session_id,
                user_id=user_id,
                note_type=params["note_type"],
                content=params["content"],
                agent_id=self.agent_id,
            )
            return {"note_id": note_id, "status": "created"}

        if fn_name == "scratchpad_read":
            notes = self.scratchpad.get_active_notes(
                session_id, note_type=params.get("note_type"),
            )
            return {"notes": notes}

        if fn_name == "scratchpad_close":
            ok = self.scratchpad.close_note(
                params["note_id"], status=params.get("status", "completed"),
            )
            return {"success": ok}

        return {"error": f"Unknown scratchpad tool: {fn_name}"}

    def _build_messages(
        self, user_input: str, context: dict[str, Any] | None,
        session_id: str | None = None,
        user_id: str | None = None,
    ) -> list[dict[str, Any]]:
        """Build messages with structured prompt composition.

        Layout (cache-friendly — stable prefix, dynamic suffix):
          [STABLE]  §1 Role & capabilities  (cacheable across turns)
          [STABLE]  §2 Constraints & format
          [DYNAMIC] §3 Observations / prior context
          [DYNAMIC] §4 Working memory (scratchpad)
          [DYNAMIC] §5 Conversation history (budget-capped)
        """
        # §1 Role — from context (set by ContextManager or caller), fallback to default
        role = "You are a development assistant. Use the available tools to help the user."
        sp = context.get("system_prompt") if context else None
        if isinstance(sp, str) and sp:
            role = sp

        # §2 Constraints (stable, always present)
        constraints = (
            "Rules:\n"
            "- Think step-by-step before acting\n"
            "- Verify changes before presenting\n"
            "- If uncertain, say so rather than guess\n"
            "- Prefer using tools over generating untested answers"
        )

        # Stable prefix (§1 + §2) — benefits from prompt caching
        sections = [role, constraints]

        # §2.5 Dynamic few-shot examples (from high-rated feedback)
        if hasattr(self, '_few_shot') and self._few_shot:
            examples = self._few_shot.retrieve(user_input)
            few_shot_section = self._few_shot.format_for_prompt(examples)
            if few_shot_section:
                sections.append(few_shot_section)

        # §3 Working memory (changes within session)
        if self.scratchpad and session_id:
            notes = self.scratchpad.get_active_notes(session_id)
            if notes:
                note_lines = [f"[{n['note_type']}] {n['content']}" for n in notes]
                sections.append(
                    "Working memory (your active notes):\n" + "\n---\n".join(note_lines)
                )

        # §5 Conversation history (changes every turn, budget-capped)
        selected = context.get("selected_events") if context else None
        if selected and isinstance(selected, list):
            # Budget: ~2000 chars for history (roughly 500 tokens)
            tb = context.get("token_budget") if isinstance(context.get("token_budget"), dict) else {}
            allocated = tb.get("history", {}).get("allocated", 500) if isinstance(tb.get("history"), dict) else 500
            budget = (allocated if isinstance(allocated, (int, float)) else 500) * 4
            history_lines = []
            used = 0
            for ev in selected:
                role_label = "User" if ev.get("event_type") == "user_query" else "Agent"
                line = f"{role_label}: {ev.get('content', '')}"
                line_len = len(line)
                if used + line_len > budget and history_lines:
                    break
                history_lines.append(line)
                used += line_len
            if history_lines:
                sections.append("Recent conversation:\n" + "\n".join(history_lines))

        # §6 Code context — retrieved by ContextManager's hybrid search
        # Budget is already enforced by ContextManager.build_context() which
        # respects token_budget["code"]["allocated"]. We apply a safety cap
        # here to guard against misconfigured or test contexts.
        code_ctx = context.get("code_context") if context else None
        if code_ctx and isinstance(code_ctx, list):
            code_lines = []
            code_used = 0
            code_budget = 8000  # ~2000 tokens safety cap
            for item in code_ctx:
                if isinstance(item, dict):
                    path = item.get("path", "unknown")
                    snippet = item.get("content", item.get("snippet", ""))
                    entry = f"### {path}\n```\n{snippet}\n```"
                else:
                    entry = str(item)
                if code_used + len(entry) > code_budget and code_lines:
                    break
                code_lines.append(entry)
                code_used += len(entry)
            if code_lines:
                sections.append("Relevant code:\n" + "\n\n".join(code_lines))

        # §7 Documentation — retrieved by ContextManager
        # Same safety cap as §6.
        doc_ctx = context.get("documentation") if context else None
        if doc_ctx and isinstance(doc_ctx, list):
            doc_lines = []
            doc_used = 0
            doc_budget = 4000  # ~1000 tokens safety cap
            for doc in doc_ctx:
                if isinstance(doc, dict):
                    title = doc.get("title", "")
                    body = doc.get("content", doc.get("body", ""))
                    entry = f"**{title}**\n{body}" if title else body
                else:
                    entry = str(doc)
                if doc_used + len(entry) > doc_budget and doc_lines:
                    break
                doc_lines.append(entry)
                doc_used += len(entry)
            if doc_lines:
                sections.append("Relevant documentation:\n" + "\n\n---\n\n".join(doc_lines))

        return [
            {"role": "system", "content": "\n\n".join(sections)},
            {"role": "user", "content": user_input},
        ]

    def set_observer(self, observer) -> None:
        """Attach an Observer for post-turn observation extraction."""
        self.observer = observer

    def set_mcp_bridge(self, bridge) -> None:
        """Attach an MCPBridge for external MCP server tool access.

        Registers a callback so that when MCP tools change (connect/refresh/close),
        the selector's skill list is updated with MCP tool metadata.
        """
        self.mcp_bridge = bridge
        if bridge is not None:
            bridge.set_on_tools_changed(self._sync_mcp_tools)
            self._sync_mcp_tools()

    def _sync_mcp_tools(self) -> None:
        """Sync MCP tool metadata — no-op after skill system cleanup."""
        pass

    def _evaluate_hitl(self, fn_name: str, params: dict, **ctx_overrides) -> tuple[bool, str | None]:
        """Check HITL policy before tool execution.

        Returns (allowed, block_message).
        - allowed=True  → proceed (NONE or OBSERVE_ONLY)
        - allowed=False → block with message (APPROVE_REJECT / REVIEW_AND_EDIT / TAKEOVER)
        """
        if not self.hitl_policy:
            return True, None
        from core.verification.hitl_policy import ActionContext, SupervisionAction
        # Auto-detect novel skill: never seen in success streak
        is_novel = fn_name not in self.hitl_policy._success_streak
        ctx = ActionContext(
            skill_name=fn_name,
            agent_id=self.agent_id,
            is_novel_skill=ctx_overrides.pop("is_novel_skill", is_novel),
            **ctx_overrides,
        )
        decision = self.hitl_policy.evaluate(ctx)
        if decision.action in (SupervisionAction.NONE, SupervisionAction.OBSERVE_ONLY):
            if decision.action == SupervisionAction.OBSERVE_ONLY:
                logger.info("[HITL] observe_only for %s: %s", fn_name, decision.reason)
            return True, None
        logger.warning("[HITL] %s blocked %s: %s", decision.action.value, fn_name, decision.reason)
        return False, json.dumps({
            "error": f"Blocked by HITL policy ({decision.action.value}): {decision.reason}",
            "hitl_action": decision.action.value,
            "triggered_policies": decision.triggered_policies,
        })

    def _log_response(
        self,
        user_id: str,
        session_id: str,
        content: str,
        parent_event_id: str,
        causal_chain_id: str | None,
        messages: list[dict[str, Any]] | None = None,
        firewall_result: Any | None = None,
    ) -> None:
        """Log the final agent response as an event, then run Observer."""
        event = self.event_logger.create_llm_response(
            user_id=user_id,
            session_id=session_id,
            content=content,
            agent_id="dev-agent",
            agent_version="0.1.0",
            parent_event_id=parent_event_id,
            causal_chain_id=causal_chain_id or "",
            llm_model_used=self.llm.config.get("model", "unknown"),
        )
        # Auto-score: fill quality_score + training_eligible
        if firewall_result is not None:
            try:
                from core.evaluation.auto_scorer import compute_auto_score

                response_tokens = len(content.split()) if content else 0
                result = compute_auto_score(
                    firewall_passed=firewall_result.safe_to_deliver,
                    firewall_confidence=firewall_result.confidence_score,
                    response_tokens=response_tokens,
                )
                self.event_logger.update_quality_score(
                    event.event_id, result.quality_score, result.training_eligible,
                )
            except Exception as e:
                logger.warning("Auto-score failed (non-fatal): %s", e)
        # Chain-level quality aggregation (non-fatal)
        if causal_chain_id:
            try:
                from core.evaluation.multi_level_scorer import score_chain
                with self.event_logger._db() as _score_db:
                    score_chain(_score_db, causal_chain_id, session_id)
            except Exception as e:
                logger.warning("Chain-level scoring failed (non-fatal): %s", e)
        # Task 4.2: Log route_feedback event for learning (non-fatal)
        try:
            blocked = getattr(self, '_blocked_tools', set())
            failures = getattr(self, '_tool_failures', {})
            if blocked or failures:
                self.event_logger.create_stream_event(
                    user_id=user_id,
                    session_id=session_id,
                    event_type="stream_run_finished",
                    content=json.dumps({
                        "route_feedback": True,
                        "blocked_tools": sorted(blocked),
                        "tool_failures": {k: len(v) for k, v in failures.items()},
                    }),
                    parent_event_id=parent_event_id,
                    causal_chain_id=causal_chain_id,
                )
        except Exception:
            pass
        # Task 4.3: Flush dirty breaker records (1 batch transaction per turn)
        self._flush_breaker()
        # Post-turn: run Observer via TurnHooks (shared with /chat/turn)
        if messages:
            from api.database import SessionLocal
            from core.agent.turn_hooks import TurnHooks
            hooks = TurnHooks(SessionLocal, llm_client=self.llm, embed_fn=self._get_embed_fn())
            hooks.run_observer(session_id, user_id, messages)

    def _get_embed_fn(self):
        """Lazy-init embedding function for memory pipeline."""
        try:
            from core.context.embeddings import get_embedding_client
            return get_embedding_client().embed
        except Exception:
            return None

    def _check_slo_escalation(self, session_id: str) -> str | None:
        """Return escalated model name if a recent SLO escalation event exists."""
        if self._escalated_model is not _UNSET:
            return self._escalated_model
        try:
            from sqlalchemy import text
            with self.event_logger._db() as _slo_db:
                row = _slo_db.execute(text("""
                    SELECT 1 FROM agent_events
                    WHERE agent_id = :aid AND event_type = 'slo_model_escalation'
                      AND created_at > DATE_SUB(NOW(), INTERVAL 24 HOUR)
                    LIMIT 1
                """), {"aid": self.agent_id}).fetchone()
            if row and hasattr(self.llm, 'router'):
                current = self.llm.config.get("model", "gpt-4o-mini")
                escalated = self.llm.router.escalate(current)
                if escalated:
                    self._escalated_model = escalated
                    logger.info("SLO escalation: %s → %s", current, escalated)
                    return escalated
        except Exception as e:
            logger.debug("SLO escalation check failed: %s", e)
        self._escalated_model = None
        return None
