"""Chat loop with multi-turn tool use and full message chain."""

import json
from typing import Dict, Any, Optional, List

from core.llm.models import LLMMessage
from core.events.event_logger import EventLogger
from core.agent.selector import AgentSkillSelector
from core.agent.executor import AgentExecutor
from core.logging_config import get_logger

logger = get_logger(__name__)

MAX_TOOL_ROUNDS = 10


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
    ):
        self.selector = selector
        self.executor = executor
        self.llm = llm_client
        self.event_logger = event_logger

    async def run_step(
        self,
        user_input: str,
        session_id: str,
        user_id: str,
        context: Optional[Dict[str, Any]] = None,
        max_candidates: int = 5,
    ) -> str:
        """Run a full conversation step with multi-turn tool use.

        The LLM can call tools multiple times before producing a final answer.
        The complete message chain is preserved so the LLM retains its
        chain-of-thought across tool calls.
        """
        # 1. Log user query event
        user_event = self.event_logger.create_user_query(
            user_id=user_id,
            session_id=session_id,
            content=user_input,
        )

        # 2. Build messages with context
        messages = self._build_messages(user_input, context)

        # 3. Get available tools schema
        tools_schema = self.selector.selector.get_tools_schema(
            query=user_input, max_candidates=max_candidates
        )

        if not tools_schema:
            # No tools available — plain chat
            response = self.llm.chat(
                messages=[LLMMessage(role=m["role"], content=m.get("content", "")) for m in messages],
                user_id=user_id,
                session_id=session_id,
            )
            self._log_response(user_id, session_id, response.content,
                               user_event.event_id, user_event.causal_chain_id)
            return response.content

        # 4. Multi-turn tool use loop
        for _round in range(MAX_TOOL_ROUNDS):
            llm_result = self.llm.chat_with_tools(
                messages=messages,
                tools=tools_schema,
                tool_choice="auto",
            )

            tool_calls = llm_result.get("tool_calls", [])

            if not tool_calls:
                # LLM produced a final text answer — done
                final_content = llm_result.get("content", "")
                self._log_response(user_id, session_id, final_content,
                                   user_event.event_id, user_event.causal_chain_id)
                return final_content

            # Append the assistant message (with tool_calls) to the chain
            assistant_msg: Dict[str, Any] = {"role": "assistant", "content": llm_result.get("content") or ""}
            assistant_msg["tool_calls"] = tool_calls
            messages.append(assistant_msg)

            # Execute each tool and append results
            for tc in tool_calls:
                fn_name = tc["function"]["name"]
                raw_args = tc["function"]["arguments"]
                tc_id = tc.get("id", fn_name)

                params = json.loads(raw_args) if isinstance(raw_args, str) else raw_args

                try:
                    result = self.executor.execute_skill(
                        skill_name=fn_name,
                        params=params,
                        session_id=session_id,
                        parent_event_id=user_event.event_id,
                    )
                    result_str = json.dumps(result, default=str) if not isinstance(result, str) else result
                except Exception as e:
                    logger.error(f"Skill {fn_name} failed: {e}")
                    result_str = json.dumps({"error": str(e)})

                # Append tool result in OpenAI protocol format
                messages.append({
                    "role": "tool",
                    "tool_call_id": tc_id,
                    "content": result_str,
                })

        # Exhausted rounds — ask LLM for a final answer without tools
        messages.append({
            "role": "system",
            "content": "Please provide your final answer based on the tool results above.",
        })
        response = self.llm.chat(
            messages=[LLMMessage(role=m["role"], content=m.get("content", "")) for m in messages],
            user_id=user_id,
            session_id=session_id,
        )
        self._log_response(user_id, session_id, response.content,
                           user_event.event_id, user_event.causal_chain_id)
        return response.content

    # ------------------------------------------------------------------
    # Helpers
    # ------------------------------------------------------------------

    def _build_messages(self, user_input: str, context: Optional[Dict[str, Any]]) -> List[Dict[str, Any]]:
        """Build the initial messages list, injecting context if available."""
        messages: List[Dict[str, Any]] = []

        system_parts = ["You are a development assistant. Use the available tools to help the user."]

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

    def _log_response(self, user_id: str, session_id: str, content: str,
                      parent_event_id: str, causal_chain_id: str) -> None:
        """Log the final agent response as an event."""
        self.event_logger.create_llm_response(
            user_id=user_id,
            session_id=session_id,
            content=content,
            agent_id="dev-agent",
            agent_version="0.1.0",
            parent_event_id=parent_event_id,
            causal_chain_id=causal_chain_id,
            llm_model_used=self.llm.config.get("model", "unknown"),
        )
