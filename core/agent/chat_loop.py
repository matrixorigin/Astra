"""Interactive chat loop for the agent."""

import asyncio
import json
from typing import Dict, Any, List, Optional
from core.agent.selector import AgentSkillSelector
from core.agent.executor import AgentExecutor
from core.logging_config import get_logger
from core.llm.models import LLMMessage
from core.events.event_logger import EventLogger
from core.events.models import EventType

logger = get_logger(__name__)

class ChatLoop:
    """Manages the conversation loop: User Input -> Select -> Execute -> Response."""

    def __init__(
        self, 
        selector: AgentSkillSelector, 
        executor: AgentExecutor,
        llm_client,
        event_logger: EventLogger
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
        max_candidates: int = 5
    ) -> str:
        """Run a single step of the chat loop.
        
        1. Log User Query
        2. Select skills based on user input.
        3. Execute selected skills.
        4. Generate final response (optional, or just return results).
        """
        logger.info(f"Processing user input: {user_input}")

        # 1. Log User Query Event
        user_event = self.event_logger.create_user_query(
            user_id=user_id,
            session_id=session_id,
            content=user_input
        )
        parent_event_id = user_event.event_id

        # 2. Select Skills
        tool_calls = self.selector.select_skills(
            query=user_input, 
            context=context,
            max_candidates=max_candidates
        )
        
        if not tool_calls:
            logger.info("No skills selected, falling back to chat.")
            messages = [LLMMessage(role="user", content=user_input)]
            response = self.llm.chat(
                messages=messages,
                user_id=user_id,
                session_id=session_id,
                event_id=None,  # Auto-generated
            )
            return response.content

        results = []
        
        # 3. Execute Skills
        for tool_call in tool_calls:
            function_name = tool_call['function']['name']
            arguments = tool_call['function']['arguments']
            
            # Arguments might be a JSON string
            if isinstance(arguments, str):
                try:
                    params = json.loads(arguments)
                except json.JSONDecodeError as e:
                    logger.error(f"Failed to parse JSON arguments for {function_name}: {arguments}")
                    results.append(f"Error: Invalid JSON arguments for {function_name}")
                    continue
            else:
                params = arguments

            try:
                # Log Tool Call Event (optional, but good for trace)
                # For now we rely on executor logs or add explicit event logging here if needed
                
                result = self.executor.execute_skill(
                    skill_name=function_name,
                    params=params,
                    session_id=session_id,
                    parent_event_id=parent_event_id
                )
                results.append(f"Result from {function_name}: {result}")
            except Exception as e:
                results.append(f"Error executing {function_name}: {str(e)}")

        # 4. Generate Final Response
        context_msg = "\n".join(results)
        messages = [
            LLMMessage(role="user", content=user_input),
            LLMMessage(role="system", content=f"Tool execution results:\n{context_msg}\n\nPlease formulate a response to the user.")
        ]
        
        response = self.llm.chat(
            messages=messages,
            user_id=user_id,
            session_id=session_id,
            event_id=None,  # Auto-generated
        )
        
        # Log Agent Response Event
        agent_event_id = self.event_logger.log_event(
            self.event_logger.create_llm_response(
                user_id=user_id,
                session_id=session_id,
                content=response.content,
                agent_id="dev-agent",
                agent_version="0.1.0",
                parent_event_id=parent_event_id,
                causal_chain_id=user_event.causal_chain_id,
                llm_model_used=self.llm.config.get("model", "unknown"),
            )
        )
        logger.debug(f"Logged agent response event: {agent_event_id}")

        return response.content
